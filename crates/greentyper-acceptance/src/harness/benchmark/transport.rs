use super::*;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const TRANSPORT_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/transport/v1/loopback-sse.json"
));
const SYNTHETIC_AUTHORIZATION: &str = "Bearer greentyper-synthetic-token-v1";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_SERVER_CONNECTIONS: u64 = 16;
const MAX_SSE_BYTES: usize = 64 * 1024;
const MAX_SSE_LINE_BYTES: usize = 8 * 1024;
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(2);
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CLIENT_DEADLINE: Duration = Duration::from_secs(2);

pub(super) fn catalog_entry() -> serde_json::Value {
    let mut implementations = Vec::new();
    #[cfg(all(windows, feature = "bench-transport-winhttp"))]
    implementations.push("winhttp-wrest");
    #[cfg(feature = "bench-transport-reqwest")]
    implementations.push("reqwest-rustls");
    serde_json::json!({
        "id": "transport",
        "version": 1,
        "implementations": implementations,
        "workloads": [{"id": "loopback-sse", "version": 1}],
        "purpose": "candidate evidence; not a transport selection"
    })
}

pub(super) fn target(implementation: &str, workload: &str) -> AppResult<Box<dyn BenchmarkTarget>> {
    if workload != "loopback-sse" {
        return Err(cli_error(format!(
            "benchmark workload transport/{workload} is not compiled into this runner"
        )));
    }
    let engine = match implementation {
        #[cfg(feature = "bench-transport-reqwest")]
        "reqwest-rustls" => TransportEngine::ReqwestRustls,
        #[cfg(all(windows, feature = "bench-transport-winhttp"))]
        "winhttp-wrest" => TransportEngine::WinHttpWrest,
        _ => {
            return Err(cli_error(format!(
                "benchmark implementation transport/{implementation} is not compiled into this runner"
            )));
        }
    };
    let fixture: TransportFixture = serde_json::from_str(TRANSPORT_FIXTURE_JSON)?;
    let scenario = validate_fixture(&fixture)?;
    Ok(Box::new(TransportTarget {
        engine,
        fixture,
        scenario,
        origin: None,
        proxy: None,
    }))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct TransportFixture {
    schema_version: u16,
    comparison_id: String,
    workload_id: String,
    workload_version: u16,
    request_count: u16,
    request_timeout_ms: u64,
    slow_response_delay_ms: u64,
    success_fragments_hex: Vec<String>,
    cancel_fragments_hex: Vec<String>,
    expected_events: Vec<SseEvent>,
    expected_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct SseEvent {
    event: String,
    data: String,
}

#[derive(Clone)]
struct ServerScenario {
    success_fragments: Vec<Vec<u8>>,
    cancel_fragments: [Vec<u8>; 2],
    slow_response_delay: Duration,
}

impl ServerScenario {
    fn success_body_len(&self) -> usize {
        self.success_fragments.iter().map(Vec::len).sum()
    }
}

fn validate_fixture(fixture: &TransportFixture) -> AppResult<ServerScenario> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    let expected_events = vec![
        SseEvent {
            event: "message".into(),
            data: "alpha".into(),
        },
        SseEvent {
            event: "delta".into(),
            data: "中 euro €\nline two".into(),
        },
        SseEvent {
            event: "done".into(),
            data: "[DONE]".into(),
        },
    ];
    if fixture.comparison_id != "transport"
        || fixture.workload_id != "loopback-sse"
        || fixture.workload_version != 1
        || fixture.request_count != 7
        || fixture.request_timeout_ms != 40
        || fixture.slow_response_delay_ms != 120
        || fixture.success_fragments_hex.len() != 7
        || fixture.cancel_fragments_hex.len() != 2
        || fixture.expected_events != expected_events
        || fixture.expected_digest.len() != 64
        || !fixture
            .expected_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cli_error("transport benchmark fixture is invalid"));
    }

    let success_fragments = fixture
        .success_fragments_hex
        .iter()
        .map(|fragment| decode_hex(fragment))
        .collect::<AppResult<Vec<_>>>()?;
    let cancel_fragments: [Vec<u8>; 2] = fixture
        .cancel_fragments_hex
        .iter()
        .map(|fragment| decode_hex(fragment))
        .collect::<AppResult<Vec<_>>>()?
        .try_into()
        .map_err(|_| cli_error("transport cancel fixture must contain two fragments"))?;

    let mut parser = SseParser::default();
    for fragment in &success_fragments {
        parser.push(fragment)?;
    }
    let parsed = parser.finish()?;
    if parsed != fixture.expected_events {
        return Err(cli_error(
            "transport fixture fragments do not produce the expected SSE events",
        ));
    }

    let mut cancel_parser = SseParser::default();
    cancel_parser.push(&cancel_fragments[0])?;
    if cancel_parser.events() != fixture.expected_events.get(..1).unwrap_or_default() {
        return Err(cli_error(
            "transport cancel fixture does not stop after the first expected event",
        ));
    }

    Ok(ServerScenario {
        success_fragments,
        cancel_fragments,
        slow_response_delay: Duration::from_millis(fixture.slow_response_delay_ms),
    })
}

