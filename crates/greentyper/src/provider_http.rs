use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use greentyper_core::config::{
    ConfigDocument, ConfigPaths, ConfigRuntime, ConfigRuntimeError, DEFAULT_MAX_OUTPUT_BYTES,
};
use greentyper_core::provider::responses::{ResponsesSseDecoder, normalize_responses_events};
use greentyper_core::provider::{
    ProviderDialect, ProviderError, ProviderEvent, ProviderPricingSource, ProviderProfileSnapshot,
    ProviderRequest, ProviderRuntime,
};
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{StatusCode, Url};

const FIXTURE_PROFILE: &str = "responses-loopback";
const FIXTURE_MODEL: &str = "fixture-model";
const FIXTURE_ROUTE: &str = "/v1/responses";
const FIXTURE_TEMPLATE: &str = "openai-compatible";
const FIXTURE_CREDENTIAL_REFERENCE: &str = "responses-loopback-synthetic";
const SYNTHETIC_AUTHORIZATION: &str = "Bearer greentyper-synthetic-provider-token-v1";
const HTTP_TIMEOUT: Duration = Duration::from_millis(200);
const SERVER_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const PRIVATE_ERROR_BODY: &[u8] = b"provider-private-error-marker";
const SUCCESS_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/responses/v1/http-text.sse");

struct LoopbackResponsesProvider {
    client: Client,
    endpoint: Url,
    profile: ProviderProfileSnapshot,
}

impl fmt::Debug for LoopbackResponsesProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoopbackResponsesProvider")
            .field("transport", &"loopback-http")
            .field("authorization", &"synthetic-redacted")
            .finish()
    }
}

impl LoopbackResponsesProvider {
    fn new(profile: ProviderProfileSnapshot) -> Result<Self, ProviderError> {
        if profile.profile() != FIXTURE_PROFILE
            || profile.template() != FIXTURE_TEMPLATE
            || profile.credential_reference() != Some(FIXTURE_CREDENTIAL_REFERENCE)
            || !profile.supports(ProviderDialect::Responses)
            || profile.pricing_source() != Some(ProviderPricingSource::Unknown)
            || !profile.allow_insecure_loopback()
        {
            return Err(ProviderError::InvalidConfiguration(
                "loopback Responses Provider Profile does not match its fixture",
            ));
        }
        let endpoint = profile.endpoint(ProviderDialect::Responses).ok_or(
            ProviderError::InvalidConfiguration(
                "loopback Responses Provider Profile has no endpoint",
            ),
        )?;
        let endpoint = validate_loopback_endpoint(&endpoint)?;
        let client = Client::builder()
            .no_proxy()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProviderError::unavailable("Responses HTTP client setup failed"))?;
        Ok(Self {
            client,
            endpoint,
            profile,
        })
    }
}

impl ProviderRuntime for LoopbackResponsesProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.provider.profile() != FIXTURE_PROFILE
            || request.provider.model() != FIXTURE_MODEL
            || request.provider.profile_snapshot() != Some(&self.profile)
        {
            return Err(ProviderError::InvalidConfiguration(
                "loopback Responses provider identity does not match its fixture",
            ));
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "input": request.input,
            "model": request.provider.model(),
            "stream": true,
        }))
        .map_err(|_| ProviderError::InvalidRequest("Responses request could not be encoded"))?;
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream")
            .header(AUTHORIZATION, SYNTHETIC_AUTHORIZATION)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| ProviderError::unavailable("Responses HTTP request failed"))?;
        if response.status() != StatusCode::OK {
            return Err(ProviderError::unavailable(
                "Responses HTTP request returned a non-success status",
            ));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream")) {
            return Err(ProviderError::InvalidResponse(
                "Responses HTTP response has the wrong content type",
            ));
        }

        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        let mut decoder = ResponsesSseDecoder::new(max_output_bytes).map_err(|_| {
            ProviderError::InvalidConfiguration("Responses decoder limits are invalid")
        })?;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|_| ProviderError::unavailable("Responses HTTP stream failed"))?;
            if read == 0 {
                break;
            }
            decoder.push(&buffer[..read]).map_err(|_| {
                ProviderError::InvalidResponse("Responses HTTP stream was rejected")
            })?;
        }
        let events = decoder
            .finish()
            .map_err(|_| ProviderError::InvalidResponse("Responses HTTP stream ended invalidly"))?;
        normalize_responses_events(&events)
    }
}

