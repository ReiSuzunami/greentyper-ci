//! Bounded OpenAI Responses SSE decoding into dialect-scoped stream facts.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

use super::sse::{SseError, SseEvent, SseLimits, SseParser};
use crate::config::MAX_OUTPUT_BYTES;

const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 4096;
const MAX_OUTPUT_ITEMS: u32 = 1024;
const MAX_CONTENT_PARTS: u32 = 1024;
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ARGUMENT_DEPTH: usize = 64;
const MAX_ERROR_BYTES: usize = 4096;
const MAX_SERVICE_TIER_BYTES: usize = 64;

#[derive(Clone, Eq, PartialEq)]
pub struct ResponsesEvent {
    pub sequence_number: u64,
    pub kind: ResponsesEventKind,
}

impl fmt::Debug for ResponsesEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesEvent")
            .field("sequence_number", &self.sequence_number)
            .field("kind", &self.kind)
            .finish()
    }
}

impl ResponsesEvent {
    #[must_use]
    pub const fn new(sequence_number: u64, kind: ResponsesEventKind) -> Self {
        Self {
            sequence_number,
            kind,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ResponsesEventKind {
    Created {
        response_id: String,
    },
    TextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
    },
    FunctionCall {
        item_id: String,
        output_index: u32,
        call_id: String,
        name: String,
        arguments_json: String,
    },
    Completed {
        response_id: String,
        usage: Option<ResponsesUsage>,
        service_tier: Option<String>,
    },
    Failed {
        response_id: String,
        code: Option<String>,
        message: Option<String>,
    },
    Incomplete {
        response_id: String,
        reason: Option<String>,
    },
    Error {
        code: Option<String>,
        message: String,
        param: Option<String>,
    },
}

impl fmt::Debug for ResponsesEventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created { response_id } => formatter
                .debug_struct("Created")
                .field("response_id_bytes", &response_id.len())
                .finish(),
            Self::TextDelta {
                item_id,
                output_index,
                content_index,
                delta,
            } => formatter
                .debug_struct("TextDelta")
                .field("item_id_bytes", &item_id.len())
                .field("output_index", output_index)
                .field("content_index", content_index)
                .field("delta_bytes", &delta.len())
                .finish(),
            Self::FunctionCall {
                item_id,
                output_index,
                call_id,
                name,
                arguments_json,
            } => formatter
                .debug_struct("FunctionCall")
                .field("item_id_bytes", &item_id.len())
                .field("output_index", output_index)
                .field("call_id_bytes", &call_id.len())
                .field("name_bytes", &name.len())
                .field("arguments_bytes", &arguments_json.len())
                .finish(),
            Self::Completed {
                response_id,
                usage,
                service_tier,
            } => formatter
                .debug_struct("Completed")
                .field("response_id_bytes", &response_id.len())
                .field("usage", usage)
                .field(
                    "service_tier_bytes",
                    &service_tier.as_ref().map(String::len),
                )
                .finish(),
            Self::Failed {
                response_id,
                code,
                message,
            } => formatter
                .debug_struct("Failed")
                .field("response_id_bytes", &response_id.len())
                .field("code_bytes", &code.as_ref().map(String::len))
                .field("message_bytes", &message.as_ref().map(String::len))
                .finish(),
            Self::Incomplete {
                response_id,
                reason,
            } => formatter
                .debug_struct("Incomplete")
                .field("response_id_bytes", &response_id.len())
                .field("reason_bytes", &reason.as_ref().map(String::len))
                .finish(),
            Self::Error {
                code,
                message,
                param,
            } => formatter
                .debug_struct("Error")
                .field("code_bytes", &code.as_ref().map(String::len))
                .field("message_bytes", &message.len())
                .field("param_bytes", &param.as_ref().map(String::len))
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResponsesUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

pub struct ResponsesSseDecoder {
    sse: Option<SseParser>,
    processed_sse_events: usize,
    max_output_bytes: usize,
    total_text_bytes: usize,
    response_id: Option<String>,
    items: BTreeMap<u32, OutputItemState>,
    last_sequence: Option<u64>,
    terminal: bool,
    poisoned: bool,
    events: Vec<ResponsesEvent>,
}

impl fmt::Debug for ResponsesSseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesSseDecoder")
            .field("sse_present", &self.sse.is_some())
            .field("processed_sse_events", &self.processed_sse_events)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("total_text_bytes", &self.total_text_bytes)
            .field(
                "response_id_bytes",
                &self.response_id.as_ref().map(String::len),
            )
            .field("item_count", &self.items.len())
            .field("last_sequence", &self.last_sequence)
            .field("terminal", &self.terminal)
            .field("poisoned", &self.poisoned)
            .field("event_count", &self.events.len())
            .finish()
    }
}

