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
    DeterministicProvider, ProviderDialect, ProviderEpoch, ProviderError, ProviderEvent,
    ProviderPricingSource, ProviderProfileSnapshot, ProviderRequest, ProviderRuntime,
};
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{StatusCode, Url};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, InMemoryCredentialVault, ProviderCredentialScope,
    SecretValue,
};

const FIXTURE_PROFILE: &str = "responses-loopback";
const FIXTURE_MODEL: &str = "fixture-model";
const FIXTURE_ROUTE: &str = "/v1/responses";
const FIXTURE_TEMPLATE: &str = "openai-compatible";
const FIXTURE_CREDENTIAL_REFERENCE: &str = "responses-loopback-synthetic";
const SYNTHETIC_AUTHORIZATION: &str = "Bearer greentyper-synthetic-provider-token-v1";
const SYNTHETIC_SECRET: &[u8] = b"greentyper-synthetic-provider-token-v1";
const HTTP_TIMEOUT: Duration = Duration::from_millis(200);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(300);
const SERVER_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const PRIVATE_ERROR_BODY: &[u8] = b"provider-private-error-marker";
const SUCCESS_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/responses/v1/http-text.sse");

pub(crate) struct ResponsesHttpProvider<V> {
    client: Client,
    endpoint: Url,
    profile: ProviderProfileSnapshot,
    credential_scope: ProviderCredentialScope,
    vault: V,
}

impl<V> fmt::Debug for ResponsesHttpProvider<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesHttpProvider")
            .field("transport", &"blocking-http-sse")
            .field("authorization", &"redacted")
            .finish()
    }
}

impl<V: CredentialVault> ResponsesHttpProvider<V> {
    fn new(profile: ProviderProfileSnapshot, vault: V) -> Result<Self, ProviderError> {
        Self::with_timeout(profile, vault, PROVIDER_TIMEOUT)
    }

    fn with_timeout(
        profile: ProviderProfileSnapshot,
        vault: V,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Self::with_client_builder(profile, vault, timeout, Client::builder())
    }

    #[cfg(test)]
    fn with_timeout_and_root(
        profile: ProviderProfileSnapshot,
        vault: V,
        timeout: Duration,
        root: reqwest::Certificate,
    ) -> Result<Self, ProviderError> {
        Self::with_client_builder(
            profile,
            vault,
            timeout,
            Client::builder().add_root_certificate(root),
        )
    }

    fn with_client_builder(
        profile: ProviderProfileSnapshot,
        vault: V,
        timeout: Duration,
        client: ClientBuilder,
    ) -> Result<Self, ProviderError> {
        if profile.template() != FIXTURE_TEMPLATE || !profile.supports(ProviderDialect::Responses) {
            return Err(ProviderError::InvalidConfiguration(
                "Responses Provider Profile is not OpenAI-compatible",
            ));
        }
        let endpoint = profile.endpoint(ProviderDialect::Responses).ok_or(
            ProviderError::InvalidConfiguration("Responses Provider Profile has no endpoint"),
        )?;
        let endpoint = validate_responses_endpoint(&endpoint, profile.allow_insecure_loopback())?;
        let credential_scope = ProviderCredentialScope::from_profile(&profile)
            .map_err(map_credential_configuration_error)?;
        drop(
            vault
                .resolve(&credential_scope)
                .map_err(map_credential_resolve_error)?,
        );
        let client = client
            .no_proxy()
            .https_only(endpoint.scheme() == "https")
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProviderError::unavailable("Responses HTTP client setup failed"))?;
        Ok(Self {
            client,
            endpoint,
            profile,
            credential_scope,
            vault,
        })
    }
}