fn validate_loopback_endpoint(value: &str) -> Result<Url, ProviderError> {
    let endpoint = Url::parse(value).map_err(|_| {
        ProviderError::InvalidConfiguration("Responses endpoint must be an absolute URL")
    })?;
    if endpoint.scheme() != "http"
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path() != FIXTURE_ROUTE
    {
        return Err(ProviderError::InvalidConfiguration(
            "Responses fixture endpoint is not an approved loopback route",
        ));
    }
    let host = endpoint
        .host_str()
        .ok_or(ProviderError::InvalidConfiguration(
            "Responses fixture endpoint has no host",
        ))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !loopback {
        return Err(ProviderError::InvalidConfiguration(
            "Responses fixture endpoint must remain on loopback",
        ));
    }
    Ok(endpoint)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderHttpSmokeScenario {
    Success,
    HttpError,
    Timeout,
}

impl ProviderHttpSmokeScenario {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "http-error" => Some(Self::HttpError),
            "timeout" => Some(Self::Timeout),
            _ => None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ProviderHttpSmokeOutcome {
    Succeeded(String),
    Unavailable,
}

pub(crate) fn run_smoke(
    ledger: &Path,
    scenario: ProviderHttpSmokeScenario,
    input: &str,
) -> Result<ProviderHttpSmokeOutcome, ProviderHttpError> {
    validate_smoke_ledger(ledger)?;
    let mut runtime = RuntimeKernel::open(ledger)?;
    let fixture = FixtureServer::spawn(scenario, input.to_owned())?;
    let config = fixture_config_runtime(
        fixture.base_url(),
        ConfigPaths::new(
            ledger.with_extension("provider-http-user.toml"),
            ledger.with_extension("provider-http-project.toml"),
        ),
    )?;
    let layers = config.config_layers()?.clone();
    let profile = config
        .selected_provider_profile()?
        .ok_or(ProviderHttpError::Harness(
            "Provider HTTP fixture profile was not frozen",
        ))?;
    let mut provider = LoopbackResponsesProvider::new(profile)?;

    let result = match runtime.execute(&layers, input.to_owned(), &mut provider) {
        Ok(output) => {
            let delivery = output.delivery();
            let text = output.text().to_owned();
            runtime
                .acknowledge(delivery)
                .map(|_| ProviderHttpSmokeOutcome::Succeeded(text))
                .map_err(ProviderHttpError::from)
        }
        Err(RuntimeError::Provider(ProviderError::Unavailable { .. })) => {
            Ok(ProviderHttpSmokeOutcome::Unavailable)
        }
        Err(error) => Err(ProviderHttpError::Runtime(error)),
    };
    fixture.finish()?;
    result
}

fn fixture_config_runtime(
    base_url: &str,
    paths: ConfigPaths,
) -> Result<ConfigRuntime, ProviderHttpError> {
    let base_url = serde_json::to_string(base_url)?;
    let document = ConfigDocument::parse(&format!(
        r#"
schema_version = 1

[provider]
profile = "{FIXTURE_PROFILE}"
model = "{FIXTURE_MODEL}"

[runtime]
max_output_bytes = {DEFAULT_MAX_OUTPUT_BYTES}

[providers.{FIXTURE_PROFILE}]
template = "{FIXTURE_TEMPLATE}"
credential = "{FIXTURE_CREDENTIAL_REFERENCE}"
base_url = {base_url}
dialects = ["responses"]
allow_insecure_loopback = true

[providers.{FIXTURE_PROFILE}.routes]
responses = "{FIXTURE_ROUTE}"

[providers.{FIXTURE_PROFILE}.pricing]
source = "unknown"
"#,
    ))?;
    ConfigRuntime::open(paths, document).map_err(ProviderHttpError::from)
}

fn validate_smoke_ledger(path: &Path) -> Result<(), ProviderHttpError> {
    if !path.is_absolute() || path.exists() {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP smoke Ledger must be a new absolute path",
        ));
    }
    let temp_root = std::env::temp_dir().canonicalize()?;
    let parent = path
        .parent()
        .ok_or(ProviderHttpError::Harness(
            "Provider HTTP smoke Ledger has no parent",
        ))?
        .canonicalize()?;
    let name =
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(ProviderHttpError::Harness(
                "Provider HTTP smoke Ledger name is invalid",
            ))?;
    if parent != temp_root
        || !name.starts_with("greentyper-provider-http-")
        || !name.ends_with(".ledger")
    {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP smoke Ledger must use the owned temporary namespace",
        ));
    }
    Ok(())
}

