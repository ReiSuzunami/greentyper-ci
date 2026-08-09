//! Provider-neutral Turn requests, canonical stream events, and a deterministic simulator.

use std::error::Error;
use std::fmt;

use crate::config::ConfigEpoch;
use crate::model::{ProviderEpochId, ThreadId, TurnId};

pub const MAX_PROVIDER_ID_BYTES: usize = 512;
pub const MAX_PROVIDER_ERROR_BYTES: usize = 4096;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRequest {
    pub thread: ThreadId,
    pub turn: TurnId,
    pub config: ConfigEpoch,
    pub provider: ProviderEpoch,
    pub input: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageRecord {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    Completed(UsageRecord),
}

pub trait ProviderRuntime {
    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError>;
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
        events.push(ProviderEvent::Completed(UsageRecord {
            input_tokens: approximate_tokens(&request.input),
            output_tokens: approximate_tokens(&output),
        }));
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    InvalidConfiguration(&'static str),
    InvalidRequest(&'static str),
    Unavailable(String),
}

impl ProviderError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_PROVIDER_ERROR_BYTES {
            message.truncate(MAX_PROVIDER_ERROR_BYTES);
            while !message.is_char_boundary(message.len()) {
                message.pop();
            }
        }
        Self::Unavailable(message)
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(reason) => {
                write!(formatter, "invalid provider configuration: {reason}")
            }
            Self::InvalidRequest(reason) => write!(formatter, "invalid provider request: {reason}"),
            Self::Unavailable(message) => write!(formatter, "provider unavailable: {message}"),
        }
    }
}

impl Error for ProviderError {}