impl ResponsesSseDecoder {
    pub fn new(max_output_bytes: usize) -> Result<Self, ResponsesError> {
        if max_output_bytes == 0 || max_output_bytes > MAX_OUTPUT_BYTES as usize {
            return Err(ResponsesError::InvalidLimits);
        }
        let limits =
            SseLimits::new(MAX_STREAM_BYTES, MAX_LINE_BYTES).map_err(ResponsesError::Sse)?;
        Ok(Self {
            sse: Some(SseParser::new(limits)),
            processed_sse_events: 0,
            max_output_bytes,
            total_text_bytes: 0,
            response_id: None,
            items: BTreeMap::new(),
            last_sequence: None,
            terminal: false,
            poisoned: false,
            events: Vec::new(),
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), ResponsesError> {
        if self.poisoned {
            return Err(ResponsesError::Poisoned);
        }
        let result = self.push_inner(chunk);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    #[must_use]
    pub fn events(&self) -> &[ResponsesEvent] {
        &self.events
    }

    pub fn finish(mut self) -> Result<Vec<ResponsesEvent>, ResponsesError> {
        if self.poisoned {
            return Err(ResponsesError::Poisoned);
        }
        let sse = self.sse.take().ok_or(ResponsesError::Poisoned)?;
        let framed = sse.finish().map_err(ResponsesError::Sse)?;
        self.process_framed_events(&framed)?;
        if !self.terminal {
            return Err(ResponsesError::IncompleteStream);
        }
        Ok(self.events)
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<(), ResponsesError> {
        let sse = self.sse.as_mut().ok_or(ResponsesError::Poisoned)?;
        sse.push(chunk).map_err(ResponsesError::Sse)?;
        let framed = sse.take_events();
        self.process_framed_events(&framed)
    }

    fn process_framed_events(&mut self, framed: &[SseEvent]) -> Result<(), ResponsesError> {
        for event in framed {
            self.process_event(event)?;
            self.processed_sse_events = self
                .processed_sse_events
                .checked_add(1)
                .ok_or(ResponsesError::EventLimitExceeded)?;
            if self.processed_sse_events > MAX_EVENTS {
                return Err(ResponsesError::EventLimitExceeded);
            }
        }
        Ok(())
    }

    fn process_event(&mut self, event: &SseEvent) -> Result<(), ResponsesError> {
        if self.terminal {
            return Err(ResponsesError::EventAfterTerminal);
        }
        let envelope: WireEnvelope = decode_json(event.data())?;
        if event.event() != "message" && event.event() != envelope.kind {
            return Err(ResponsesError::EventTypeMismatch);
        }
        if self
            .last_sequence
            .is_some_and(|last| envelope.sequence_number <= last)
        {
            return Err(ResponsesError::SequenceNotIncreasing);
        }

        let emitted = match envelope.kind.as_str() {
            "response.created" => self.created(event.data())?,
            "response.in_progress" => {
                self.in_progress(event.data())?;
                None
            }
            "response.output_item.added" => {
                self.output_item_added(event.data())?;
                None
            }
            "response.content_part.added" => {
                self.content_part_added(event.data())?;
                None
            }
            "response.output_text.delta" => self.output_text_delta(event.data())?,
            "response.output_text.done" => {
                self.output_text_done(event.data())?;
                None
            }
            "response.content_part.done" => {
                self.content_part_done(event.data())?;
                None
            }
            "response.function_call_arguments.delta" => {
                self.function_arguments_delta(event.data())?;
                None
            }
            "response.function_call_arguments.done" => {
                self.function_arguments_done(event.data())?;
                None
            }
            "response.output_item.done" => self.output_item_done(event.data())?,
            "response.completed" => self.completed(event.data())?,
            "response.failed" => self.failed(event.data())?,
            "response.incomplete" => self.incomplete(event.data())?,
            "error" => self.error(event.data())?,
            _ => return Err(ResponsesError::UnsupportedEvent),
        };
        self.last_sequence = Some(envelope.sequence_number);
        if let Some(kind) = emitted {
            self.events
                .push(ResponsesEvent::new(envelope.sequence_number, kind));
        }
        Ok(())
    }

    fn created(&mut self, data: &str) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireResponseEvent = decode_json(data)?;
        if self.response_id.is_some() || !self.items.is_empty() {
            return Err(ResponsesError::InvalidTransition);
        }
        validate_identifier(&event.response.id)?;
        validate_status(event.response.status.as_deref(), "in_progress")?;
        self.response_id = Some(event.response.id.clone());
        Ok(Some(ResponsesEventKind::Created {
            response_id: event.response.id,
        }))
    }

    fn in_progress(&self, data: &str) -> Result<(), ResponsesError> {
        let event: WireResponseEvent = decode_json(data)?;
        self.validate_response(&event.response.id)?;
        validate_status(event.response.status.as_deref(), "in_progress")
    }

    fn output_item_added(&mut self, data: &str) -> Result<(), ResponsesError> {
        self.require_response()?;
        let event: WireOutputItemEvent = decode_json(data)?;
        validate_output_index(event.output_index)?;
        validate_identifier(&event.item.id)?;
        if self.items.contains_key(&event.output_index) {
            return Err(ResponsesError::InvalidTransition);
        }
        let state = match event.item.kind.as_str() {
            "message" => {
                validate_status(event.item.status.as_deref(), "in_progress")?;
                OutputItemState::Message(MessageState {
                    id: event.item.id,
                    parts: BTreeMap::new(),
                    done: false,
                })
            }
            "function_call" => {
                validate_status(event.item.status.as_deref(), "in_progress")?;
                let call_id = event.item.call_id.ok_or(ResponsesError::MalformedEvent)?;
                let name = event.item.name.ok_or(ResponsesError::MalformedEvent)?;
                let arguments = event.item.arguments.unwrap_or_default();
                validate_identifier(&call_id)?;
                validate_tool_name(&name)?;
                if arguments.len() > MAX_ARGUMENT_BYTES {
                    return Err(ResponsesError::ArgumentLimitExceeded);
                }
                OutputItemState::FunctionCall(FunctionCallState {
                    id: event.item.id,
                    call_id,
                    name,
                    arguments,
                    canonical_arguments: None,
                    arguments_done: false,
                    done: false,
                })
            }
            _ => return Err(ResponsesError::UnsupportedOutputItem),
        };
        self.items.insert(event.output_index, state);
        Ok(())
    }

    fn content_part_added(&mut self, data: &str) -> Result<(), ResponsesError> {
        let event: WireContentPartEvent = decode_json(data)?;
        validate_content_index(event.content_index)?;
        if event.part.kind != "output_text" {
            return Err(ResponsesError::UnsupportedContentPart);
        }
        let text = event.part.text.ok_or(ResponsesError::MalformedEvent)?;
        self.reserve_text(text.len())?;
        let message = self.message_mut(event.output_index, &event.item_id)?;
        if message.done || message.parts.contains_key(&event.content_index) {
            return Err(ResponsesError::InvalidTransition);
        }
        message.parts.insert(
            event.content_index,
            TextPartState {
                text,
                output_done: false,
                content_done: false,
            },
        );
        Ok(())
    }

    fn output_text_delta(
        &mut self,
        data: &str,
    ) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireTextDeltaEvent = decode_json(data)?;
        if event.delta.is_empty() {
            return Err(ResponsesError::MalformedEvent);
        }
        self.reserve_text(event.delta.len())?;
        let part = self.text_part_mut(event.output_index, event.content_index, &event.item_id)?;
        if part.output_done || part.content_done {
            return Err(ResponsesError::InvalidTransition);
        }
        part.text.push_str(&event.delta);
        Ok(Some(ResponsesEventKind::TextDelta {
            item_id: event.item_id,
            output_index: event.output_index,
            content_index: event.content_index,
            delta: event.delta,
        }))
    }

    fn output_text_done(&mut self, data: &str) -> Result<(), ResponsesError> {
        let event: WireTextDoneEvent = decode_json(data)?;
        let part = self.text_part_mut(event.output_index, event.content_index, &event.item_id)?;
        if part.output_done || part.content_done || part.text != event.text {
            return Err(ResponsesError::InvalidTransition);
        }
        part.output_done = true;
        Ok(())
    }

    fn content_part_done(&mut self, data: &str) -> Result<(), ResponsesError> {
        let event: WireContentPartEvent = decode_json(data)?;
        if event.part.kind != "output_text" {
            return Err(ResponsesError::UnsupportedContentPart);
        }
        let text = event.part.text.ok_or(ResponsesError::MalformedEvent)?;
        let part = self.text_part_mut(event.output_index, event.content_index, &event.item_id)?;
        if !part.output_done || part.content_done || part.text != text {
            return Err(ResponsesError::InvalidTransition);
        }
        part.content_done = true;
        Ok(())
    }

    fn function_arguments_delta(&mut self, data: &str) -> Result<(), ResponsesError> {
        let event: WireFunctionArgumentsDeltaEvent = decode_json(data)?;
        if event.delta.is_empty() {
            return Err(ResponsesError::MalformedEvent);
        }
        let function = self.function_mut(event.output_index, &event.item_id)?;
        if function.arguments_done || function.done {
            return Err(ResponsesError::InvalidTransition);
        }
        let next = function
            .arguments
            .len()
            .checked_add(event.delta.len())
            .ok_or(ResponsesError::ArgumentLimitExceeded)?;
        if next > MAX_ARGUMENT_BYTES {
            return Err(ResponsesError::ArgumentLimitExceeded);
        }
        function.arguments.push_str(&event.delta);
        Ok(())
    }

    fn function_arguments_done(&mut self, data: &str) -> Result<(), ResponsesError> {
        let event: WireFunctionArgumentsDoneEvent = decode_json(data)?;
        let function = self.function_mut(event.output_index, &event.item_id)?;
        if function.arguments_done
            || function.done
            || function.name != event.name
            || function.arguments != event.arguments
        {
            return Err(ResponsesError::InvalidTransition);
        }
        let canonical = canonical_arguments(&event.arguments)?;
        function.canonical_arguments = Some(canonical);
        function.arguments_done = true;
        Ok(())
    }

    fn output_item_done(
        &mut self,
        data: &str,
    ) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireOutputItemEvent = decode_json(data)?;
        let state = self
            .items
            .get_mut(&event.output_index)
            .ok_or(ResponsesError::InvalidTransition)?;
        match state {
            OutputItemState::Message(message) => {
                if message.done || event.item.kind != "message" || event.item.id != message.id {
                    return Err(ResponsesError::InvalidTransition);
                }
                validate_status(event.item.status.as_deref(), "completed")?;
                if message.parts.is_empty()
                    || message
                        .parts
                        .values()
                        .any(|part| !part.output_done || !part.content_done)
                {
                    return Err(ResponsesError::InvalidTransition);
                }
                validate_message_content(&message.parts, &event.item.content)?;
                message.done = true;
                Ok(None)
            }
            OutputItemState::FunctionCall(function) => {
                if function.done
                    || !function.arguments_done
                    || event.item.kind != "function_call"
                    || event.item.id != function.id
                    || event.item.call_id.as_deref() != Some(function.call_id.as_str())
                    || event.item.name.as_deref() != Some(function.name.as_str())
                    || event.item.arguments.as_deref() != Some(function.arguments.as_str())
                {
                    return Err(ResponsesError::InvalidTransition);
                }
                validate_status(event.item.status.as_deref(), "completed")?;
                let arguments_json = function
                    .canonical_arguments
                    .clone()
                    .ok_or(ResponsesError::InvalidTransition)?;
                function.done = true;
                Ok(Some(ResponsesEventKind::FunctionCall {
                    item_id: function.id.clone(),
                    output_index: event.output_index,
                    call_id: function.call_id.clone(),
                    name: function.name.clone(),
                    arguments_json,
                }))
            }
        }
    }

    fn completed(&mut self, data: &str) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireResponseEvent = decode_json(data)?;
        self.validate_response(&event.response.id)?;
        validate_status(event.response.status.as_deref(), "completed")?;
        if self.items.values().any(|item| !item.is_done()) {
            return Err(ResponsesError::InvalidTransition);
        }
        let service_tier =
            validate_optional_text(event.response.service_tier, MAX_SERVICE_TIER_BYTES)?;
        let usage = event.response.usage.map(ResponsesUsage::from);
        self.terminal = true;
        Ok(Some(ResponsesEventKind::Completed {
            response_id: event.response.id,
            usage,
            service_tier,
        }))
    }

