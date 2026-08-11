//! Bounded OpenAI Chat Completions SSE decoding into dialect-scoped facts.

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
const MAX_SERVICE_TIER_BYTES: usize = 64;

#[derive(Clone, Eq, PartialEq)]
pub struct ChatCompletionsEvent {
    pub kind: ChatCompletionsEventKind,
}

impl ChatCompletionsEvent {
    #[must_use]
    pub const fn new(kind: ChatCompletionsEventKind) -> Self {
        Self { kind }
    }
}

impl fmt::Debug for ChatCompletionsEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatCompletionsEvent")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ChatCompletionsEventKind {
    TextDelta {
        completion_id: String,
        choice_index: u32,
        delta: String,
    },
    FunctionCall {
        completion_id: String,
        choice_index: u32,
        call_id: String,
        name: String,
        arguments_json: String,
    },
    Completed {
        completion_id: String,
        usage: Option<ChatCompletionsUsage>,
        service_tier: Option<String>,
    },
    Incomplete {
        completion_id: String,
        reason: String,
    },
}

impl fmt::Debug for ChatCompletionsEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TextDelta {
                completion_id,
                choice_index,
                delta,
            } => formatter
                .debug_struct("TextDelta")
                .field("completion_id_bytes", &completion_id.len())
                .field("choice_index", choice_index)
                .field("delta_bytes", &delta.len())
                .finish(),
            Self::FunctionCall {
                completion_id,
                choice_index,
                call_id,
                name,
                arguments_json,
            } => formatter
                .debug_struct("FunctionCall")
                .field("completion_id_bytes", &completion_id.len())
                .field("choice_index", choice_index)
                .field("call_id_bytes", &call_id.len())
                .field("name_bytes", &name.len())
                .field("arguments_bytes", &arguments_json.len())
                .finish(),
            Self::Completed {
                completion_id,
                usage,
                service_tier,
            } => formatter
                .debug_struct("Completed")
                .field("completion_id_bytes", &completion_id.len())
                .field("usage", usage)
                .field(
                    "service_tier_bytes",
                    &service_tier.as_ref().map(String::len),
                )
                .finish(),
            Self::Incomplete {
                completion_id,
                reason,
            } => formatter
                .debug_struct("Incomplete")
                .field("completion_id_bytes", &completion_id.len())
                .field("reason_bytes", &reason.len())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatCompletionsUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

pub struct ChatCompletionsSseDecoder {
    sse: Option<SseParser>,
    processed_sse_events: usize,
    max_output_bytes: usize,
    total_text_bytes: usize,
    completion_id: Option<String>,
    model: Option<String>,
    service_tier: Option<String>,
    role_seen: bool,
    choice_finished: bool,
    incomplete: bool,
    tool_call: Option<ToolCallState>,
    usage: Option<ChatCompletionsUsage>,
    terminal: bool,
    poisoned: bool,
    events: Vec<ChatCompletionsEvent>,
}

impl fmt::Debug for ChatCompletionsSseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatCompletionsSseDecoder")
            .field("sse_present", &self.sse.is_some())
            .field("processed_sse_events", &self.processed_sse_events)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("total_text_bytes", &self.total_text_bytes)
            .field(
                "completion_id_bytes",
                &self.completion_id.as_ref().map(String::len),
            )
            .field("model_bytes", &self.model.as_ref().map(String::len))
            .field("has_service_tier", &self.service_tier.is_some())
            .field("choice_finished", &self.choice_finished)
            .field("incomplete", &self.incomplete)
            .field("has_tool_call", &self.tool_call.is_some())
            .field("has_usage", &self.usage.is_some())
            .field("terminal", &self.terminal)
            .field("poisoned", &self.poisoned)
            .field("event_count", &self.events.len())
            .finish()
    }
}