fn decode_hex(value: &str) -> AppResult<Vec<u8>> {
    if value.is_empty()
        || !value.len().is_multiple_of(2)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cli_error(
            "transport fixture hex fragments must be non-empty lowercase byte strings",
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> AppResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(cli_error("transport fixture contains invalid hexadecimal")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportEngine {
    #[cfg(feature = "bench-transport-reqwest")]
    ReqwestRustls,
    #[cfg(all(windows, feature = "bench-transport-winhttp"))]
    WinHttpWrest,
}

impl TransportEngine {
    const fn implementation(self) -> &'static str {
        match self {
            #[cfg(feature = "bench-transport-reqwest")]
            Self::ReqwestRustls => "reqwest-rustls",
            #[cfg(all(windows, feature = "bench-transport-winhttp"))]
            Self::WinHttpWrest => "winhttp-wrest",
        }
    }

    const fn dependencies(self) -> &'static str {
        match self {
            #[cfg(feature = "bench-transport-reqwest")]
            Self::ReqwestRustls => {
                "candidate=reqwest-rustls;feature=bench-transport;reqwest=0.13.4[http2,rustls,stream,system-proxy];tokio=1[rt,time];futures-util=0.3"
            }
            #[cfg(all(windows, feature = "bench-transport-winhttp"))]
            Self::WinHttpWrest => {
                "candidate=winhttp-wrest;feature=bench-transport;wrest=0.5.7[http2,stream,system-proxy];tokio=1[rt,time];futures-util=0.3"
            }
        }
    }
}

struct TransportTarget {
    engine: TransportEngine,
    fixture: TransportFixture,
    scenario: ServerScenario,
    origin: Option<LoopbackServer>,
    proxy: Option<LoopbackServer>,
}

impl BenchmarkTarget for TransportTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "transport",
            comparison_version: 1,
            implementation: self.engine.implementation(),
            implementation_revision: "1",
            dependencies: self.engine.dependencies(),
            workload_id: "loopback-sse",
            workload_version: self.fixture.workload_version,
            input_shape: "seven synthetic loopback requests: cold/warm SSE, HTTP 503, body timeout, mid-stream cancellation, explicit proxy, custom origin",
            unit: "verified transport cases",
            boundary: "create runtime and clients, execute loopback requests, incrementally parse SSE, and verify routing and credential isolation",
            process_mode: "in-process with loopback server threads",
            fixture_bytes: TRANSPORT_FIXTURE_JSON.as_bytes(),
        }
    }

    fn prepare_run(&mut self) -> AppResult<()> {
        if self.origin.is_some() || self.proxy.is_some() {
            return Err(cli_error("transport benchmark servers are already running"));
        }
        let origin = LoopbackServer::start(ServerRole::Origin, self.scenario.clone())?;
        let proxy = LoopbackServer::start(ServerRole::Proxy, self.scenario.clone())?;
        self.origin = Some(origin);
        self.proxy = Some(proxy);
        Ok(())
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        let origin = self
            .origin
            .as_ref()
            .ok_or_else(|| cli_error("transport origin server is not running"))?;
        let proxy = self
            .proxy
            .as_ref()
            .ok_or_else(|| cli_error("transport proxy server is not running"))?;
        run_transport_matrix(
            self.engine,
            &self.fixture,
            origin,
            proxy,
            &origin.url(),
            &proxy.url(),
        )
    }

    fn cleanup_run(&mut self) -> AppResult<()> {
        let proxy = self.proxy.take().map(|mut server| server.stop());
        let origin = self.origin.take().map(|mut server| server.stop());
        match (origin, proxy) {
            (Some(Err(error)), _) | (_, Some(Err(error))) => Err(error),
            _ => Ok(()),
        }
    }
}