    fn failed(&mut self, data: &str) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireResponseEvent = decode_json(data)?;
        self.validate_response(&event.response.id)?;
        validate_status(event.response.status.as_deref(), "failed")?;
        let (code, message) = validate_wire_error(event.response.error)?;
        self.terminal = true;
        Ok(Some(ResponsesEventKind::Failed {
            response_id: event.response.id,
            code,
            message,
        }))
    }

    fn incomplete(&mut self, data: &str) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireResponseEvent = decode_json(data)?;
        self.validate_response(&event.response.id)?;
        validate_status(event.response.status.as_deref(), "incomplete")?;
        let reason = event
            .response
            .incomplete_details
            .and_then(|details| details.reason);
        let reason = validate_optional_text(reason, MAX_ERROR_BYTES)?;
        self.terminal = true;
        Ok(Some(ResponsesEventKind::Incomplete {
            response_id: event.response.id,
            reason,
        }))
    }

    fn error(&mut self, data: &str) -> Result<Option<ResponsesEventKind>, ResponsesError> {
        let event: WireErrorEvent = decode_json(data)?;
        let code = validate_optional_text(event.code, MAX_ERROR_BYTES)?;
        let message = validate_text(event.message, MAX_ERROR_BYTES)?;
        let param = validate_optional_text(event.param, MAX_ERROR_BYTES)?;
        self.terminal = true;
        Ok(Some(ResponsesEventKind::Error {
            code,
            message,
            param,
        }))
    }

    fn require_response(&self) -> Result<(), ResponsesError> {
        self.response_id
            .as_ref()
            .map(|_| ())
            .ok_or(ResponsesError::InvalidTransition)
    }

    fn validate_response(&self, actual: &str) -> Result<(), ResponsesError> {
        validate_identifier(actual)?;
        if self.response_id.as_deref() != Some(actual) {
            return Err(ResponsesError::InvalidTransition);
        }
        Ok(())
    }

    fn reserve_text(&mut self, bytes: usize) -> Result<(), ResponsesError> {
        self.total_text_bytes = self
            .total_text_bytes
            .checked_add(bytes)
            .ok_or(ResponsesError::OutputLimitExceeded)?;
        if self.total_text_bytes > self.max_output_bytes {
            return Err(ResponsesError::OutputLimitExceeded);
        }
        Ok(())
    }

    fn message_mut(
        &mut self,
        output_index: u32,
        item_id: &str,
    ) -> Result<&mut MessageState, ResponsesError> {
        validate_identifier(item_id)?;
        match self.items.get_mut(&output_index) {
            Some(OutputItemState::Message(message)) if message.id == item_id => Ok(message),
            _ => Err(ResponsesError::InvalidTransition),
        }
    }

    fn text_part_mut(
        &mut self,
        output_index: u32,
        content_index: u32,
        item_id: &str,
    ) -> Result<&mut TextPartState, ResponsesError> {
        validate_content_index(content_index)?;
        let message = self.message_mut(output_index, item_id)?;
        if message.done {
            return Err(ResponsesError::InvalidTransition);
        }
        message
            .parts
            .get_mut(&content_index)
            .ok_or(ResponsesError::InvalidTransition)
    }

    fn function_mut(
        &mut self,
        output_index: u32,
        item_id: &str,
    ) -> Result<&mut FunctionCallState, ResponsesError> {
        validate_identifier(item_id)?;
        match self.items.get_mut(&output_index) {
            Some(OutputItemState::FunctionCall(function)) if function.id == item_id => Ok(function),
            _ => Err(ResponsesError::InvalidTransition),
        }
    }
}