impl<V: CredentialVault> ProviderRuntime for ResponsesHttpProvider<V> {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.provider.profile() != self.profile.profile()
            || request.provider.profile_snapshot() != Some(&self.profile)
        {
            return Err(ProviderError::InvalidConfiguration(
                "Responses provider identity does not match its frozen Profile",
            ));
        }
        let secret = self
            .vault
            .resolve(&self.credential_scope)
            .map_err(map_credential_resolve_error)?;
        let authorization = bearer_header(&secret)?;
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
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| ProviderError::unavailable("Responses HTTP request failed"))?;
        if response.status() != StatusCode::OK {
            return Err(classify_http_status(response.status()));
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

pub(crate) enum ConfiguredProvider<V> {
    Simulator(DeterministicProvider),
    Responses(Box<ResponsesHttpProvider<V>>),
}

impl<V: CredentialVault> ConfiguredProvider<V> {
    pub(crate) fn for_new_turn(
        profile: Option<ProviderProfileSnapshot>,
        vault: V,
    ) -> Result<Self, ProviderError> {
        match profile {
            Some(profile) => ResponsesHttpProvider::new(profile, vault)
                .map(Box::new)
                .map(Self::Responses),
            None => Ok(Self::Simulator(DeterministicProvider::default())),
        }
    }

    pub(crate) fn from_epoch(epoch: &ProviderEpoch, vault: V) -> Result<Self, ProviderError> {
        match (epoch.profile(), epoch.profile_snapshot()) {
            ("simulator", None) => Ok(Self::Simulator(DeterministicProvider::default())),
            ("simulator", Some(_)) => Err(ProviderError::InvalidConfiguration(
                "simulator Provider Epoch cannot carry a Profile snapshot",
            )),
            (_, Some(profile)) => ResponsesHttpProvider::new(profile.clone(), vault)
                .map(Box::new)
                .map(Self::Responses),
            (_, None) => Err(ProviderError::InvalidConfiguration(
                "non-simulator Provider Epoch has no frozen Profile",
            )),
        }
    }
}

impl<V: CredentialVault> ProviderRuntime for ConfiguredProvider<V> {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        match self {
            Self::Simulator(provider) => provider.profile_snapshot(),
            Self::Responses(provider) => provider.profile_snapshot(),
        }
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        match self {
            Self::Simulator(provider) => provider.run(request),
            Self::Responses(provider) => provider.run(request),
        }
    }
}

struct LoopbackResponsesProvider {
    inner: ResponsesHttpProvider<InMemoryCredentialVault>,
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
        validate_loopback_endpoint(&endpoint)?;
        let scope = ProviderCredentialScope::from_profile(&profile)
            .map_err(map_credential_configuration_error)?;
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec())
                    .map_err(map_credential_configuration_error)?,
            )
            .map_err(map_credential_configuration_error)?;
        Ok(Self {
            inner: ResponsesHttpProvider::with_timeout(profile, vault, HTTP_TIMEOUT)?,
        })
    }
}

impl ProviderRuntime for LoopbackResponsesProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        self.inner.profile_snapshot()
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.provider.profile() != FIXTURE_PROFILE
            || request.provider.model() != FIXTURE_MODEL
        {
            return Err(ProviderError::InvalidConfiguration(
                "loopback Responses provider identity does not match its fixture",
            ));
        }
        self.inner.run(request)
    }
}

fn bearer_header(secret: &SecretValue) -> Result<HeaderValue, ProviderError> {
    let mut bytes = Vec::with_capacity("Bearer ".len() + secret.expose().len());
    bytes.extend_from_slice(b"Bearer ");
    bytes.extend_from_slice(secret.expose());
    let header = HeaderValue::from_bytes(&bytes)
        .map_err(|_| ProviderError::InvalidConfiguration("Provider credential is invalid"));
    bytes.fill(0);
    let mut header = header?;
    header.set_sensitive(true);
    Ok(header)
}

fn map_credential_configuration_error(_error: CredentialVaultError) -> ProviderError {
    ProviderError::InvalidConfiguration("Provider credential binding is invalid")
}

fn map_credential_resolve_error(error: CredentialVaultError) -> ProviderError {
    match error {
        CredentialVaultError::NotFound => {
            ProviderError::InvalidConfiguration("Provider credential binding was not found")
        }
        CredentialVaultError::Unavailable => {
            ProviderError::unavailable("Provider credential vault is unavailable")
        }
        CredentialVaultError::InvalidScope(_)
        | CredentialVaultError::InvalidSecret
        | CredentialVaultError::AlreadyBound => {
            ProviderError::InvalidConfiguration("Provider credential binding is invalid")
        }
    }
}

fn classify_http_status(status: StatusCode) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ProviderError::InvalidConfiguration("Provider credential was rejected")
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::unavailable("Responses HTTP request was temporarily rejected")
        }
        status if status.is_server_error() => {
            ProviderError::unavailable("Responses HTTP service failed")
        }
        status if status.is_redirection() => {
            ProviderError::InvalidResponse("Responses HTTP redirect was rejected")
        }
        status if status.is_client_error() => {
            ProviderError::InvalidRequest("Responses HTTP request was rejected")
        }
        _ => ProviderError::InvalidResponse("Responses HTTP status was invalid"),
    }
}

