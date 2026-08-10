//! Provider-neutral Turn requests, canonical stream events, and a deterministic simulator.

pub mod chat_completions;
pub mod messages;
pub mod responses;
pub mod sse;

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::config::ConfigEpoch;
use crate::model::{ProviderEpochId, ThreadId, TurnId};

pub const MAX_PROVIDER_ID_BYTES: usize = 512;
pub const MAX_PROVIDER_ERROR_BYTES: usize = 4096;
pub const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_SERVICE_TIER_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDialect {
    Responses,
    ChatCompletions,
    Messages,
}

impl ProviderDialect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPricingSource {
    Unknown,
    Template,
    Manual,
    ProviderReported,
}

impl ProviderPricingSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Template => "template",
            Self::Manual => "manual",
            Self::ProviderReported => "provider_reported",
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderProfileSnapshot {
    profile: String,
    template: String,
    credential_reference: Option<String>,
    base_url: Option<String>,
    responses_route: Option<String>,
    chat_completions_route: Option<String>,
    messages_route: Option<String>,
    models_route: Option<String>,
    dialects: BTreeSet<ProviderDialect>,
    pricing_source: Option<ProviderPricingSource>,
    allow_insecure_loopback: bool,
    fingerprint: u64,
}

impl ProviderProfileSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        profile: impl Into<String>,
        template: impl Into<String>,
        credential_reference: Option<String>,
        base_url: Option<String>,
        responses_route: Option<String>,
        chat_completions_route: Option<String>,
        messages_route: Option<String>,
        models_route: Option<String>,
        dialects: impl IntoIterator<Item = ProviderDialect>,
        pricing_source: Option<ProviderPricingSource>,
        allow_insecure_loopback: bool,
    ) -> Result<Self, ProviderError> {
        let profile = profile.into();
        let template = template.into();
        validate_provider_id("provider profile", &profile)?;
        validate_snapshot_value("provider template", &template)?;
        if let Some(reference) = credential_reference.as_deref() {
            validate_snapshot_value("provider credential reference", reference)?;
        }
        let base_url = base_url
            .as_deref()
            .map(|value| normalize_provider_origin(value, allow_insecure_loopback))
            .transpose()?;
        if base_url.is_none() && allow_insecure_loopback {
            return Err(ProviderError::InvalidConfiguration(
                "insecure loopback requires a provider origin",
            ));
        }
        let responses_route = normalize_optional_route(responses_route)?;
        let chat_completions_route = normalize_optional_route(chat_completions_route)?;
        let messages_route = normalize_optional_route(messages_route)?;
        let models_route = normalize_optional_route(models_route)?;
        let dialects = dialects.into_iter().collect::<BTreeSet<_>>();
        if dialects.is_empty() {
            return Err(ProviderError::InvalidConfiguration(
                "provider dialect set cannot be empty",
            ));
        }
        let mut snapshot = Self {
            profile,
            template,
            credential_reference,
            base_url,
            responses_route,
            chat_completions_route,
            messages_route,
            models_route,
            dialects,
            pricing_source,
            allow_insecure_loopback,
            fingerprint: 0,
        };
        snapshot.fingerprint = fingerprint_profile_snapshot(&snapshot);
        Ok(snapshot)
    }

    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub fn template(&self) -> &str {
        &self.template
    }

    #[must_use]
    pub fn credential_reference(&self) -> Option<&str> {
        self.credential_reference.as_deref()
    }

    #[must_use]
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    #[must_use]
    pub fn route(&self, dialect: ProviderDialect) -> Option<&str> {
        match dialect {
            ProviderDialect::Responses => self.responses_route.as_deref(),
            ProviderDialect::ChatCompletions => self.chat_completions_route.as_deref(),
            ProviderDialect::Messages => self.messages_route.as_deref(),
        }
    }

    #[must_use]
    pub fn models_route(&self) -> Option<&str> {
        self.models_route.as_deref()
    }

    #[must_use]
    pub fn models_endpoint(&self) -> Option<String> {
        Some(format!("{}{}", self.base_url()?, self.models_route()?))
    }

    #[must_use]
    pub fn endpoint(&self, dialect: ProviderDialect) -> Option<String> {
        Some(format!("{}{}", self.base_url()?, self.route(dialect)?))
    }

    #[must_use]
    pub fn supports(&self, dialect: ProviderDialect) -> bool {
        self.dialects.contains(&dialect)
    }

    pub fn dialects(&self) -> impl ExactSizeIterator<Item = ProviderDialect> + '_ {
        self.dialects.iter().copied()
    }

    #[must_use]
    pub const fn pricing_source(&self) -> Option<ProviderPricingSource> {
        self.pricing_source
    }

    #[must_use]
    pub const fn allow_insecure_loopback(&self) -> bool {
        self.allow_insecure_loopback
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

impl fmt::Debug for ProviderProfileSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderProfileSnapshot")
            .field("profile", &self.profile)
            .field("template", &self.template)
            .field("dialect_count", &self.dialects.len())
            .field(
                "has_credential_reference",
                &self.credential_reference.is_some(),
            )
            .field("has_origin", &self.base_url.is_some())
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEpoch {
    id: ProviderEpochId,
    profile: String,
    model: String,
    profile_snapshot: Option<ProviderProfileSnapshot>,
    dialect: Option<ProviderDialect>,
    fingerprint: u64,
}

impl ProviderEpoch {
    pub fn new(
        id: ProviderEpochId,
        profile: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let profile = profile.into();
        let model = model.into();
        validate_provider_id("provider profile", &profile)?;
        validate_provider_id("provider model", &model)?;
        let fingerprint = fingerprint_provider_epoch(&profile, &model, None, None);
        Ok(Self {
            id,
            profile,
            model,
            profile_snapshot: None,
            dialect: None,
            fingerprint,
        })
    }

    pub fn with_profile_snapshot(
        id: ProviderEpochId,
        profile: impl Into<String>,
        model: impl Into<String>,
        profile_snapshot: ProviderProfileSnapshot,
    ) -> Result<Self, ProviderError> {
        let profile = profile.into();
        let model = model.into();
        validate_provider_id("provider profile", &profile)?;
        validate_provider_id("provider model", &model)?;
        if profile_snapshot.profile() != profile {
            return Err(ProviderError::InvalidConfiguration(
                "Provider Profile snapshot identity mismatch",
            ));
        }
        Self::with_profile_snapshot_and_dialect(id, profile, model, profile_snapshot, None)
    }

    pub fn with_profile_snapshot_and_dialect(
        id: ProviderEpochId,
        profile: impl Into<String>,
        model: impl Into<String>,
        profile_snapshot: ProviderProfileSnapshot,
        dialect: Option<ProviderDialect>,
    ) -> Result<Self, ProviderError> {
        let profile = profile.into();
        let model = model.into();
        validate_provider_id("provider profile", &profile)?;
        validate_provider_id("provider model", &model)?;
        if profile_snapshot.profile() != profile {
            return Err(ProviderError::InvalidConfiguration(
                "Provider Profile snapshot identity mismatch",
            ));
        }
        if dialect.is_some_and(|dialect| !profile_snapshot.supports(dialect)) {
            return Err(ProviderError::InvalidConfiguration(
                "Provider dialect is not supported by its frozen Profile",
            ));
        }
        let fingerprint =
            fingerprint_provider_epoch(&profile, &model, Some(&profile_snapshot), dialect);
        Ok(Self {
            id,
            profile,
            model,
            profile_snapshot: Some(profile_snapshot),
            dialect,
            fingerprint,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ProviderEpochId {
        self.id
    }

    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub const fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        self.profile_snapshot.as_ref()
    }

    #[must_use]
    pub const fn dialect(&self) -> Option<ProviderDialect> {
        self.dialect
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderRequest {
    pub thread: ThreadId,
    pub turn: TurnId,
    pub config: ConfigEpoch,
    pub provider: ProviderEpoch,
    pub input: String,
}

impl fmt::Debug for ProviderRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequest")
            .field("thread", &self.thread)
            .field("turn", &self.turn)
            .field("config", &self.config.id())
            .field("provider", &self.provider.id())
            .field("input_bytes", &self.input.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAccuracy {
    #[default]
    Exact,
    Estimated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UsageRecord {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    service_tier: Option<String>,
    accuracy: UsageAccuracy,
}

impl Default for UsageRecord {
    fn default() -> Self {
        Self {
            input_tokens: None,
            cached_input_tokens: None,
            cache_write_input_tokens: None,
            output_tokens: None,
            reasoning_output_tokens: None,
            total_tokens: None,
            service_tier: None,
            accuracy: UsageAccuracy::Exact,
        }
    }
}

impl UsageRecord {
    pub fn new(
        input_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        cache_write_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_output_tokens: Option<u64>,
        total_tokens: Option<u64>,
        service_tier: Option<String>,
    ) -> Result<Self, ProviderError> {
        if let Some(value) = service_tier.as_deref() {
            validate_provider_text("service tier", value, MAX_SERVICE_TIER_BYTES)?;
        }
        Ok(Self {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            service_tier,
            accuracy: UsageAccuracy::Exact,
        })
    }

    #[must_use]
    pub fn estimated(input_tokens: u32, output_tokens: u32) -> Self {
        let input_tokens = u64::from(input_tokens);
        let output_tokens = u64::from(output_tokens);
        Self {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: input_tokens.checked_add(output_tokens),
            accuracy: UsageAccuracy::Estimated,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_accuracy(mut self, accuracy: UsageAccuracy) -> Self {
        self.accuracy = accuracy;
        self
    }

    #[must_use]
    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    #[must_use]
    pub const fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }

    #[must_use]
    pub const fn cache_write_input_tokens(&self) -> Option<u64> {
        self.cache_write_input_tokens
    }

    #[must_use]
    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    #[must_use]
    pub const fn reasoning_output_tokens(&self) -> Option<u64> {
        self.reasoning_output_tokens
    }

    #[must_use]
    pub const fn total_tokens(&self) -> Option<u64> {
        self.total_tokens
    }

    #[must_use]
    pub fn service_tier(&self) -> Option<&str> {
        self.service_tier.as_deref()
    }

    #[must_use]
    pub const fn accuracy(&self) -> UsageAccuracy {
        self.accuracy
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderToolCall {
    call_id: String,
    tool: String,
    arguments_json: String,
}

impl ProviderToolCall {
    pub fn new(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments_json: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let call = Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments_json: arguments_json.into(),
        };
        validate_provider_text(
            "Provider Tool call ID",
            &call.call_id,
            MAX_PROVIDER_ID_BYTES,
        )?;
        validate_provider_text("Provider Tool name", &call.tool, MAX_PROVIDER_ID_BYTES)?;
        validate_provider_text(
            "Provider Tool arguments",
            &call.arguments_json,
            MAX_PROVIDER_TOOL_ARGUMENT_BYTES,
        )?;
        Ok(call)
    }

    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    #[must_use]
    pub fn arguments_json(&self) -> &str {
        &self.arguments_json
    }
}

impl fmt::Debug for ProviderToolCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderToolCall")
            .field("call_id_bytes", &self.call_id.len())
            .field("tool_bytes", &self.tool.len())
            .field("arguments_bytes", &self.arguments_json.len())
            .finish()
    }
}

pub struct ProviderToolOutput {
    call_id: String,
    output: String,
}

impl ProviderToolOutput {
    pub(crate) fn new(call_id: String, output: String) -> Result<Self, ProviderError> {
        validate_provider_text("Provider Tool call ID", &call_id, MAX_PROVIDER_ID_BYTES)?;
        if output.len() > MAX_PROVIDER_TOOL_OUTPUT_BYTES {
            return Err(ProviderError::InvalidResponse(
                "Provider Tool output exceeds the byte limit",
            ));
        }
        Ok(Self { call_id, output })
    }

    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    #[must_use]
    pub fn output(&self) -> &str {
        &self.output
    }
}

impl fmt::Debug for ProviderToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderToolOutput")
            .field("call_id_bytes", &self.call_id.len())
            .field("output_bytes", &self.output.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    FunctionCall(ProviderToolCall),
    Completed(UsageRecord),
}

impl fmt::Debug for ProviderEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta(delta) => formatter
                .debug_struct("TextDelta")
                .field("bytes", &delta.len())
                .finish(),
            Self::FunctionCall(call) => formatter.debug_tuple("FunctionCall").field(call).finish(),
            Self::Completed(usage) => formatter.debug_tuple("Completed").field(usage).finish(),
        }
    }
}

pub trait ProviderRuntime {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        None
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        None
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError>;

    fn continue_after_tool(
        &mut self,
        _request: &ProviderRequest,
        _output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        Err(ProviderError::InvalidRequest(
            "Provider does not support Tool continuation",
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeterministicProvider {
    prefix: String,
    max_chunk_bytes: usize,
}

impl Default for DeterministicProvider {
    fn default() -> Self {
        Self {
            prefix: "simulated: ".to_owned(),
            max_chunk_bytes: 8,
        }
    }
}

impl DeterministicProvider {
    pub fn new(prefix: impl Into<String>, max_chunk_bytes: usize) -> Result<Self, ProviderError> {
        let prefix = prefix.into();
        if prefix.len() > MAX_PROVIDER_ID_BYTES {
            return Err(ProviderError::InvalidConfiguration(
                "simulator prefix is too long",
            ));
        }
        if max_chunk_bytes == 0 {
            return Err(ProviderError::InvalidConfiguration(
                "simulator chunk size must be nonzero",
            ));
        }
        Ok(Self {
            prefix,
            max_chunk_bytes,
        })
    }
}

impl ProviderRuntime for DeterministicProvider {
    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.input.trim().is_empty() {
            return Err(ProviderError::InvalidRequest("input cannot be empty"));
        }
        let output = format!("{}{input}", self.prefix, input = request.input);
        let mut events = split_utf8_chunks(&output, self.max_chunk_bytes)
            .into_iter()
            .map(ProviderEvent::TextDelta)
            .collect::<Vec<_>>();
        events.push(ProviderEvent::Completed(UsageRecord::estimated(
            approximate_tokens(&request.input),
            approximate_tokens(&output),
        )));
        Ok(events)
    }
}

fn split_utf8_chunks(text: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = start.saturating_add(max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        chunks.push(text[start..end].to_owned());
        start = end;
    }
    chunks
}

fn approximate_tokens(text: &str) -> u32 {
    let words = text.split_whitespace().count().max(1);
    u32::try_from(words).unwrap_or(u32::MAX)
}

fn validate_provider_id(field: &'static str, value: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::InvalidConfiguration(field));
    }
    if value.trim() != value {
        return Err(ProviderError::InvalidConfiguration(
            "provider identifiers cannot have surrounding whitespace",
        ));
    }
    if value.len() > MAX_PROVIDER_ID_BYTES || value.chars().any(char::is_control) {
        return Err(ProviderError::InvalidConfiguration(
            "provider identifier is too long",
        ));
    }
    Ok(())
}

fn validate_snapshot_value(field: &'static str, value: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > MAX_PROVIDER_ID_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidConfiguration(field));
    }
    Ok(())
}

fn normalize_provider_origin(
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<String, ProviderError> {
    validate_snapshot_value("provider origin", value)?;
    let url = Url::parse(value).map_err(|_| {
        ProviderError::InvalidConfiguration("provider origin must be an absolute HTTP(S) URL")
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidConfiguration(
            "provider origin contains unsupported URL components",
        ));
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() == "http" && (!loopback || !allow_insecure_loopback) {
        return Err(ProviderError::InvalidConfiguration(
            "plain HTTP requires explicit loopback permission",
        ));
    }
    if !loopback && allow_insecure_loopback {
        return Err(ProviderError::InvalidConfiguration(
            "loopback permission is invalid for a remote provider origin",
        ));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn normalize_optional_route(route: Option<String>) -> Result<Option<String>, ProviderError> {
    route
        .map(|route| normalize_provider_route(&route))
        .transpose()
}

fn normalize_provider_route(route: &str) -> Result<String, ProviderError> {
    validate_snapshot_value("provider route", route)?;
    if route.contains('?')
        || route.contains('#')
        || route.contains("://")
        || route.contains('\\')
        || route
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(ProviderError::InvalidConfiguration(
            "provider route contains unsupported components",
        ));
    }
    let trimmed = route.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(ProviderError::InvalidConfiguration(
            "provider route cannot be the origin root",
        ));
    }
    Ok(format!("/{trimmed}"))
}

fn fingerprint_profile_snapshot(snapshot: &ProviderProfileSnapshot) -> u64 {
    let mut hash = Fingerprint::new(1);
    hash.string(&snapshot.profile);
    hash.string(&snapshot.template);
    hash.optional_string(snapshot.credential_reference.as_deref());
    hash.optional_string(snapshot.base_url.as_deref());
    hash.optional_string(snapshot.responses_route.as_deref());
    hash.optional_string(snapshot.chat_completions_route.as_deref());
    hash.optional_string(snapshot.messages_route.as_deref());
    hash.optional_string(snapshot.models_route.as_deref());
    hash.usize(snapshot.dialects.len());
    for dialect in &snapshot.dialects {
        hash.byte(match dialect {
            ProviderDialect::Responses => 1,
            ProviderDialect::ChatCompletions => 2,
            ProviderDialect::Messages => 3,
        });
    }
    hash.byte(match snapshot.pricing_source {
        None => 0,
        Some(ProviderPricingSource::Unknown) => 1,
        Some(ProviderPricingSource::Template) => 2,
        Some(ProviderPricingSource::Manual) => 3,
        Some(ProviderPricingSource::ProviderReported) => 4,
    });
    hash.byte(u8::from(snapshot.allow_insecure_loopback));
    hash.finish()
}

fn fingerprint_provider_epoch(
    profile: &str,
    model: &str,
    snapshot: Option<&ProviderProfileSnapshot>,
    dialect: Option<ProviderDialect>,
) -> u64 {
    let mut hash = Fingerprint::new(1);
    hash.string(profile);
    hash.string(model);
    match snapshot {
        None => hash.byte(0),
        Some(snapshot) => {
            hash.byte(1);
            hash.bytes(&snapshot.fingerprint().to_le_bytes());
        }
    }
    if let Some(dialect) = dialect {
        hash.byte(2);
        hash.byte(match dialect {
            ProviderDialect::Responses => 1,
            ProviderDialect::ChatCompletions => 2,
            ProviderDialect::Messages => 3,
        });
    }
    hash.finish()
}

struct Fingerprint(u64);

impl Fingerprint {
    fn new(schema: u8) -> Self {
        let mut hash = Self(0xcbf2_9ce4_8422_2325_u64);
        hash.byte(schema);
        hash
    }

    fn byte(&mut self, value: u8) {
        self.bytes(&[value]);
    }

    fn usize(&mut self, value: usize) {
        self.bytes(&(value as u64).to_le_bytes());
    }

    fn string(&mut self, value: &str) {
        self.bytes(&(value.len() as u64).to_le_bytes());
        self.bytes(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            None => self.byte(0),
            Some(value) => {
                self.byte(1);
                self.string(value);
            }
        }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    const fn finish(self) -> u64 {
        self.0
    }
}

fn validate_provider_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ProviderError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(ProviderError::InvalidResponse(field));
    }
    Ok(())
}

#[derive(Clone, Eq, PartialEq)]
pub enum ProviderError {
    InvalidConfiguration(&'static str),
    InvalidRequest(&'static str),
    InvalidResponse(&'static str),
    Unavailable { diagnostic_bytes: usize },
}

impl ProviderError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::Unavailable {
            diagnostic_bytes: message.len().min(MAX_PROVIDER_ERROR_BYTES),
        }
    }
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => formatter
                .debug_tuple("InvalidConfiguration")
                .field(reason)
                .finish(),
            Self::InvalidRequest(reason) => formatter
                .debug_tuple("InvalidRequest")
                .field(reason)
                .finish(),
            Self::InvalidResponse(reason) => formatter
                .debug_tuple("InvalidResponse")
                .field(reason)
                .finish(),
            Self::Unavailable { diagnostic_bytes } => formatter
                .debug_struct("Unavailable")
                .field("message_bytes", diagnostic_bytes)
                .finish(),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => {
                write!(formatter, "invalid provider configuration: {reason}")
            }
            Self::InvalidRequest(reason) => write!(formatter, "invalid provider request: {reason}"),
            Self::InvalidResponse(reason) => {
                write!(formatter, "invalid provider response: {reason}")
            }
            Self::Unavailable { .. } => formatter.write_str("provider unavailable"),
        }
    }
}

impl Error for ProviderError {}