fn run_transport_matrix(
    engine: TransportEngine,
    fixture: &TransportFixture,
    origin: &LoopbackServer,
    proxy: &LoopbackServer,
    origin_url: &str,
    proxy_url: &str,
) -> AppResult<BenchmarkObservation> {
    let runtime_started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let runtime_setup = elapsed_ns(runtime_started)?;
    let mut result = match engine {
        #[cfg(feature = "bench-transport-reqwest")]
        TransportEngine::ReqwestRustls => runtime.block_on(run_candidate::<ReqwestAdapter>(
            fixture, origin_url, proxy_url,
        ))?,
        #[cfg(all(windows, feature = "bench-transport-winhttp"))]
        TransportEngine::WinHttpWrest => runtime.block_on(run_candidate::<WinHttpWrestAdapter>(
            fixture, origin_url, proxy_url,
        ))?,
    };
    result
        .timings_ns
        .insert("runtime_setup".into(), runtime_setup);

    let verification_started = Instant::now();
    let origin_requests = origin.stats.requests.load(Ordering::Acquire);
    let proxy_requests = proxy.stats.requests.load(Ordering::Acquire);
    let custom_auth_cases = origin.stats.custom_auth_cases.load(Ordering::Acquire);
    let credential_leak_bytes = proxy.stats.credential_leak_bytes.load(Ordering::Acquire);
    if origin_requests != 6
        || proxy_requests != 1
        || custom_auth_cases != 1
        || credential_leak_bytes != 0
    {
        return Err(cli_error(format!(
            "transport route evidence is invalid: origin={origin_requests}, proxy={proxy_requests}, custom_auth={custom_auth_cases}, leaked_bytes={credential_leak_bytes}"
        )));
    }
    let digest = canonical_result_digest(&result, origin_requests, proxy_requests);
    if digest != fixture.expected_digest {
        return Err(cli_error(format!(
            "transport benchmark digest mismatch: expected {}, observed {digest}",
            fixture.expected_digest
        )));
    }
    result
        .timings_ns
        .insert("verification".into(), elapsed_ns(verification_started)?);
    result
        .gauges
        .insert("credential_leak_bytes".into(), credential_leak_bytes);
    result.gauges.insert(
        "origin_connections".into(),
        origin.stats.connections.load(Ordering::Acquire),
    );
    result
        .gauges
        .insert("origin_requests".into(), origin_requests);
    result.gauges.insert(
        "proxy_connections".into(),
        proxy.stats.connections.load(Ordering::Acquire),
    );
    result
        .gauges
        .insert("proxy_requests".into(), proxy_requests);
    result
        .gauges
        .insert("custom_origin_auth_cases".into(), custom_auth_cases);

    Ok(BenchmarkObservation {
        operation_units: u64::from(fixture.request_count),
        output_digest: digest,
        timings_ns: result.timings_ns,
        gauges: result.gauges,
    })
}

fn elapsed_ns(started: Instant) -> AppResult<u64> {
    u64::try_from(started.elapsed().as_nanos())
        .map_err(|_| cli_error("transport benchmark duration exceeds u64 nanoseconds"))
}

struct MatrixResult {
    event_sets: [Vec<SseEvent>; 4],
    cancelled_events: Vec<SseEvent>,
    timings_ns: BTreeMap<String, u64>,
    gauges: BTreeMap<String, u64>,
}

fn canonical_result_digest(
    result: &MatrixResult,
    origin_requests: u64,
    proxy_requests: u64,
) -> String {
    let mut canonical = String::from("transport/loopback-sse/v1");
    for (label, events) in ["cold", "warm", "proxy", "custom"]
        .into_iter()
        .zip(&result.event_sets)
    {
        canonical.push('|');
        canonical.push_str(label);
        canonical.push('=');
        for event in events {
            canonical.push_str(&event.event);
            canonical.push(':');
            canonical.push_str(&event.data);
            canonical.push(';');
        }
    }
    canonical.push_str("|http=503|timeout=1|cancel=");
    for event in &result.cancelled_events {
        canonical.push_str(&event.event);
        canonical.push(':');
        canonical.push_str(&event.data);
        canonical.push(';');
    }
    canonical.push_str(&format!(
        "|origin_requests={origin_requests}|proxy_requests={proxy_requests}|credential_leak_bytes=0"
    ));
    sha256_bytes(canonical.as_bytes())
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    event_type: Option<String>,
    data_lines: Vec<String>,
    events: Vec<SseEvent>,
    total_bytes: usize,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> AppResult<()> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(|| cli_error("SSE byte count overflow"))?;
        if self.total_bytes > MAX_SSE_BYTES {
            return Err(cli_error("SSE response exceeds the benchmark byte limit"));
        }
        self.buffer.extend_from_slice(chunk);
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            if newline > MAX_SSE_LINE_BYTES {
                return Err(cli_error("SSE line exceeds the benchmark byte limit"));
            }
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line)?;
        }
        if self.buffer.len() > MAX_SSE_LINE_BYTES {
            return Err(cli_error("SSE line exceeds the benchmark byte limit"));
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8]) -> AppResult<()> {
        if line.is_empty() {
            self.dispatch();
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| cli_error("SSE line is not valid UTF-8 after framing"))?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event_type = Some(value.into()),
            "data" => self.data_lines.push(value.into()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) {
        if !self.data_lines.is_empty() {
            self.events.push(SseEvent {
                event: self.event_type.take().unwrap_or_else(|| "message".into()),
                data: self.data_lines.join("\n"),
            });
        } else {
            self.event_type = None;
        }
        self.data_lines.clear();
    }

    fn events(&self) -> &[SseEvent] {
        &self.events
    }

    fn finish(self) -> AppResult<Vec<SseEvent>> {
        if !self.buffer.is_empty() || self.event_type.is_some() || !self.data_lines.is_empty() {
            return Err(cli_error("SSE response ended with an incomplete event"));
        }
        Ok(self.events)
    }
}

struct StreamRead {
    events: Vec<SseEvent>,
    body_bytes: u64,
    chunks: u64,
}

struct CancelRead {
    events: Vec<SseEvent>,
    body_bytes: u64,
    chunks: u64,
}

trait CandidateAdapter {
    type Client;

