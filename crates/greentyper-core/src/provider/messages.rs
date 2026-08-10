//! Bounded Anthropic Messages SSE decoding into dialect-scoped facts.

use std::error::Error;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use super::sse::{SseError, SseEvent, SseLimits, SseParser};
use super::{ProviderError, ProviderEvent, ProviderToolCall, UsageRecord};
use crate::config::MAX_OUTPUT_BYTES;

const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 4096;
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_DEPTH: usize = 64;
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Clone, Eq, PartialEq)]
pub struct MessagesEvent {
    pub kind: MessagesEventKind,
}

impl MessagesEvent {
    #[must_use]
    pub const fn new(kind: MessagesEventKind) -> Self {
        Self { kind }
    }
}

impl fmt::Debug for MessagesEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesEvent")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum MessagesEventKind {
    TextDelta {
        message_id: String,
        block_index: u32,
        delta: String,
    },
    FunctionCall {
        message_id: String,
        block_index: u32,
        call_id: String,
        name: String,
        arguments_json: String,
    },
    Completed {
        message_id: String,
        usage: MessagesUsage,
    },
    Incomplete {
        message_id: String,
        reason: String,
    },
    Error {
        diagnostic_bytes: usize,
    },
}

impl fmt::Debug for MessagesEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta {
                message_id,
                block_index,
                delta,
            } => formatter
                .debug_struct("TextDelta")
                .field("message_id_bytes", &message_id.len())
                .field("block_index", block_index)
                .field("delta_bytes", &delta.len())
                .finish(),
            Self::FunctionCall {
                message_id,
                block_index,
                call_id,
                name,
                arguments_json,
            } => formatter
                .debug_struct("FunctionCall")
                .field("message_id_bytes", &message_id.len())
                .field("block_index", block_index)
                .field("call_id_bytes", &call_id.len())
                .field("name_bytes", &name.len())
                .field("arguments_bytes", &arguments_json.len())
                .finish(),
            Self::Completed { message_id, usage } => formatter
                .debug_struct("Completed")
                .field("message_id_bytes", &message_id.len())
                .field("usage", usage)
                .finish(),
            Self::Incomplete { message_id, reason } => formatter
                .debug_struct("Incomplete")
                .field("message_id_bytes", &message_id.len())
                .field("reason_bytes", &reason.len())
                .finish(),
            Self::Error { diagnostic_bytes } => formatter
                .debug_struct("Error")
                .field("diagnostic_bytes", diagnostic_bytes)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessagesUsage {
    pub uncached_input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

pub struct MessagesSseDecoder {
    sse: Option<SseParser>,
    processed_sse_events: usize,
    max_output_bytes: usize,
    total_text_bytes: usize,
    message_id: Option<String>,
    model: Option<String>,
    usage: MessagesUsage,
    next_block_index: u32,
    active_block: Option<ActiveBlock>,
    tool_call_seen: bool,
    message_delta_seen: bool,
    incomplete: bool,
    terminal: bool,
    poisoned: bool,
    events: Vec<MessagesEvent>,
}

impl fmt::Debug for MessagesSseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesSseDecoder")
            .field("sse_present", &self.sse.is_some())
            .field("processed_sse_events", &self.processed_sse_events)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("total_text_bytes", &self.total_text_bytes)
            .field(
                "message_id_bytes",
                &self.message_id.as_ref().map(String::len),
            )
            .field("model_bytes", &self.model.as_ref().map(String::len))
            .field("next_block_index", &self.next_block_index)
            .field("has_active_block", &self.active_block.is_some())
            .field("tool_call_seen", &self.tool_call_seen)
            .field("message_delta_seen", &self.message_delta_seen)
            .field("incomplete", &self.incomplete)
            .field("terminal", &self.terminal)
            .field("poisoned", &self.poisoned)
            .field("event_count", &self.events.len())
            .finish()
    }
}