impl ChatCompletionsSseDecoder {
    pub fn new(max_output_bytes: usize) -> Result<Self, ChatCompletionsError> {
        if max_output_bytes == 0 || max_output_bytes > MAX_OUTPUT_BYTES as usize {
            return Err(ChatCompletionsError::InvalidLimits);
        }
        let limits =
            SseLimits::new(MAX_STREAM_BYTES, MAX_LINE_BYTES).map_err(ChatCompletionsError::Sse)?;
        Ok(Self {
            sse: Some(SseParser::new(limits)),
            processed_sse_events: 0,
            max_output_bytes,
            total_text_bytes: 0,
            completion_id: None,
            model: None,
            service_tier: None,
            role_seen: false,
            choice_finished: false,
            incomplete: false,
            tool_call: None,
            usage: None,
            terminal: false,
            poisoned: false,
            events: Vec::new(),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), ChatCompletionsError> {
        if self.poisoned {
            return Err(ChatCompletionsError::Poisoned);
        }
        let result = self.push_inner(chunk);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    #[must_use]
    pub fn events(&self) -> &[ChatCompletionsEvent] {
        &self.events
    }

    #[must_use]
    pub const fn has_stream_progress(&self) -> bool {
        self.processed_sse_events != 0
    }

    pub fn finish(mut self) -> Result<Vec<ChatCompletionsEvent>, ChatCompletionsError> {
        if self.poisoned {
            return Err(ChatCompletionsError::Poisoned);
        }
        let sse = self.sse.take().ok_or(ChatCompletionsError::Poisoned)?;
        let framed = sse.finish().map_err(ChatCompletionsError::Sse)?;
        self.process_framed_events(&framed)?;
        if !self.terminal {
            return Err(ChatCompletionsError::IncompleteStream);
        }
        Ok(self.events)
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<(), ChatCompletionsError> {
        let sse = self.sse.as_mut().ok_or(ChatCompletionsError::Poisoned)?;
        sse.push(chunk).map_err(ChatCompletionsError::Sse)?;
        let framed = sse.take_events();
        self.process_framed_events(&framed)
    }

    fn process_framed_events(&mut self, framed: &[SseEvent]) -> Result<(), ChatCompletionsError> {
        for event in framed {
            self.process_event(event)?;
            self.processed_sse_events = self
                .processed_sse_events
                .checked_add(1)
                .ok_or(ChatCompletionsError::EventLimitExceeded)?;
            if self.processed_sse_events > MAX_EVENTS {
                return Err(ChatCompletionsError::EventLimitExceeded);
            }
        }
        Ok(())
    }

    fn process_event(&mut self, event: &SseEvent) -> Result<(), ChatCompletionsError> {
        if self.terminal {
            return Err(ChatCompletionsError::EventAfterTerminal);
        }
        if event.event() != "message" {
            return Err(ChatCompletionsError::EventTypeMismatch);
        }
        if event.data() == "[DONE]" {
            return self.complete_stream();
        }

        let chunk: WireChunk = decode_json(event.data())?;
        if chunk.object != "chat.completion.chunk" {
            return Err(ChatCompletionsError::MalformedEvent);
        }
        self.validate_identity(&chunk.id, &chunk.model)?;
        self.observe_service_tier(chunk.service_tier)?;

        if let Some(usage) = chunk.usage {
            if !chunk.choices.is_empty() || !self.choice_finished || self.usage.is_some() {
                return Err(ChatCompletionsError::InvalidTransition);
            }
            self.usage = Some(ChatCompletionsUsage::try_from(usage)?);
            return Ok(());
        }

        if chunk.choices.len() != 1 {
            return Err(ChatCompletionsError::UnsupportedChoice);
        }
        if self.choice_finished {
            return Err(ChatCompletionsError::InvalidTransition);
        }
        let choice = chunk
            .choices
            .into_iter()
            .next()
            .ok_or(ChatCompletionsError::UnsupportedChoice)?;
        if choice.index != 0 {
            return Err(ChatCompletionsError::UnsupportedChoice);
        }
        self.process_choice(choice)
    }

    fn validate_identity(
        &mut self,
        completion_id: &str,
        model: &str,
    ) -> Result<(), ChatCompletionsError> {
        validate_identifier(completion_id)?;
        validate_identifier(model)?;
        match (&self.completion_id, &self.model) {
            (None, None) => {
                self.completion_id = Some(completion_id.to_owned());
                self.model = Some(model.to_owned());
                Ok(())
            }
            (Some(expected_id), Some(expected_model))
                if expected_id == completion_id && expected_model == model =>
            {
                Ok(())
            }
            _ => Err(ChatCompletionsError::InvalidTransition),
        }
    }

    fn observe_service_tier(
        &mut self,
        service_tier: Option<String>,
    ) -> Result<(), ChatCompletionsError> {
        let Some(service_tier) = service_tier else {
            return Ok(());
        };
        validate_text(&service_tier, MAX_SERVICE_TIER_BYTES)?;
        match self.service_tier.as_deref() {
            Some(expected) if expected != service_tier => {
                Err(ChatCompletionsError::InvalidTransition)
            }
            Some(_) => Ok(()),
            None => {
                self.service_tier = Some(service_tier);
                Ok(())
            }
        }
    }

    fn process_choice(&mut self, choice: WireChoice) -> Result<(), ChatCompletionsError> {
        let WireDelta {
            role,
            content,
            refusal,
            function_call,
            tool_calls,
        } = choice.delta;
        if refusal.is_some() || function_call.is_some() {
            return Err(ChatCompletionsError::UnsupportedDelta);
        }
        if let Some(role) = role {
            if role != "assistant" || self.role_seen {
                return Err(ChatCompletionsError::InvalidTransition);
            }
            self.role_seen = true;
        }
        if content.is_some() && !tool_calls.is_empty() {
            return Err(ChatCompletionsError::UnsupportedDelta);
        }
        if tool_calls.len() > 1 {
            return Err(ChatCompletionsError::UnsupportedToolCall);
        }
        if choice.finish_reason.is_some() && (content.is_some() || !tool_calls.is_empty()) {
            return Err(ChatCompletionsError::InvalidTransition);
        }

        if let Some(content) = content {
            if content.is_empty() {
                return Err(ChatCompletionsError::InvalidText);
            }
            self.reserve_text(content.len())?;
            let completion_id = self.require_completion_id()?.to_owned();
            self.events.push(ChatCompletionsEvent::new(
                ChatCompletionsEventKind::TextDelta {
                    completion_id,
                    choice_index: choice.index,
                    delta: content,
                },
            ));
        }
        if let Some(tool_call) = tool_calls.into_iter().next() {
            self.observe_tool_call(tool_call)?;
        }
        if let Some(reason) = choice.finish_reason {
            self.finish_choice(&reason, choice.index)?;
        }
        Ok(())
    }

    fn observe_tool_call(&mut self, delta: WireToolCallDelta) -> Result<(), ChatCompletionsError> {
        if delta.index != 0 {
            return Err(ChatCompletionsError::UnsupportedToolCall);
        }
        if delta.kind.as_deref().is_some_and(|kind| kind != "function") {
            return Err(ChatCompletionsError::UnsupportedToolCall);
        }
        let state = self.tool_call.get_or_insert_with(ToolCallState::default);
        if let Some(call_id) = delta.id {
            validate_identifier(&call_id)?;
            set_once_or_match(&mut state.call_id, call_id)?;
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                validate_tool_name(&name)?;
                set_once_or_match(&mut state.name, name)?;
            }
            if let Some(arguments) = function.arguments {
                let next = state
                    .arguments
                    .len()
                    .checked_add(arguments.len())
                    .ok_or(ChatCompletionsError::ArgumentLimitExceeded)?;
                if next > MAX_ARGUMENT_BYTES {
                    return Err(ChatCompletionsError::ArgumentLimitExceeded);
                }
                state.arguments.push_str(&arguments);
            }
        }
        Ok(())
    }

    fn finish_choice(
        &mut self,
        reason: &str,
        choice_index: u32,
    ) -> Result<(), ChatCompletionsError> {
        let completion_id = self.require_completion_id()?.to_owned();
        match reason {
            "stop" => {
                if self.tool_call.is_some() {
                    return Err(ChatCompletionsError::InvalidTransition);
                }
            }
            "tool_calls" => {
                let state = self
                    .tool_call
                    .take()
                    .ok_or(ChatCompletionsError::InvalidTransition)?;
                let call_id = state
                    .call_id
                    .ok_or(ChatCompletionsError::InvalidTransition)?;
                let name = state.name.ok_or(ChatCompletionsError::InvalidTransition)?;
                let arguments_json = canonical_arguments(&state.arguments)?;
                self.events.push(ChatCompletionsEvent::new(
                    ChatCompletionsEventKind::FunctionCall {
                        completion_id,
                        choice_index,
                        call_id,
                        name,
                        arguments_json,
                    },
                ));
            }
            "length" | "content_filter" => {
                self.incomplete = true;
                self.events.push(ChatCompletionsEvent::new(
                    ChatCompletionsEventKind::Incomplete {
                        completion_id,
                        reason: reason.to_owned(),
                    },
                ));
            }
            _ => return Err(ChatCompletionsError::UnsupportedFinishReason),
        }
        self.choice_finished = true;
        Ok(())
    }

    fn complete_stream(&mut self) -> Result<(), ChatCompletionsError> {
        if !self.choice_finished {
            return Err(ChatCompletionsError::InvalidTransition);
        }
        if !self.incomplete {
            let completion_id = self.require_completion_id()?.to_owned();
            self.events.push(ChatCompletionsEvent::new(
                ChatCompletionsEventKind::Completed {
                    completion_id,
                    usage: self.usage,
                    service_tier: self.service_tier.clone(),
                },
            ));
        }
        self.terminal = true;
        Ok(())
    }

    fn reserve_text(&mut self, bytes: usize) -> Result<(), ChatCompletionsError> {
        self.total_text_bytes = self
            .total_text_bytes
            .checked_add(bytes)
            .ok_or(ChatCompletionsError::OutputLimitExceeded)?;
        if self.total_text_bytes > self.max_output_bytes {
            return Err(ChatCompletionsError::OutputLimitExceeded);
        }
        Ok(())
    }

    fn require_completion_id(&self) -> Result<&str, ChatCompletionsError> {
        self.completion_id
            .as_deref()
            .ok_or(ChatCompletionsError::InvalidTransition)
    }
}

/// Converts validated Chat Completions facts into Provider Runtime facts.
///
/// Completion identifiers and failure details stay inside the dialect adapter.
/// Runtime callers receive bounded text, one canonical Tool call, normalized
/// usage, or a fixed failure classification.
pub fn normalize_chat_completions_events(
    events: &[ChatCompletionsEvent],
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let mut completion_id = None;
    let mut completed = false;
    let mut normalized = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if completed {
            return Err(ProviderError::InvalidResponse(
                "Chat Completions event followed completion",
            ));
        }
        match &event.kind {
            ChatCompletionsEventKind::TextDelta {
                completion_id: actual,
                choice_index,
                delta,
            } => {
                observe_normalized_identity(&mut completion_id, actual, *choice_index)?;
                normalized.push(ProviderEvent::TextDelta(delta.clone()));
            }
            ChatCompletionsEventKind::FunctionCall {
                completion_id: actual,
                choice_index,
                call_id,
                name,
                arguments_json,
            } => {
                observe_normalized_identity(&mut completion_id, actual, *choice_index)?;
                normalized.push(ProviderEvent::FunctionCall(ProviderToolCall::new(
                    call_id.clone(),
                    name.clone(),
                    arguments_json.clone(),
                )?));
            }
            ChatCompletionsEventKind::Completed {
                completion_id: actual,
                usage,
                service_tier,
            } => {
                observe_normalized_identity(&mut completion_id, actual, 0)?;
                if index + 1 != events.len() {
                    return Err(ProviderError::InvalidResponse(
                        "Chat Completions completion is not terminal",
                    ));
                }
                let usage = usage.unwrap_or_default();
                normalized.push(ProviderEvent::Completed(UsageRecord::new(
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    None,
                    usage.output_tokens,
                    usage.reasoning_output_tokens,
                    usage.total_tokens,
                    service_tier.clone(),
                )?));
                completed = true;
            }
            ChatCompletionsEventKind::Incomplete { .. } => {
                return Err(ProviderError::unavailable(
                    "Chat Completions request ended incomplete",
                ));
            }
        }
    }
    if completion_id.is_none() || !completed {
        return Err(ProviderError::InvalidResponse(
            "Chat Completions stream has no complete terminal",
        ));
    }
    Ok(normalized)
}

fn observe_normalized_identity(
    expected: &mut Option<String>,
    actual: &str,
    choice_index: u32,
) -> Result<(), ProviderError> {
    if choice_index != 0 {
        return Err(ProviderError::InvalidResponse(
            "Chat Completions choice is not supported",
        ));
    }
    match expected.as_deref() {
        Some(expected) if expected != actual => Err(ProviderError::InvalidResponse(
            "Chat Completions stream identity changed",
        )),
        Some(_) => Ok(()),
        None => {
            *expected = Some(actual.to_owned());
            Ok(())
        }
    }
}

#[derive(Default)]
struct ToolCallState {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Deserialize)]
struct WireChunk {
    id: String,
    object: String,
    model: String,
    #[serde(default)]
    choices: Vec<WireChoice>,
    service_tier: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    index: u32,
    delta: WireDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct WireDelta {
    role: Option<String>,
    content: Option<String>,
    refusal: Option<String>,
    function_call: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Deserialize)]
struct WireToolCallDelta {
    index: u32,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<WireFunctionDelta>,
}

#[derive(Deserialize)]
struct WireFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    prompt_cache_hit_tokens: Option<u64>,
    prompt_cache_miss_tokens: Option<u64>,
    prompt_tokens_details: Option<WirePromptTokenDetails>,
    completion_tokens_details: Option<WireCompletionTokenDetails>,
}