    fn build_direct(deadline: Duration) -> AppResult<Self::Client>;
    fn build_proxy(deadline: Duration, proxy_url: &str) -> AppResult<Self::Client>;
    async fn read_sse(
        client: &Self::Client,
        url: &str,
        authorization: Option<&str>,
    ) -> AppResult<StreamRead>;
    async fn expect_http_error(client: &Self::Client, url: &str) -> AppResult<(u64, u64)>;
    async fn expect_timeout(
        client: &Self::Client,
        url: &str,
        timeout: Duration,
    ) -> AppResult<(u64, u64)>;
    async fn cancel_after_first_event(client: &Self::Client, url: &str) -> AppResult<CancelRead>;
}

async fn run_candidate<A: CandidateAdapter>(
    fixture: &TransportFixture,
    origin_url: &str,
    proxy_url: &str,
) -> AppResult<MatrixResult> {
    let mut timings_ns = BTreeMap::new();
    let setup_started = Instant::now();
    let direct = A::build_direct(CLIENT_DEADLINE)?;
    let proxy = A::build_proxy(CLIENT_DEADLINE, proxy_url)?;
    timings_ns.insert("client_setup".into(), elapsed_ns(setup_started)?);

    let cold_started = Instant::now();
    let cold = A::read_sse(&direct, &format!("{origin_url}/sse"), None).await?;
    timings_ns.insert("cold_success".into(), elapsed_ns(cold_started)?);

    let warm_started = Instant::now();
    let warm = A::read_sse(&direct, &format!("{origin_url}/sse"), None).await?;
    timings_ns.insert("warm_success".into(), elapsed_ns(warm_started)?);

    let error_started = Instant::now();
    let (error_bytes, error_chunks) =
        A::expect_http_error(&direct, &format!("{origin_url}/error")).await?;
    timings_ns.insert("http_error".into(), elapsed_ns(error_started)?);

    let timeout_started = Instant::now();
    let (timeout_bytes, timeout_chunks) = A::expect_timeout(
        &direct,
        &format!("{origin_url}/timeout"),
        Duration::from_millis(fixture.request_timeout_ms),
    )
    .await?;
    timings_ns.insert("timeout".into(), elapsed_ns(timeout_started)?);

    let cancel_started = Instant::now();
    let cancelled = A::cancel_after_first_event(&direct, &format!("{origin_url}/cancel")).await?;
    timings_ns.insert("cancel".into(), elapsed_ns(cancel_started)?);

    let proxy_started = Instant::now();
    let proxied = A::read_sse(&proxy, "http://greentyper.invalid/proxy-sse", None).await?;
    timings_ns.insert("proxy".into(), elapsed_ns(proxy_started)?);

    let custom_started = Instant::now();
    let custom = A::read_sse(
        &direct,
        &format!("{origin_url}/custom-origin"),
        Some(SYNTHETIC_AUTHORIZATION),
    )
    .await?;
    timings_ns.insert("custom_origin".into(), elapsed_ns(custom_started)?);

    for events in [&cold.events, &warm.events, &proxied.events, &custom.events] {
        if events != &fixture.expected_events {
            return Err(cli_error(
                "transport candidate did not preserve the canonical SSE event sequence",
            ));
        }
    }
    if cancelled.events != fixture.expected_events[..1] {
        return Err(cli_error(
            "transport cancellation did not stop after the first canonical event",
        ));
    }

    let network_chunks = cold
        .chunks
        .checked_add(warm.chunks)
        .and_then(|value| value.checked_add(error_chunks))
        .and_then(|value| value.checked_add(timeout_chunks))
        .and_then(|value| value.checked_add(cancelled.chunks))
        .and_then(|value| value.checked_add(proxied.chunks))
        .and_then(|value| value.checked_add(custom.chunks))
        .ok_or_else(|| cli_error("transport network chunk count overflow"))?;
    let response_body_bytes = cold
        .body_bytes
        .checked_add(warm.body_bytes)
        .and_then(|value| value.checked_add(error_bytes))
        .and_then(|value| value.checked_add(timeout_bytes))
        .and_then(|value| value.checked_add(cancelled.body_bytes))
        .and_then(|value| value.checked_add(proxied.body_bytes))
        .and_then(|value| value.checked_add(custom.body_bytes))
        .ok_or_else(|| cli_error("transport response byte count overflow"))?;

    Ok(MatrixResult {
        event_sets: [cold.events, warm.events, proxied.events, custom.events],
        cancelled_events: cancelled.events,
        timings_ns,
        gauges: BTreeMap::from([
            ("cancelled_cases".into(), 1),
            ("events_verified".into(), 13),
            ("http_error_cases".into(), 1),
            ("network_chunks".into(), network_chunks),
            ("request_count".into(), u64::from(fixture.request_count)),
            ("response_body_bytes".into(), response_body_bytes),
            ("timeout_cases".into(), 1),
        ]),
    })
}

#[cfg(feature = "bench-transport-reqwest")]
struct ReqwestAdapter;

#[cfg(feature = "bench-transport-reqwest")]
impl CandidateAdapter for ReqwestAdapter {
    type Client = reqwest::Client;