impl MessagesSseDecoder {
    pub fn new(max_output_bytes: usize) -> Result<Self, MessagesError> {
        if max_output_bytes == 0 || max_output_bytes > MAX_OUTPUT_BYTES as usize {
            return Err(MessagesError::InvalidLimits);
        }
        let limits =
            SseLimits::new(MAX_STREAM_BYTES, MAX_LINE_BYTES).map_err(MessagesError::Sse)?;
        Ok(Self {
            sse: Some(SseParser::new(limits)),
            processed_sse_events: 0,
            max_output_bytes,
            total_text_bytes: 0,
            message_id: None,
            model: None,
            usage: MessagesUsage::default(),
            next_block_index: 0,
            active_block: None,
            tool_call_seen: false,
            message_delta_seen: false,
            incomplete: false,
            terminal: false,
            poisoned: false,
            events: Vec::new(),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), MessagesError> {
        if self.poisoned {
            return Err(MessagesError::Poisoned);
        }
        let result = self.push_inner(chunk);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    #[must_use]
    pub fn events(&self) -> &[MessagesEvent] {
        &self.events
    }

    pub fn finish(mut self) -> Result<Vec<MessagesEvent>, MessagesError> {
        if self.poisoned {
            return Err(MessagesError::Poisoned);
        }
        let sse = self.sse.take().ok_or(MessagesError::Poisoned)?;
        let framed = sse.finish().map_err(MessagesError::Sse)?;
        self.process_framed_events(&framed)?;
        if !self.terminal {
            return Err(MessagesError::IncompleteStream);
        }
        Ok(self.events)
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<(), MessagesError> {
        let sse = self.sse.as_mut().ok_or(MessagesError::Poisoned)?;
        sse.push(chunk).map_err(MessagesError::Sse)?;
        let framed = sse.take_events();
        self.process_framed_events(&framed)
    }

    fn process_framed_events(&mut self, framed: &[SseEvent]) -> Result<(), MessagesError> {
        for event in framed {
            self.process_event(event)?;
            self.processed_sse_events = self
                .processed_sse_events
                .checked_add(1)
                .ok_or(MessagesError::EventLimitExceeded)?;
            if self.processed_sse_events > MAX_EVENTS {
                return Err(MessagesError::EventLimitExceeded);
            }
        }
        Ok(())
    }

    fn process_event(&mut self, event: &SseEvent) -> Result<(), MessagesError> {
        if self.terminal {
            return Err(MessagesError::EventAfterTerminal);
        }
        let envelope: WireEnvelope = decode_json(event.data())?;
        if event.event() != envelope.kind {
            return Err(MessagesError::EventTypeMismatch);
        }
        match envelope.kind.as_str() {
            "message_start" => self.start_message(decode_json(event.data())?),
            "content_block_start" => self.start_block(decode_json(event.data())?),
            "content_block_delta" => self.apply_block_delta(decode_json(event.data())?),
            "content_block_stop" => self.stop_block(decode_json(event.data())?),
            "message_delta" => self.apply_message_delta(decode_json(event.data())?),
            "message_stop" => self.stop_message(decode_json(event.data())?),
            "ping" => Ok(()),
            "error" => self.observe_error(decode_json(event.data())?),
            _ => Err(MessagesError::UnsupportedEvent),
        }
    }

    fn start_message(&mut self, event: WireMessageStart) -> Result<(), MessagesError> {
        if event.kind != "message_start"
            || self.message_id.is_some()
            || event.message.kind != "message"
            || event.message.role != "assistant"
            || !event.message.content.is_empty()
            || event.message.stop_reason.is_some()
            || event.message.stop_sequence.is_some()
        {
            return Err(MessagesError::InvalidTransition);
        }
        validate_identifier(&event.message.id)?;
        validate_identifier(&event.message.model)?;
        self.message_id = Some(event.message.id);
        self.model = Some(event.message.model);
        self.usage = event.message.usage.into();
        refresh_usage_totals(&mut self.usage)?;
        Ok(())
    }

    fn start_block(&mut self, event: WireContentBlockStart) -> Result<(), MessagesError> {
        self.require_message_id()?;
        if event.kind != "content_block_start"
            || self.message_delta_seen
            || self.active_block.is_some()
            || event.index != self.next_block_index
        {
            return Err(MessagesError::InvalidTransition);
        }
        self.active_block = Some(match event.content_block {
            WireContentBlock::Text { text } if text.is_empty() => {
                ActiveBlock::Text { index: event.index }
            }
            WireContentBlock::ToolUse { id, name, input } => {
                if self.tool_call_seen || !input.as_object().is_some_and(serde_json::Map::is_empty)
                {
                    return Err(MessagesError::UnsupportedToolCall);
                }
                validate_identifier(&id)?;
                validate_tool_name(&name)?;
                self.tool_call_seen = true;
                ActiveBlock::ToolUse {
                    index: event.index,
                    call_id: id,
                    name,
                    arguments: String::new(),
                }
            }
            _ => return Err(MessagesError::UnsupportedContentBlock),
        });
        Ok(())
    }

    fn apply_block_delta(&mut self, event: WireContentBlockDelta) -> Result<(), MessagesError> {
        if event.kind != "content_block_delta" || self.message_delta_seen {
            return Err(MessagesError::InvalidTransition);
        }
        let active = self
            .active_block
            .as_mut()
            .ok_or(MessagesError::InvalidTransition)?;
        if active.index() != event.index {
            return Err(MessagesError::InvalidTransition);
        }
        match (active, event.delta) {
            (ActiveBlock::Text { .. }, WireContentDelta::TextDelta { text }) => {
                if text.is_empty() {
                    return Err(MessagesError::InvalidText);
                }
                self.total_text_bytes = self
                    .total_text_bytes
                    .checked_add(text.len())
                    .ok_or(MessagesError::OutputLimitExceeded)?;
                if self.total_text_bytes > self.max_output_bytes {
                    return Err(MessagesError::OutputLimitExceeded);
                }
                let message_id = self.require_message_id()?.to_owned();
                self.events
                    .push(MessagesEvent::new(MessagesEventKind::TextDelta {
                        message_id,
                        block_index: event.index,
                        delta: text,
                    }));
                Ok(())
            }
            (
                ActiveBlock::ToolUse { arguments, .. },
                WireContentDelta::InputJsonDelta { partial_json },
            ) => {
                let next = arguments
                    .len()
                    .checked_add(partial_json.len())
                    .ok_or(MessagesError::ArgumentLimitExceeded)?;
                if next > MAX_ARGUMENT_BYTES {
                    return Err(MessagesError::ArgumentLimitExceeded);
                }
                arguments.push_str(&partial_json);
                Ok(())
            }
            _ => Err(MessagesError::InvalidTransition),
        }
    }

    fn stop_block(&mut self, event: WireContentBlockStop) -> Result<(), MessagesError> {
        if event.kind != "content_block_stop" || self.message_delta_seen {
            return Err(MessagesError::InvalidTransition);
        }
        let active = self
            .active_block
            .take()
            .ok_or(MessagesError::InvalidTransition)?;
        if active.index() != event.index {
            return Err(MessagesError::InvalidTransition);
        }
        if let ActiveBlock::ToolUse {
            call_id,
            name,
            arguments,
            ..
        } = active
        {
            let message_id = self.require_message_id()?.to_owned();
            let arguments_json = canonical_arguments(if arguments.is_empty() {
                "{}"
            } else {
                &arguments
            })?;
            self.events
                .push(MessagesEvent::new(MessagesEventKind::FunctionCall {
                    message_id,
                    block_index: event.index,
                    call_id,
                    name,
                    arguments_json,
                }));
        }
        self.next_block_index = self
            .next_block_index
            .checked_add(1)
            .ok_or(MessagesError::InvalidTransition)?;
        Ok(())
    }

    fn apply_message_delta(&mut self, event: WireMessageDelta) -> Result<(), MessagesError> {
        let message_id = self.require_message_id()?.to_owned();
        if event.kind != "message_delta"
            || self.active_block.is_some()
            || self.message_delta_seen
            || event
                .delta
                .stop_sequence
                .as_deref()
                .is_some_and(str::is_empty)
        {
            return Err(MessagesError::InvalidTransition);
        }
        self.observe_output_usage(event.usage.output_tokens)?;
        let reason = event
            .delta
            .stop_reason
            .ok_or(MessagesError::InvalidTransition)?;
        match reason.as_str() {
            "end_turn" | "stop_sequence" if !self.tool_call_seen => {}
            "tool_use" if self.tool_call_seen => {}
            "max_tokens" | "model_context_window_exceeded" => {
                self.incomplete = true;
                self.events
                    .push(MessagesEvent::new(MessagesEventKind::Incomplete {
                        message_id,
                        reason,
                    }));
            }
            "end_turn" | "stop_sequence" | "tool_use" => {
                return Err(MessagesError::InvalidTransition);
            }
            _ => return Err(MessagesError::UnsupportedStopReason),
        }
        self.message_delta_seen = true;
        Ok(())
    }

    fn stop_message(&mut self, event: WireMessageStop) -> Result<(), MessagesError> {
        if event.kind != "message_stop" || !self.message_delta_seen || self.active_block.is_some() {
            return Err(MessagesError::InvalidTransition);
        }
        if !self.incomplete {
            let message_id = self.require_message_id()?.to_owned();
            self.usage.total_tokens = total_tokens(&self.usage)?;
            self.events
                .push(MessagesEvent::new(MessagesEventKind::Completed {
                    message_id,
                    usage: self.usage,
                }));
        }
        self.terminal = true;
        Ok(())
    }

    fn observe_error(&mut self, event: WireErrorEvent) -> Result<(), MessagesError> {
        if event.kind != "error"
            || event.error.kind.trim().is_empty()
            || event.error.message.trim().is_empty()
            || event.error.kind.len() > MAX_IDENTIFIER_BYTES
            || event.error.message.len() > MAX_ERROR_BYTES
        {
            return Err(MessagesError::MalformedEvent);
        }
        self.events
            .push(MessagesEvent::new(MessagesEventKind::Error {
                diagnostic_bytes: event.error.message.len(),
            }));
        self.terminal = true;
        Ok(())
    }

    fn observe_output_usage(&mut self, output_tokens: Option<u64>) -> Result<(), MessagesError> {
        if let (Some(previous), Some(next)) = (self.usage.output_tokens, output_tokens)
            && next < previous
        {
            return Err(MessagesError::InvalidUsage);
        }
        if output_tokens.is_some() {
            self.usage.output_tokens = output_tokens;
        }
        refresh_usage_totals(&mut self.usage)
    }

    fn require_message_id(&self) -> Result<&str, MessagesError> {
        self.message_id
            .as_deref()
            .ok_or(MessagesError::InvalidTransition)
    }
}

/// Converts validated Messages facts into Provider Runtime facts.
///
/// Message identifiers and failure details stay inside the dialect adapter.
/// Runtime callers receive bounded text, one canonical Tool call, normalized
/// usage, or a fixed failure classification.
pub fn normalize_messages_events(
    events: &[MessagesEvent],
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut message_id = None;
    let mut completed = false;
    let mut normalized = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if completed {
            return Err(ProviderError::InvalidResponse(
                "Messages event followed completion",
            ));
        }
        match &event.kind {
            MessagesEventKind::TextDelta {
                message_id: actual,
                delta,
                ..
            } => {
                observe_normalized_identity(&mut message_id, actual)?;
                normalized.push(ProviderEvent::TextDelta(delta.clone()));
            }
            MessagesEventKind::FunctionCall {
                message_id: actual,
                call_id,
                name,
                arguments_json,
                ..
            } => {
                observe_normalized_identity(&mut message_id, actual)?;
                normalized.push(ProviderEvent::FunctionCall(ProviderToolCall::new(
                    call_id.clone(),
                    name.clone(),
                    arguments_json.clone(),
                )?));
            }
            MessagesEventKind::Completed {
                message_id: actual,
                usage,
            } => {
                observe_normalized_identity(&mut message_id, actual)?;
                if index + 1 != events.len() {
                    return Err(ProviderError::InvalidResponse(
                        "Messages completion is not terminal",
                    ));
                }
                let input_tokens = total_input_tokens(usage).map_err(|_| {
                    ProviderError::InvalidResponse("Messages usage accounting overflowed")
                })?;
                let total_tokens = total_tokens(usage).map_err(|_| {
                    ProviderError::InvalidResponse("Messages usage accounting overflowed")
                })?;
                if usage.total_tokens != total_tokens {
                    return Err(ProviderError::InvalidResponse(
                        "Messages usage total is inconsistent",
                    ));
                }
                normalized.push(ProviderEvent::Completed(UsageRecord::new(
                    input_tokens,
                    usage.cached_input_tokens,
                    usage.cache_write_input_tokens,
                    usage.output_tokens,
                    None,
                    total_tokens,
                    None,
                )?));
                completed = true;
            }
            MessagesEventKind::Incomplete { .. } => {
                return Err(ProviderError::unavailable(
                    "Messages request ended incomplete",
                ));
            }
            MessagesEventKind::Error { .. } => {
                return Err(ProviderError::unavailable(
                    "Messages stream reported an error",
                ));
            }
        }
    }
    if message_id.is_none() || !completed {
        return Err(ProviderError::InvalidResponse(
            "Messages stream has no complete terminal",
        ));
    }
    Ok(normalized)
}

fn observe_normalized_identity(
    expected: &mut Option<String>,
    actual: &str,
) -> Result<(), ProviderError> {
    match expected.as_deref() {
        Some(expected) if expected != actual => Err(ProviderError::InvalidResponse(
            "Messages stream identity changed",
        )),
        Some(_) => Ok(()),
        None => {
            *expected = Some(actual.to_owned());
            Ok(())
        }
    }
}

enum ActiveBlock {
    Text {
        index: u32,
    },
    ToolUse {
        index: u32,
        call_id: String,
        name: String,
        arguments: String,
    },
}

impl ActiveBlock {
    const fn index(&self) -> u32 {
        match self {
            Self::Text { index } | Self::ToolUse { index, .. } => *index,
        }
    }
}

#[derive(Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct WireMessageStart {
    #[serde(rename = "type")]
    kind: String,
    message: WireMessage,
}

#[derive(Deserialize)]
struct WireMessage {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    role: String,
    #[serde(default)]
    content: Vec<Value>,
    model: String,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireContentBlockStart {
    #[serde(rename = "type")]
    kind: String,
    index: u32,
    content_block: WireContentBlock,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
struct WireContentBlockDelta {
    #[serde(rename = "type")]
    kind: String,
    index: u32,
    delta: WireContentDelta,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
struct WireContentBlockStop {
    #[serde(rename = "type")]
    kind: String,
    index: u32,
}

#[derive(Deserialize)]
struct WireMessageDelta {
    #[serde(rename = "type")]
    kind: String,
    delta: WireStopDelta,
    usage: WireUsage,
}

#[derive(Deserialize)]
struct WireStopDelta {
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
}

#[derive(Deserialize)]
struct WireMessageStop {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct WireErrorEvent {
    #[serde(rename = "type")]
    kind: String,
    error: WireError,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl From<WireUsage> for MessagesUsage {
    fn from(value: WireUsage) -> Self {
        Self {
            uncached_input_tokens: value.input_tokens,
            cached_input_tokens: value.cache_read_input_tokens,
            cache_write_input_tokens: value.cache_creation_input_tokens,
            output_tokens: value.output_tokens,
            total_tokens: None,
        }
    }
}

fn refresh_usage_totals(usage: &mut MessagesUsage) -> Result<(), MessagesError> {
    usage.total_tokens = total_tokens(usage)?;
    Ok(())
}

fn total_input_tokens(usage: &MessagesUsage) -> Result<Option<u64>, MessagesError> {
    match (
        usage.uncached_input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
    ) {
        (Some(uncached), cached, written) => uncached
            .checked_add(cached.unwrap_or(0))
            .and_then(|value| value.checked_add(written.unwrap_or(0)))
            .map(Some)
            .ok_or(MessagesError::InvalidUsage),
        (None, _, _) => Ok(None),
    }
}

fn total_tokens(usage: &MessagesUsage) -> Result<Option<u64>, MessagesError> {
    match (total_input_tokens(usage)?, usage.output_tokens) {
        (Some(input), Some(output)) => input
            .checked_add(output)
            .map(Some)
            .ok_or(MessagesError::InvalidUsage),
        _ => Ok(None),
    }
}

fn decode_json<T>(data: &str) -> Result<T, MessagesError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(data).map_err(|_| MessagesError::MalformedEvent)
}

fn canonical_arguments(arguments: &str) -> Result<String, MessagesError> {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(MessagesError::ArgumentLimitExceeded);
    }
    let value: Value = decode_json(arguments)?;
    if !value.is_object() {
        return Err(MessagesError::ArgumentsNotObject);
    }
    validate_argument_depth(&value)?;
    let canonical = serde_json::to_string(&value).map_err(|_| MessagesError::MalformedEvent)?;
    if canonical.len() > MAX_ARGUMENT_BYTES {
        return Err(MessagesError::ArgumentLimitExceeded);
    }
    Ok(canonical)
}

fn validate_argument_depth(value: &Value) -> Result<(), MessagesError> {
    let mut pending = vec![(value, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_ARGUMENT_DEPTH {
            return Err(MessagesError::ArgumentNestingExceeded);
        }
        match value {
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<(), MessagesError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(MessagesError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), MessagesError> {
    if value.is_empty()
        || value.len() > MAX_TOOL_NAME_BYTES
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err(MessagesError::InvalidToolName);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessagesError {
    InvalidLimits,
    Sse(SseError),
    Poisoned,
    MalformedEvent,
    EventTypeMismatch,
    UnsupportedEvent,
    EventLimitExceeded,
    EventAfterTerminal,
    UnsupportedContentBlock,
    UnsupportedStopReason,
    UnsupportedToolCall,
    InvalidTransition,
    InvalidIdentifier,
    InvalidToolName,
    InvalidText,
    InvalidUsage,
    OutputLimitExceeded,
    ArgumentLimitExceeded,
    ArgumentNestingExceeded,
    ArgumentsNotObject,
    IncompleteStream,
}

impl fmt::Display for MessagesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "invalid Messages decoder limits",
            Self::Sse(_) => "invalid Messages SSE framing",
            Self::Poisoned => "Messages decoder must be discarded after a protocol error",
            Self::MalformedEvent => "malformed Messages event",
            Self::EventTypeMismatch => "Messages SSE event type does not match its payload",
            Self::UnsupportedEvent => "unsupported Messages SSE event",
            Self::EventLimitExceeded => "Messages stream exceeds its event limit",
            Self::EventAfterTerminal => "Messages event follows its terminal",
            Self::UnsupportedContentBlock => "unsupported Messages content block",
            Self::UnsupportedStopReason => "unsupported Messages stop reason",
            Self::UnsupportedToolCall => "unsupported Messages Tool call",
            Self::InvalidTransition => "invalid Messages event transition",
            Self::InvalidIdentifier => "invalid Messages identifier",
            Self::InvalidToolName => "invalid Messages Tool name",
            Self::InvalidText => "invalid Messages text field",
            Self::InvalidUsage => "invalid Messages usage",
            Self::OutputLimitExceeded => "Messages text exceeds the configured output limit",
            Self::ArgumentLimitExceeded => "Messages Tool input exceeds its byte limit",
            Self::ArgumentNestingExceeded => "Messages Tool input exceeds its nesting limit",
            Self::ArgumentsNotObject => "Messages Tool input must be a JSON object",
            Self::IncompleteStream => "Messages stream ended before its terminal event",
        })
    }
}

impl Error for MessagesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sse(error) => Some(error),
            _ => None,
        }
    }
}