#[derive(Deserialize)]
struct WirePromptTokenDetails {
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireCompletionTokenDetails {
    reasoning_tokens: Option<u64>,
}

impl TryFrom<WireUsage> for ChatCompletionsUsage {
    type Error = ChatCompletionsError;

    fn try_from(value: WireUsage) -> Result<Self, Self::Error> {
        let detailed_cached_tokens = value
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens);
        let cached_input_tokens = match (detailed_cached_tokens, value.prompt_cache_hit_tokens) {
            (Some(details), Some(top_level)) if details != top_level => {
                return Err(ChatCompletionsError::InvalidTransition);
            }
            (Some(details), _) => Some(details),
            (_, top_level) => top_level,
        };
        if let (Some(input), Some(cached)) = (value.prompt_tokens, cached_input_tokens)
            && cached > input
        {
            return Err(ChatCompletionsError::InvalidTransition);
        }
        if let (Some(input), Some(hit), Some(miss)) = (
            value.prompt_tokens,
            value.prompt_cache_hit_tokens,
            value.prompt_cache_miss_tokens,
        ) && hit.checked_add(miss) != Some(input)
        {
            return Err(ChatCompletionsError::InvalidTransition);
        }
        Ok(Self {
            input_tokens: value.prompt_tokens,
            cached_input_tokens,
            output_tokens: value.completion_tokens,
            reasoning_output_tokens: value
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: value.total_tokens,
        })
    }
}