    fn build_direct(deadline: Duration) -> AppResult<Self::Client> {
        Ok(reqwest::Client::builder()
            .no_proxy()
            .timeout(deadline)
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }

    fn build_proxy(deadline: Duration, proxy_url: &str) -> AppResult<Self::Client> {
        Ok(reqwest::Client::builder()
            .no_proxy()
            .proxy(reqwest::Proxy::all(proxy_url)?)
            .timeout(deadline)
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }

    async fn read_sse(
        client: &Self::Client,
        url: &str,
        authorization: Option<&str>,
    ) -> AppResult<StreamRead> {
        let mut request = client.get(url);
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let response = request.send().await?;
        if response.status().as_u16() != 200 {
            return Err(cli_error(format!(
                "Reqwest SSE request returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut parser = SseParser::default();
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            chunks = chunks
                .checked_add(1)
                .ok_or_else(|| cli_error("Reqwest chunk count overflow"))?;
            body_bytes = body_bytes
                .checked_add(u64::try_from(chunk.len())?)
                .ok_or_else(|| cli_error("Reqwest byte count overflow"))?;
            parser.push(&chunk)?;
        }
        Ok(StreamRead {
            events: parser.finish()?,
            body_bytes,
            chunks,
        })
    }

    async fn expect_http_error(client: &Self::Client, url: &str) -> AppResult<(u64, u64)> {
        let response = client.get(url).send().await?;
        if response.status().as_u16() != 503 {
            return Err(cli_error(format!(
                "Reqwest HTTP error case returned {} instead of 503",
                response.status().as_u16()
            )));
        }
        drain_reqwest(response).await
    }

    async fn expect_timeout(
        client: &Self::Client,
        url: &str,
        timeout: Duration,
    ) -> AppResult<(u64, u64)> {
        let response = match client.get(url).timeout(timeout).send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return Ok((0, 0)),
            Err(error) => return Err(error.into()),
        };
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    chunks += 1;
                    body_bytes += u64::try_from(chunk.len())?;
                }
                Err(error) if error.is_timeout() => return Ok((body_bytes, chunks)),
                Err(error) => return Err(error.into()),
            }
        }
        Err(cli_error(
            "Reqwest timeout case completed without timing out",
        ))
    }

    async fn cancel_after_first_event(client: &Self::Client, url: &str) -> AppResult<CancelRead> {
        let response = client.get(url).send().await?;
        if response.status().as_u16() != 200 {
            return Err(cli_error(
                "Reqwest cancellation case did not return HTTP 200",
            ));
        }
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut parser = SseParser::default();
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            chunks += 1;
            body_bytes += u64::try_from(chunk.len())?;
            parser.push(&chunk)?;
            if !parser.events().is_empty() {
                return Ok(CancelRead {
                    events: parser.events().to_vec(),
                    body_bytes,
                    chunks,
                });
            }
        }
        Err(cli_error(
            "Reqwest cancellation case ended before the first SSE event",
        ))
    }
}

#[cfg(feature = "bench-transport-reqwest")]
async fn drain_reqwest(response: reqwest::Response) -> AppResult<(u64, u64)> {
    let stream = response.bytes_stream();
    futures_util::pin_mut!(stream);
    let mut chunks = 0_u64;
    let mut body_bytes = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        chunks += 1;
        body_bytes += u64::try_from(chunk.len())?;
    }
    Ok((body_bytes, chunks))
}

#[cfg(all(windows, feature = "bench-transport-winhttp"))]
struct WinHttpWrestAdapter;

#[cfg(all(windows, feature = "bench-transport-winhttp"))]
impl CandidateAdapter for WinHttpWrestAdapter {
    type Client = wrest::Client;

    fn build_direct(deadline: Duration) -> AppResult<Self::Client> {
        Ok(wrest::Client::builder()
            .no_proxy()
            .timeout(deadline)
            .redirect(wrest::redirect::Policy::none())
            .build()?)
    }

    fn build_proxy(deadline: Duration, proxy_url: &str) -> AppResult<Self::Client> {
        Ok(wrest::Client::builder()
            .no_proxy()
            .proxy(wrest::Proxy::all(proxy_url)?)
            .timeout(deadline)
            .redirect(wrest::redirect::Policy::none())
            .build()?)
    }

