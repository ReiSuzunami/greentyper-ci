//! Provider-neutral Turn requests, canonical stream events, and a deterministic simulator.

pub mod responses;
pub mod sse;

use std::error::Error;
use std::fmt;

use crate::config::ConfigEpoch;
use crate::model::{ProviderEpochId, ThreadId, TurnId};

pub const MAX_PROVIDER_ID_BYTES: usize = 512;
pub const MAX_PROVIDER_ERROR_BYTES: usize = 4096;
pub const MAX_PROVIDER_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_SERVICE_TIER_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEpoch {
    id: ProviderEpochId,
    profile: String,
    model: String,
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
        Ok(Self { id, profile, model })
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageRecord {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    service_tier: Option<String>,
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
            ..Self::default()
        }
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
    if value.len() > MAX_PROVIDER_ID_BYTES {
        return Err(ProviderError::InvalidConfiguration(
            "provider identifier is too long",
        ));
    }
    Ok(())
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