fn validate_responses_endpoint(
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<Url, ProviderError> {
    let endpoint = Url::parse(value).map_err(|_| {
        ProviderError::InvalidConfiguration("Responses endpoint must be an absolute URL")
    })?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::InvalidConfiguration(
            "Responses endpoint contains unsupported URL components",
        ));
    }
    let host = endpoint
        .host_str()
        .ok_or(ProviderError::InvalidConfiguration(
            "Responses endpoint has no host",
        ))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if endpoint.scheme() == "http" && (!loopback || !allow_insecure_loopback) {
        return Err(ProviderError::InvalidConfiguration(
            "plain HTTP requires explicit loopback permission",
        ));
    }
    if !loopback && allow_insecure_loopback {
        return Err(ProviderError::InvalidConfiguration(
            "loopback permission is invalid for a remote endpoint",
        ));
    }
    Ok(endpoint)
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
    provider_config_runtime(base_url, true, paths)
}

fn provider_config_runtime(
    base_url: &str,
    allow_insecure_loopback: bool,
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
allow_insecure_loopback = {allow_insecure_loopback}

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
    configure_fixture_stream(&stream)?;
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

fn configure_fixture_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(SERVER_TIMEOUT))?;
    stream.set_write_timeout(Some(SERVER_TIMEOUT))
}