    async fn read_sse(
        client: &Self::Client,
        url: &str,
        authorization: Option<&str>,
    ) -> AppResult<StreamRead> {
        let mut request = client.get(url);
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let response = request.send().await?;
        if response.status().as_u16() != 200 {
            return Err(cli_error(format!(
                "WinHTTP SSE request returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut parser = SseParser::default();
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            chunks = chunks
                .checked_add(1)
                .ok_or_else(|| cli_error("WinHTTP chunk count overflow"))?;
            body_bytes = body_bytes
                .checked_add(u64::try_from(chunk.len())?)
                .ok_or_else(|| cli_error("WinHTTP byte count overflow"))?;
            parser.push(&chunk)?;
        }
        Ok(StreamRead {
            events: parser.finish()?,
            body_bytes,
            chunks,
        })
    }

    async fn expect_http_error(client: &Self::Client, url: &str) -> AppResult<(u64, u64)> {
        let response = client.get(url).send().await?;
        if response.status().as_u16() != 503 {
            return Err(cli_error(format!(
                "WinHTTP error case returned {} instead of 503",
                response.status().as_u16()
            )));
        }
        drain_wrest(response).await
    }

    async fn expect_timeout(
        client: &Self::Client,
        url: &str,
        timeout: Duration,
    ) -> AppResult<(u64, u64)> {
        let response = match client.get(url).timeout(timeout).send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return Ok((0, 0)),
            Err(error) => return Err(error.into()),
        };
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    chunks += 1;
                    body_bytes += u64::try_from(chunk.len())?;
                }
                Err(error) if error.is_timeout() => return Ok((body_bytes, chunks)),
                Err(error) => return Err(error.into()),
            }
        }
        Err(cli_error(
            "WinHTTP timeout case completed without timing out",
        ))
    }

    async fn cancel_after_first_event(client: &Self::Client, url: &str) -> AppResult<CancelRead> {
        let response = client.get(url).send().await?;
        if response.status().as_u16() != 200 {
            return Err(cli_error(
                "WinHTTP cancellation case did not return HTTP 200",
            ));
        }
        let stream = response.bytes_stream();
        futures_util::pin_mut!(stream);
        let mut parser = SseParser::default();
        let mut chunks = 0_u64;
        let mut body_bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            chunks += 1;
            body_bytes += u64::try_from(chunk.len())?;
            parser.push(&chunk)?;
            if !parser.events().is_empty() {
                return Ok(CancelRead {
                    events: parser.events().to_vec(),
                    body_bytes,
                    chunks,
                });
            }
        }
        Err(cli_error(
            "WinHTTP cancellation case ended before the first SSE event",
        ))
    }
}

#[cfg(all(windows, feature = "bench-transport-winhttp"))]
async fn drain_wrest(response: wrest::Response) -> AppResult<(u64, u64)> {
    let stream = response.bytes_stream();
    futures_util::pin_mut!(stream);
    let mut chunks = 0_u64;
    let mut body_bytes = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        chunks += 1;
        body_bytes += u64::try_from(chunk.len())?;
    }
    Ok((body_bytes, chunks))
}

#[derive(Clone, Copy)]
enum ServerRole {
    Origin,
    Proxy,
}

#[derive(Default)]
struct ServerStats {
    connections: AtomicU64,
    requests: AtomicU64,
    custom_auth_cases: AtomicU64,
    credential_leak_bytes: AtomicU64,
}

struct LoopbackServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    stats: Arc<ServerStats>,
    join: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    fn start(role: ServerRole, scenario: ServerScenario) -> AppResult<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(ServerStats::default());
        let thread_stop = Arc::clone(&stop);
        let thread_stats = Arc::clone(&stats);
        let join = thread::Builder::new()
            .name(match role {
                ServerRole::Origin => "greentyper-transport-origin".into(),
                ServerRole::Proxy => "greentyper-transport-proxy".into(),
            })
            .spawn(move || {
                serve_loopback(
                    listener,
                    role,
                    Arc::new(scenario),
                    thread_stop,
                    thread_stats,
                )
            })?;
        Ok(Self {
            address,
            stop,
            stats,
            join: Some(join),
        })
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn stop(&mut self) -> AppResult<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| cli_error("transport loopback server thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn serve_loopback(
    listener: TcpListener,
    role: ServerRole,
    scenario: Arc<ServerScenario>,
    stop: Arc<AtomicBool>,
    stats: Arc<ServerStats>,
) {
    let mut connections = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stats
                    .connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |connections| {
                        (connections < MAX_SERVER_CONNECTIONS).then_some(connections + 1)
                    })
                    .is_err()
                {
                    drop(stream);
                    continue;
                }
                let connection_stop = Arc::clone(&stop);
                let connection_stats = Arc::clone(&stats);
                let connection_scenario = Arc::clone(&scenario);
                connections.push(thread::spawn(move || {
                    handle_connection(
                        stream,
                        role,
                        &connection_scenario,
                        &connection_stop,
                        &connection_stats,
                    );
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(SERVER_POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }
    for connection in connections {
        let _ = connection.join();
    }
}

fn handle_connection(
    mut stream: TcpStream,
    role: ServerRole,
    scenario: &ServerScenario,
    stop: &AtomicBool,
    stats: &ServerStats,
) {
    let _ = stream.set_read_timeout(Some(CONNECTION_POLL_INTERVAL));
    let _ = stream.set_write_timeout(Some(CLIENT_DEADLINE));
    loop {
        let request = match read_request(&mut stream, stop) {
            Ok(Some(request)) => request,
            Ok(None) | Err(_) => return,
        };
        stats.requests.fetch_add(1, Ordering::AcqRel);
        let keep_open = match role {
            ServerRole::Origin => serve_origin(&mut stream, &request, scenario, stop, stats),
            ServerRole::Proxy => serve_proxy(&mut stream, &request, scenario, stats),
        };
        if !keep_open || stop.load(Ordering::Acquire) {
            return;
        }
    }
}

struct ParsedRequest {
    target: String,
    headers: BTreeMap<String, String>,
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<Option<ParsedRequest>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    let deadline = Instant::now() + CLIENT_DEADLINE;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "transport request header deadline expired",
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > MAX_HEADER_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "transport request headers exceed the limit",
                    ));
                }
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    parse_request(&bytes).map(Some)
}