fn decode_json<T>(data: &str) -> Result<T, ChatCompletionsError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(data).map_err(|_| ChatCompletionsError::MalformedEvent)
}

fn canonical_arguments(arguments: &str) -> Result<String, ChatCompletionsError> {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(ChatCompletionsError::ArgumentLimitExceeded);
    }
    let value: Value = decode_json(arguments)?;
    if !value.is_object() {
        return Err(ChatCompletionsError::ArgumentsNotObject);
    }
    validate_argument_depth(&value)?;
    let canonical =
        serde_json::to_string(&value).map_err(|_| ChatCompletionsError::MalformedEvent)?;
    if canonical.len() > MAX_ARGUMENT_BYTES {
        return Err(ChatCompletionsError::ArgumentLimitExceeded);
    }
    Ok(canonical)
}

fn validate_argument_depth(value: &Value) -> Result<(), ChatCompletionsError> {
    let mut pending = vec![(value, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_ARGUMENT_DEPTH {
            return Err(ChatCompletionsError::ArgumentNestingExceeded);
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

fn set_once_or_match(
    destination: &mut Option<String>,
    value: String,
) -> Result<(), ChatCompletionsError> {
    match destination.as_deref() {
        Some(expected) if expected != value => Err(ChatCompletionsError::InvalidTransition),
        Some(_) => Ok(()),
        None => {
            *destination = Some(value);
            Ok(())
        }
    }
}

fn validate_identifier(value: &str) -> Result<(), ChatCompletionsError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(ChatCompletionsError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), ChatCompletionsError> {
    if value.is_empty()
        || value.len() > MAX_TOOL_NAME_BYTES
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err(ChatCompletionsError::InvalidToolName);
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize) -> Result<(), ChatCompletionsError> {
    if value.trim().is_empty()
        || value.trim() != value
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(ChatCompletionsError::InvalidText);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatCompletionsError {
    InvalidLimits,
    Sse(SseError),
    Poisoned,
    MalformedEvent,
    EventTypeMismatch,
    EventLimitExceeded,
    EventAfterTerminal,
    UnsupportedChoice,
    UnsupportedDelta,
    UnsupportedFinishReason,
    UnsupportedToolCall,
    InvalidTransition,
    InvalidIdentifier,
    InvalidToolName,
    InvalidText,
    OutputLimitExceeded,
    ArgumentLimitExceeded,
    ArgumentNestingExceeded,
    ArgumentsNotObject,
    IncompleteStream,
}

impl fmt::Display for ChatCompletionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "invalid Chat Completions decoder limits",
            Self::Sse(_) => "invalid Chat Completions SSE framing",
            Self::Poisoned => "Chat Completions decoder must be discarded after a protocol error",
            Self::MalformedEvent => "malformed Chat Completions event",
            Self::EventTypeMismatch => "unsupported Chat Completions SSE event type",
            Self::EventLimitExceeded => "Chat Completions stream exceeds its event limit",
            Self::EventAfterTerminal => "Chat Completions event follows its terminal",
            Self::UnsupportedChoice => "unsupported Chat Completions choice",
            Self::UnsupportedDelta => "unsupported Chat Completions delta",
            Self::UnsupportedFinishReason => "unsupported Chat Completions finish reason",
            Self::UnsupportedToolCall => "unsupported Chat Completions Tool call",
            Self::InvalidTransition => "invalid Chat Completions event transition",
            Self::InvalidIdentifier => "invalid Chat Completions identifier",
            Self::InvalidToolName => "invalid Chat Completions function name",
            Self::InvalidText => "invalid Chat Completions text field",
            Self::OutputLimitExceeded => {
                "Chat Completions text exceeds the configured output limit"
            }
            Self::ArgumentLimitExceeded => {
                "Chat Completions function arguments exceed their byte limit"
            }
            Self::ArgumentNestingExceeded => {
                "Chat Completions function arguments exceed their nesting limit"
            }
            Self::ArgumentsNotObject => "Chat Completions function arguments must be a JSON object",
            Self::IncompleteStream => "Chat Completions stream ended before its terminal event",
        })
    }
}

impl Error for ChatCompletionsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sse(error) => Some(error),
            _ => None,
        }
    }
}