enum OutputItemState {
    Message(MessageState),
    FunctionCall(FunctionCallState),
}

impl OutputItemState {
    const fn is_done(&self) -> bool {
        match self {
            Self::Message(message) => message.done,
            Self::FunctionCall(function) => function.done,
        }
    }
}

struct MessageState {
    id: String,
    parts: BTreeMap<u32, TextPartState>,
    done: bool,
}

struct TextPartState {
    text: String,
    output_done: bool,
    content_done: bool,
}

struct FunctionCallState {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    canonical_arguments: Option<String>,
    arguments_done: bool,
    done: bool,
}

#[derive(Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    kind: String,
    sequence_number: u64,
}

#[derive(Deserialize)]
struct WireResponseEvent {
    response: WireResponse,
}

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    status: Option<String>,
    service_tier: Option<String>,
    usage: Option<WireUsage>,
    error: Option<WireResponseError>,
    incomplete_details: Option<WireIncompleteDetails>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    input_tokens_details: Option<WireInputTokenDetails>,
    output_tokens: Option<u64>,
    output_tokens_details: Option<WireOutputTokenDetails>,
    total_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireInputTokenDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireOutputTokenDetails {
    reasoning_tokens: Option<u64>,
}

impl From<WireUsage> for ResponsesUsage {
    fn from(value: WireUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cached_input_tokens: value
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            cache_write_input_tokens: value
                .input_tokens_details
                .and_then(|details| details.cache_write_tokens),
            output_tokens: value.output_tokens,
            reasoning_output_tokens: value
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: value.total_tokens,
        }
    }
}