fn parse_request(bytes: &[u8]) -> io::Result<ParsedRequest> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    if request_parts.next() != Some("GET") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "benchmark server accepts only GET",
        ));
    }
    let target = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request target"))?;
    if request_parts.next() != Some("HTTP/1.1") || request_parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported HTTP request line",
        ));
    }
    let mut headers = BTreeMap::new();
    for (index, line) in lines.take_while(|line| !line.is_empty()).enumerate() {
        if index >= MAX_HEADER_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transport request contains too many headers",
            ));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP header"))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !value
                .bytes()
                .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transport request contains an invalid header",
            ));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.into())
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transport request contains a duplicate header",
            ));
        }
    }
    Ok(ParsedRequest {
        target: target.into(),
        headers,
    })
}

fn serve_origin(
    stream: &mut TcpStream,
    request: &ParsedRequest,
    scenario: &ServerScenario,
    stop: &AtomicBool,
    stats: &ServerStats,
) -> bool {
    match request.target.as_str() {
        "/sse" => write_sse(stream, &scenario.success_fragments, true),
        "/error" => write_response(
            stream,
            503,
            "Service Unavailable",
            b"synthetic unavailable",
            true,
        ),
        "/timeout" => {
            if write_sse_headers(stream, scenario.success_body_len(), true) {
                wait_or_stop(scenario.slow_response_delay, stop);
                if !stop.load(Ordering::Acquire) {
                    let _ = write_fragments(stream, &scenario.success_fragments);
                }
            }
            false
        }
        "/cancel" => {
            let body_len = scenario.cancel_fragments.iter().map(Vec::len).sum();
            if write_sse_headers(stream, body_len, true)
                && stream.write_all(&scenario.cancel_fragments[0]).is_ok()
                && stream.flush().is_ok()
            {
                wait_or_stop(scenario.slow_response_delay, stop);
                if !stop.load(Ordering::Acquire) {
                    let _ = stream.write_all(&scenario.cancel_fragments[1]);
                    let _ = stream.flush();
                }
            }
            false
        }
        "/custom-origin" => {
            if request.headers.get("authorization").map(String::as_str)
                == Some(SYNTHETIC_AUTHORIZATION)
            {
                stats.custom_auth_cases.fetch_add(1, Ordering::AcqRel);
                write_sse(stream, &scenario.success_fragments, true)
            } else {
                write_response(
                    stream,
                    403,
                    "Forbidden",
                    b"missing synthetic binding",
                    false,
                )
            }
        }
        _ => write_response(stream, 404, "Not Found", b"not found", false),
    }
}

fn serve_proxy(
    stream: &mut TcpStream,
    request: &ParsedRequest,
    scenario: &ServerScenario,
    stats: &ServerStats,
) -> bool {
    if let Some(value) = request.headers.get("authorization") {
        stats.credential_leak_bytes.fetch_add(
            u64::try_from(value.len()).unwrap_or(u64::MAX),
            Ordering::AcqRel,
        );
    }
    let host_matches =
        request.headers.get("host").map(String::as_str) == Some("greentyper.invalid");
    let target_matches =
        request.target == "http://greentyper.invalid/proxy-sse" || request.target == "/proxy-sse";
    if host_matches && target_matches {
        write_sse(stream, &scenario.success_fragments, true)
    } else {
        write_response(stream, 400, "Bad Request", b"invalid proxy route", false)
    }
}

fn write_sse(stream: &mut TcpStream, fragments: &[Vec<u8>], keep_alive: bool) -> bool {
    let body_len = fragments.iter().map(Vec::len).sum();
    write_sse_headers(stream, body_len, keep_alive) && write_fragments(stream, fragments)
}

fn write_sse_headers(stream: &mut TcpStream, body_len: usize, keep_alive: bool) -> bool {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {body_len}\r\nCache-Control: no-cache\r\nConnection: {connection}\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_ok()
}

fn write_fragments(stream: &mut TcpStream, fragments: &[Vec<u8>]) -> bool {
    for fragment in fragments {
        if stream
            .write_all(fragment)
            .and_then(|_| stream.flush())
            .is_err()
        {
            return false;
        }
        thread::yield_now();
    }
    true
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
    keep_alive: bool,
) -> bool {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    )
    .and_then(|_| stream.write_all(body))
    .and_then(|_| stream.flush())
    .is_ok()
        && keep_alive
}