struct FixtureServer {
    base_url: String,
    handle: Option<JoinHandle<Result<(), ProviderHttpError>>>,
}

impl FixtureServer {
    fn spawn(
        scenario: ProviderHttpSmokeScenario,
        expected_input: String,
    ) -> Result<Self, ProviderHttpError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let handle = thread::spawn(move || serve_fixture(listener, scenario, &expected_input));
        Ok(Self {
            base_url: format!("http://{address}"),
            handle: Some(handle),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn finish(mut self) -> Result<(), ProviderHttpError> {
        self.join()
    }

    fn join(&mut self) -> Result<(), ProviderHttpError> {
        let handle = self.handle.take().ok_or(ProviderHttpError::Harness(
            "Provider HTTP fixture handle was already consumed",
        ))?;
        handle
            .join()
            .map_err(|_| ProviderHttpError::FixtureThreadPanicked)??;
        Ok(())
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_fixture(
    listener: TcpListener,
    scenario: ProviderHttpSmokeScenario,
    expected_input: &str,
) -> Result<(), ProviderHttpError> {
    let deadline = Instant::now() + SERVER_TIMEOUT;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(ProviderHttpError::Harness(
                        "Provider HTTP fixture received no request",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error.into()),
        }
    };
    stream.set_read_timeout(Some(SERVER_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_TIMEOUT))?;
    validate_fixture_request(&mut stream, expected_input)?;
    match scenario {
        ProviderHttpSmokeScenario::Success => write_fixture_response(
            &mut stream,
            "200 OK",
            "text/event-stream; charset=utf-8",
            SUCCESS_SSE,
            true,
        ),
        ProviderHttpSmokeScenario::HttpError => write_fixture_response(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            PRIVATE_ERROR_BODY,
            false,
        ),
        ProviderHttpSmokeScenario::Timeout => {
            thread::sleep(Duration::from_millis(350));
            let _ = write_fixture_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                SUCCESS_SSE,
                false,
            );
            Ok(())
        }
    }
}

fn validate_fixture_request(
    stream: &mut TcpStream,
    expected_input: &str,
) -> Result<(), ProviderHttpError> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(ProviderHttpError::Harness(
                "Provider HTTP fixture request ended early",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(ProviderHttpError::Harness(
                "Provider HTTP fixture request exceeded its byte limit",
            ));
        }
        if let Some(header_end) = find_header_end(&bytes) {
            if header_end > MAX_HEADER_BYTES {
                return Err(ProviderHttpError::Harness(
                    "Provider HTTP fixture headers exceeded their byte limit",
                ));
            }
            let headers = std::str::from_utf8(&bytes[..header_end]).map_err(|_| {
                ProviderHttpError::Harness("Provider HTTP fixture headers were not UTF-8")
            })?;
            let content_length = parse_fixture_headers(headers)?;
            break (header_end + 4, content_length);
        }
    };
    let expected_total =
        header_end
            .checked_add(content_length)
            .ok_or(ProviderHttpError::Harness(
                "Provider HTTP fixture request length overflowed",
            ))?;
    while bytes.len() < expected_total {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(ProviderHttpError::Harness(
                "Provider HTTP fixture body ended early",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(ProviderHttpError::Harness(
                "Provider HTTP fixture request exceeded its byte limit",
            ));
        }
    }
    if bytes.len() != expected_total {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP fixture request had trailing bytes",
        ));
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes[header_end..])?;
    let object = body.as_object().ok_or(ProviderHttpError::Harness(
        "Provider HTTP fixture body was not an object",
    ))?;
    if object.len() != 3
        || object.get("input").and_then(serde_json::Value::as_str) != Some(expected_input)
        || object.get("model").and_then(serde_json::Value::as_str) != Some(FIXTURE_MODEL)
        || object.get("stream").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP fixture body did not match the canonical request",
        ));
    }
    Ok(())
}