#[derive(Deserialize)]
struct WireResponseError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct WireIncompleteDetails {
    reason: Option<String>,
}

#[derive(Deserialize)]
struct WireOutputItemEvent {
    output_index: u32,
    item: WireOutputItem,
}

#[derive(Deserialize)]
struct WireOutputItem {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    #[serde(default)]
    content: Vec<WireContentPart>,
}

#[derive(Deserialize)]
struct WireContentPartEvent {
    item_id: String,
    output_index: u32,
    content_index: u32,
    part: WireContentPart,
}

#[derive(Deserialize)]
struct WireContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct WireTextDeltaEvent {
    item_id: String,
    output_index: u32,
    content_index: u32,
    delta: String,
}

#[derive(Deserialize)]
struct WireTextDoneEvent {
    item_id: String,
    output_index: u32,
    content_index: u32,
    text: String,
}

#[derive(Deserialize)]
struct WireFunctionArgumentsDeltaEvent {
    item_id: String,
    output_index: u32,
    delta: String,
}

#[derive(Deserialize)]
struct WireFunctionArgumentsDoneEvent {
    item_id: String,
    output_index: u32,
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct WireErrorEvent {
    code: Option<String>,
    message: String,
    param: Option<String>,
}

fn decode_json<T>(data: &str) -> Result<T, ResponsesError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(data).map_err(|_| ResponsesError::MalformedEvent)
}