fn wait_or_stop(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_fragments_split_utf8_and_line_endings_without_changing_events() {
        let fixture: TransportFixture =
            serde_json::from_str(TRANSPORT_FIXTURE_JSON).expect("transport fixture");
        let scenario = validate_fixture(&fixture).expect("valid transport fixture");
        let mut parser = SseParser::default();
        for fragment in &scenario.success_fragments {
            parser.push(fragment).expect("incremental SSE parse");
        }
        assert_eq!(
            parser.finish().expect("complete SSE"),
            fixture.expected_events
        );
        assert!(
            scenario
                .success_fragments
                .iter()
                .any(|fragment| fragment.ends_with(&[0xe4]))
        );
        assert!(
            scenario
                .success_fragments
                .iter()
                .any(|fragment| fragment.ends_with(&[0xe2]))
        );
        assert!(
            scenario
                .success_fragments
                .iter()
                .any(|fragment| fragment.ends_with(b"\r"))
        );
    }

    #[test]
    fn fixture_shape_is_frozen() {
        let mut fixture: TransportFixture =
            serde_json::from_str(TRANSPORT_FIXTURE_JSON).expect("transport fixture");
        fixture.request_timeout_ms += 1;
        assert!(validate_fixture(&fixture).is_err());
        let mut fixture: TransportFixture =
            serde_json::from_str(TRANSPORT_FIXTURE_JSON).expect("transport fixture");
        fixture.success_fragments_hex[0].make_ascii_uppercase();
        assert!(validate_fixture(&fixture).is_err());
    }

    #[test]
    fn malformed_or_unbounded_sse_fails_closed() {
        let mut parser = SseParser::default();
        assert!(parser.push(&vec![b'a'; MAX_SSE_LINE_BYTES + 1]).is_err());
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: \xff\n\n").is_err());
        let mut parser = SseParser::default();
        parser.push(b"data: incomplete").expect("bounded fragment");
        assert!(parser.finish().is_err());
    }

    #[test]
    fn request_parser_rejects_duplicate_invalid_and_excess_headers() {
        assert!(
            parse_request(b"GET /sse HTTP/1.1\r\nHost: local\r\nhost: duplicate\r\n\r\n").is_err()
        );
        assert!(parse_request(b"GET /sse HTTP/1.1\r\nBad Header: value\r\n\r\n").is_err());
        let mut request = String::from("GET /sse HTTP/1.1\r\n");
        for index in 0..=MAX_HEADER_COUNT {
            request.push_str(&format!("x-{index}: value\r\n"));
        }
        request.push_str("\r\n");
        assert!(parse_request(request.as_bytes()).is_err());
    }

    #[cfg(feature = "bench-transport-reqwest")]
    #[test]
    fn reqwest_candidate_runs_the_complete_loopback_matrix() {
        let fixture: TransportFixture =
            serde_json::from_str(TRANSPORT_FIXTURE_JSON).expect("transport fixture");
        let scenario = validate_fixture(&fixture).expect("valid transport fixture");
        let mut target = TransportTarget {
            engine: TransportEngine::ReqwestRustls,
            fixture,
            scenario,
            origin: None,
            proxy: None,
        };
        target.prepare_run().expect("servers start");
        let result = target.run_once();
        target.cleanup_run().expect("servers stop");
        let observation = result.expect("Reqwest matrix");
        assert_eq!(observation.operation_units, 7);
        assert_eq!(observation.gauges["origin_requests"], 6);
        assert_eq!(observation.gauges["proxy_requests"], 1);
        assert_eq!(observation.gauges["credential_leak_bytes"], 0);
    }

    #[cfg(all(
        windows,
        feature = "bench-transport-reqwest",
        feature = "bench-transport-winhttp"
    ))]
    #[test]
    fn windows_candidates_produce_identical_correctness_digests() {
        let fixture: TransportFixture =
            serde_json::from_str(TRANSPORT_FIXTURE_JSON).expect("transport fixture");
        let scenario = validate_fixture(&fixture).expect("valid transport fixture");
        let mut observations = Vec::new();
        for engine in [
            TransportEngine::WinHttpWrest,
            TransportEngine::ReqwestRustls,
        ] {
            let mut target = TransportTarget {
                engine,
                fixture: fixture.clone(),
                scenario: scenario.clone(),
                origin: None,
                proxy: None,
            };
            target.prepare_run().expect("servers start");
            let result = target.run_once();
            target.cleanup_run().expect("servers stop");
            observations.push(result.expect("transport matrix"));
        }
        assert_eq!(observations[0].output_digest, observations[1].output_digest);
        assert_eq!(
            observations[0].operation_units,
            observations[1].operation_units
        );
        assert_eq!(
            observations[0].gauges.keys().collect::<Vec<_>>(),
            observations[1].gauges.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            observations[0].timings_ns.keys().collect::<Vec<_>>(),
            observations[1].timings_ns.keys().collect::<Vec<_>>()
        );
    }

    #[cfg(all(not(windows), feature = "bench-transport-winhttp"))]
    #[test]
    fn winhttp_candidate_is_not_advertised_off_windows() {
        assert!(!catalog_entry().to_string().contains("winhttp-wrest"));
        assert!(target("winhttp-wrest", "loopback-sse").is_err());
    }
}