fn validate_fixture_request(
    stream: &mut impl Read,
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
    stream: &mut impl Write,
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use greentyper_core::config::{ConfigEpoch, ConfigLayers};
    use greentyper_core::model::{ConfigEpochId, ProviderEpochId, ThreadId, TurnId};
    use greentyper_core::provider::ProviderEpoch;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use rustls::{ServerConfig, ServerConnection, StreamOwned};

    use crate::credential_vault::InMemoryCredentialVault;

    static NEXT_CONFIG: AtomicU64 = AtomicU64::new(1);

    fn test_config_paths(name: &str) -> ConfigPaths {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let stem = format!(
            "greentyper-provider-http-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        );
        ConfigPaths::new(
            std::env::temp_dir().join(format!("{stem}-user")),
            std::env::temp_dir().join(format!("{stem}-project")),
        )
    }

    fn provider_request(profile: ProviderProfileSnapshot, input: &str) -> ProviderRequest {
        ProviderRequest {
            thread: ThreadId::new(1).expect("thread"),
            turn: TurnId::new(1).expect("turn"),
            config: ConfigEpoch::freeze(
                ConfigEpochId::new(1).expect("Config Epoch"),
                &ConfigLayers::default(),
            )
            .expect("Config"),
            provider: ProviderEpoch::with_profile_snapshot(
                ProviderEpochId::new(1).expect("Provider Epoch"),
                FIXTURE_PROFILE,
                FIXTURE_MODEL,
                profile,
            )
            .expect("Provider Epoch"),
            input: input.to_owned(),
        }
    }

    #[test]
    fn fixture_socket_configuration_restores_blocking_reads() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let writer = thread::spawn(move || {
            let mut client = TcpStream::connect(address).expect("connect fixture client");
            thread::sleep(Duration::from_millis(50));
            client.write_all(b"x").expect("write delayed request byte");
        });
        let (mut server, _) = listener.accept().expect("accept fixture client");
        server
            .set_nonblocking(true)
            .expect("simulate inherited nonblocking socket");
        configure_fixture_stream(&server).expect("configure fixture socket");

        let mut byte = [0_u8; 1];
        server
            .read_exact(&mut byte)
            .expect("fixture read must wait for delayed client data");
        writer.join().expect("join fixture client");
        assert_eq!(byte, *b"x");
    }

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
            let paths = test_config_paths(&format!("invalid-{index}"));
            let runtime = fixture_config_runtime(base_url, paths).expect("open repairable Config");
            assert!(!runtime.status().ready);
            assert!(runtime.selected_provider_profile().is_err());
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let paths = test_config_paths("valid-loopback");
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

    #[test]
    fn responses_provider_requires_origin_bound_credential_before_network() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let paths = test_config_paths("missing-credential");
        let runtime = fixture_config_runtime(&base_url, paths).expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        assert!(matches!(
            ResponsesHttpProvider::with_timeout(
                profile,
                InMemoryCredentialVault::default(),
                HTTP_TIMEOUT,
            ),
            Err(ProviderError::InvalidConfiguration(
                "Provider credential binding was not found"
            ))
        ));
    }

    #[test]
    fn configured_provider_fails_closed_for_non_simulator_epoch_without_snapshot() {
        let legacy = ProviderEpoch::new(
            ProviderEpochId::new(1).unwrap(),
            "legacy-provider",
            "legacy-model",
        )
        .expect("legacy Provider Epoch");

        assert!(matches!(
            ConfiguredProvider::from_epoch(&legacy, InMemoryCredentialVault::default()),
            Err(ProviderError::InvalidConfiguration(
                "non-simulator Provider Epoch has no frozen Profile"
            ))
        ));
    }

    #[test]
    fn responses_endpoint_and_status_policy_fail_closed() {
        assert!(
            validate_responses_endpoint("https://provider.example/v1/responses", false).is_ok()
        );
        assert!(validate_responses_endpoint("http://127.0.0.1/v1/responses", true).is_ok());
        for (endpoint, allow_insecure_loopback) in [
            ("http://127.0.0.1/v1/responses", false),
            ("http://198.51.100.1/v1/responses", false),
            ("http://198.51.100.1/v1/responses", true),
            ("https://provider.example/v1/responses", true),
            ("https://user:password@provider.example/v1/responses", false),
            ("https://provider.example/v1/responses?secret=value", false),
            ("https://provider.example/v1/responses#fragment", false),
        ] {
            assert!(
                validate_responses_endpoint(endpoint, allow_insecure_loopback).is_err(),
                "endpoint must be rejected: {endpoint}"
            );
        }

        assert!(matches!(
            classify_http_status(StatusCode::UNAUTHORIZED),
            ProviderError::InvalidConfiguration("Provider credential was rejected")
        ));
        assert!(matches!(
            classify_http_status(StatusCode::BAD_REQUEST),
            ProviderError::InvalidRequest("Responses HTTP request was rejected")
        ));
        assert!(matches!(
            classify_http_status(StatusCode::FOUND),
            ProviderError::InvalidResponse("Responses HTTP redirect was rejected")
        ));
        assert!(matches!(
            classify_http_status(StatusCode::TOO_MANY_REQUESTS),
            ProviderError::Unavailable { .. }
        ));
        assert!(matches!(
            classify_http_status(StatusCode::SERVICE_UNAVAILABLE),
            ProviderError::Unavailable { .. }
        ));
    }

    #[test]
    fn responses_provider_rejects_an_untrusted_https_certificate() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()]).expect("test certificate");
        let private_key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key.into())
            .expect("TLS server config");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("TLS listener");
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("TLS accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("TLS read timeout");
            let connection =
                ServerConnection::new(std::sync::Arc::new(server_config)).expect("TLS connection");
            let mut stream = StreamOwned::new(connection, stream);
            let mut byte = [0_u8; 1];
            assert!(stream.read(&mut byte).is_err());
        });

        let base_url = format!("https://localhost:{}", address.port());
        let runtime =
            provider_config_runtime(&base_url, false, test_config_paths("untrusted-https"))
                .expect("valid HTTPS Provider Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve HTTPS Provider Profile")
            .expect("HTTPS Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind HTTPS credential");
        let mut provider =
            ResponsesHttpProvider::with_timeout(profile.clone(), vault, Duration::from_secs(2))
                .expect("HTTPS Responses provider");

        assert!(matches!(
            provider.run(&provider_request(profile, "untrusted https")),
            Err(ProviderError::Unavailable { .. })
        ));
        server.join().expect("join TLS server");
    }

    #[test]
    fn responses_provider_accepts_verified_https_with_origin_bound_credential() {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()]).expect("test certificate");
        let certificate = cert.der().clone();
        let private_key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key.into())
            .expect("TLS server config");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("TLS listener");
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("TLS accept");
            let connection =
                ServerConnection::new(std::sync::Arc::new(server_config)).expect("TLS connection");
            let mut stream = StreamOwned::new(connection, stream);
            validate_fixture_request(&mut stream, "verified https").expect("HTTPS request");
            write_fixture_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                SUCCESS_SSE,
                true,
            )
            .expect("HTTPS response");
        });

        let base_url = format!("https://localhost:{}", address.port());
        let paths = test_config_paths("verified-https");
        let runtime =
            provider_config_runtime(&base_url, false, paths).expect("valid HTTPS Provider Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve HTTPS Provider Profile")
            .expect("HTTPS Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind HTTPS credential");
        let root = reqwest::Certificate::from_der(certificate.as_ref()).expect("client root");
        let mut provider = ResponsesHttpProvider::with_timeout_and_root(
            profile.clone(),
            vault,
            Duration::from_secs(2),
            root,
        )
        .expect("HTTPS Responses provider");
        let request = provider_request(profile, "verified https");
        let events = provider.run(&request).expect("verified HTTPS response");

        assert!(matches!(events.last(), Some(ProviderEvent::Completed(_))));
        server.join().expect("join TLS server");
    }
}