fn canonical_arguments(arguments: &str) -> Result<String, ResponsesError> {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(ResponsesError::ArgumentLimitExceeded);
    }
    let value: Value = decode_json(arguments)?;
    if !value.is_object() {
        return Err(ResponsesError::ArgumentsNotObject);
    }
    validate_argument_depth(&value)?;
    let canonical = serde_json::to_string(&value).map_err(|_| ResponsesError::MalformedEvent)?;
    if canonical.len() > MAX_ARGUMENT_BYTES {
        return Err(ResponsesError::ArgumentLimitExceeded);
    }
    Ok(canonical)
}

fn validate_argument_depth(value: &Value) -> Result<(), ResponsesError> {
    let mut pending = vec![(value, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_ARGUMENT_DEPTH {
            return Err(ResponsesError::ArgumentNestingExceeded);
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

fn validate_message_content(
    expected: &BTreeMap<u32, TextPartState>,
    actual: &[WireContentPart],
) -> Result<(), ResponsesError> {
    if expected.len() != actual.len() {
        return Err(ResponsesError::InvalidTransition);
    }
    for (index, part) in actual.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| ResponsesError::InvalidTransition)?;
        let expected = expected
            .get(&index)
            .ok_or(ResponsesError::InvalidTransition)?;
        if part.kind != "output_text" || part.text.as_deref() != Some(expected.text.as_str()) {
            return Err(ResponsesError::InvalidTransition);
        }
    }
    Ok(())
}

fn validate_wire_error(
    error: Option<WireResponseError>,
) -> Result<(Option<String>, Option<String>), ResponsesError> {
    match error {
        Some(error) => Ok((
            validate_optional_text(error.code, MAX_ERROR_BYTES)?,
            validate_optional_text(error.message, MAX_ERROR_BYTES)?,
        )),
        None => Ok((None, None)),
    }
}

fn validate_output_index(value: u32) -> Result<(), ResponsesError> {
    if value >= MAX_OUTPUT_ITEMS {
        return Err(ResponsesError::OutputItemLimitExceeded);
    }
    Ok(())
}

fn validate_content_index(value: u32) -> Result<(), ResponsesError> {
    if value >= MAX_CONTENT_PARTS {
        return Err(ResponsesError::ContentPartLimitExceeded);
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<(), ResponsesError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(ResponsesError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), ResponsesError> {
    if value.is_empty()
        || value.len() > MAX_TOOL_NAME_BYTES
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err(ResponsesError::InvalidToolName);
    }
    Ok(())
}

fn validate_status(actual: Option<&str>, expected: &str) -> Result<(), ResponsesError> {
    if actual != Some(expected) {
        return Err(ResponsesError::InvalidTransition);
    }
    Ok(())
}

fn validate_optional_text(
    value: Option<String>,
    max_bytes: usize,
) -> Result<Option<String>, ResponsesError> {
    value
        .map(|value| validate_text(value, max_bytes))
        .transpose()
}

fn validate_text(value: String, max_bytes: usize) -> Result<String, ResponsesError> {
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(ResponsesError::InvalidText);
    }
    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesError {
    InvalidLimits,
    Sse(SseError),
    Poisoned,
    MalformedEvent,
    EventTypeMismatch,
    SequenceNotIncreasing,
    EventLimitExceeded,
    EventAfterTerminal,
    UnsupportedEvent,
    UnsupportedOutputItem,
    UnsupportedContentPart,
    InvalidTransition,
    InvalidIdentifier,
    InvalidToolName,
    InvalidText,
    OutputItemLimitExceeded,
    ContentPartLimitExceeded,
    OutputLimitExceeded,
    ArgumentLimitExceeded,
    ArgumentNestingExceeded,
    ArgumentsNotObject,
    IncompleteStream,
}

impl fmt::Display for ResponsesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "invalid Responses decoder limits",
            Self::Sse(_) => "invalid Responses SSE framing",
            Self::Poisoned => "Responses decoder must be discarded after a protocol error",
            Self::MalformedEvent => "malformed Responses event",
            Self::EventTypeMismatch => "Responses SSE and JSON event types do not match",
            Self::SequenceNotIncreasing => "Responses sequence numbers must increase",
            Self::EventLimitExceeded => "Responses stream exceeds its event limit",
            Self::EventAfterTerminal => "Responses event follows a terminal event",
            Self::UnsupportedEvent => "unsupported Responses event type",
            Self::UnsupportedOutputItem => "unsupported Responses output item type",
            Self::UnsupportedContentPart => "unsupported Responses content part type",
            Self::InvalidTransition => "invalid Responses event transition",
            Self::InvalidIdentifier => "invalid Responses identifier",
            Self::InvalidToolName => "invalid Responses function name",
            Self::InvalidText => "invalid Responses text field",
            Self::OutputItemLimitExceeded => "Responses output item index exceeds its limit",
            Self::ContentPartLimitExceeded => "Responses content part index exceeds its limit",
            Self::OutputLimitExceeded => "Responses text exceeds the configured output limit",
            Self::ArgumentLimitExceeded => "Responses function arguments exceed their byte limit",
            Self::ArgumentNestingExceeded => {
                "Responses function arguments exceed their nesting limit"
            }
            Self::ArgumentsNotObject => "Responses function arguments must be a JSON object",
            Self::IncompleteStream => "Responses stream ended before a terminal event",
        })
    }
}

impl Error for ResponsesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sse(error) => Some(error),
            _ => None,
        }
    }
}