fn parse_fixture_headers(headers: &str) -> Result<usize, ProviderHttpError> {
    let mut lines = headers.split("\r\n");
    if lines.next() != Some("POST /v1/responses HTTP/1.1") {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP fixture received the wrong request line",
        ));
    }
    let mut authorization = None;
    let mut content_length = None;
    let mut content_type = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(ProviderHttpError::Harness(
            "Provider HTTP fixture received a malformed header",
        ))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            if authorization.replace(value).is_some() {
                return Err(ProviderHttpError::Harness(
                    "Provider HTTP fixture received duplicate authorization",
                ));
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length
                .replace(value.parse::<usize>().map_err(|_| {
                    ProviderHttpError::Harness("Provider HTTP fixture content length was invalid")
                })?)
                .is_some()
            {
                return Err(ProviderHttpError::Harness(
                    "Provider HTTP fixture received duplicate content length",
                ));
            }
        } else if name.eq_ignore_ascii_case("content-type") && content_type.replace(value).is_some()
        {
            return Err(ProviderHttpError::Harness(
                "Provider HTTP fixture received duplicate content type",
            ));
        }
    }
    if authorization != Some(SYNTHETIC_AUTHORIZATION) || content_type != Some("application/json") {
        return Err(ProviderHttpError::Harness(
            "Provider HTTP fixture request headers were not canonical",
        ));
    }
    content_length.ok_or(ProviderHttpError::Harness(
        "Provider HTTP fixture request omitted content length",
    ))
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_fixture_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    fragment: bool,
) -> Result<(), ProviderHttpError> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if fragment {
        for chunk in body.chunks(11) {
            stream.write_all(chunk)?;
            stream.flush()?;
        }
    } else {
        stream.write_all(body)?;
        stream.flush()?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum ProviderHttpError {
    Io(io::Error),
    Json(serde_json::Error),
    Provider(ProviderError),
    Runtime(RuntimeError),
    Config(ConfigRuntimeError),
    Harness(&'static str),
    FixtureThreadPanicked,
}

impl fmt::Display for ProviderHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "Provider HTTP fixture I/O failed: {source}"),
            Self::Json(_) => formatter.write_str("Provider HTTP fixture JSON was invalid"),
            Self::Provider(source) => write!(formatter, "{source}"),
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::Harness(reason) => write!(formatter, "Provider HTTP fixture failed: {reason}"),
            Self::FixtureThreadPanicked => {
                formatter.write_str("Provider HTTP fixture thread panicked")
            }
        }
    }
}

impl Error for ProviderHttpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
            Self::Provider(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::Harness(_) | Self::FixtureThreadPanicked => None,
        }
    }
}

impl From<io::Error> for ProviderHttpError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<serde_json::Error> for ProviderHttpError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

impl From<ProviderError> for ProviderHttpError {
    fn from(source: ProviderError) -> Self {
        Self::Provider(source)
    }
}

impl From<RuntimeError> for ProviderHttpError {
    fn from(source: RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

impl From<ConfigRuntimeError> for ProviderHttpError {
    fn from(source: ConfigRuntimeError) -> Self {
        Self::Config(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_provider_rejects_non_loopback_and_redacts_authorization() {
        for (index, base_url) in [
            "https://provider.invalid",
            "http://198.51.100.1",
            "http://user:password@127.0.0.1",
            "http://127.0.0.1?private=query",
        ]
        .into_iter()
        .enumerate()
        {
            let paths = ConfigPaths::new(
                std::env::temp_dir().join(format!("greentyper-provider-http-unit-{index}-user")),
                std::env::temp_dir().join(format!("greentyper-provider-http-unit-{index}-project")),
            );
            let runtime = fixture_config_runtime(base_url, paths).expect("open repairable Config");
            assert!(!runtime.status().ready);
            assert!(runtime.selected_provider_profile().is_err());
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let paths = ConfigPaths::new(
            std::env::temp_dir().join("greentyper-provider-http-unit-valid-user"),
            std::env::temp_dir().join("greentyper-provider-http-unit-valid-project"),
        );
        let runtime = fixture_config_runtime(&base_url, paths).expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        let provider = LoopbackResponsesProvider::new(profile).expect("loopback Provider");
        let debug = format!("{provider:?}");
        assert!(!debug.contains(SYNTHETIC_AUTHORIZATION));
        assert!(debug.contains("synthetic-redacted"));
    }
}
