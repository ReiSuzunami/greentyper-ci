//! Bounded Server-Sent Events framing independent of any Provider dialect.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

use serde::Deserialize;

const MAX_DATA_LINES_PER_EVENT: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseLimits {
    max_total_bytes: usize,
    max_line_bytes: usize,
}

impl SseLimits {
    pub fn new(max_total_bytes: usize, max_line_bytes: usize) -> Result<Self, SseError> {
        if max_total_bytes == 0 || max_line_bytes == 0 || max_line_bytes > max_total_bytes {
            return Err(SseError::InvalidLimits);
        }
        Ok(Self {
            max_total_bytes,
            max_line_bytes,
        })
    }

    #[must_use]
    pub const fn max_total_bytes(self) -> usize {
        self.max_total_bytes
    }

    #[must_use]
    pub const fn max_line_bytes(self) -> usize {
        self.max_line_bytes
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct SseEvent {
    event: String,
    data: String,
}

impl fmt::Debug for SseEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SseEvent")
            .field("event_bytes", &self.event.len())
            .field("data_bytes", &self.data.len())
            .finish()
    }
}

impl SseEvent {
    #[must_use]
    pub fn new(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            data: data.into(),
        }
    }

    #[must_use]
    pub fn event(&self) -> &str {
        &self.event
    }

    #[must_use]
    pub fn data(&self) -> &str {
        &self.data
    }
}

pub struct SseParser {
    limits: SseLimits,
    buffer: VecDeque<u8>,
    event_type: Option<String>,
    event_data: String,
    data_line_count: usize,
    events: Vec<SseEvent>,
    total_bytes: usize,
    poisoned: bool,
    skip_lf_after_cr: bool,
}

impl fmt::Debug for SseParser {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SseParser")
            .field("limits", &self.limits)
            .field("buffer_bytes", &self.buffer.len())
            .field(
                "event_type_bytes",
                &self.event_type.as_ref().map(String::len),
            )
            .field("event_data_bytes", &self.event_data.len())
            .field("data_line_count", &self.data_line_count)
            .field("event_count", &self.events.len())
            .field("total_bytes", &self.total_bytes)
            .field("poisoned", &self.poisoned)
            .field("skip_lf_after_cr", &self.skip_lf_after_cr)
            .finish()
    }
}

impl SseParser {
    #[must_use]
    pub const fn new(limits: SseLimits) -> Self {
        Self {
            limits,
            buffer: VecDeque::new(),
            event_type: None,
            event_data: String::new(),
            data_line_count: 0,
            events: Vec::new(),
            total_bytes: 0,
            poisoned: false,
            skip_lf_after_cr: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), SseError> {
        if self.poisoned {
            return Err(SseError::Poisoned);
        }
        let result = self.push_inner(chunk, false).map(|_| ());
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    pub fn push_until_first_event(&mut self, chunk: &[u8]) -> Result<bool, SseError> {
        if self.poisoned {
            return Err(SseError::Poisoned);
        }
        let result = self.push_inner(chunk, true);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    #[must_use]
    pub fn events(&self) -> &[SseEvent] {
        &self.events
    }

    pub(super) fn take_events(&mut self) -> Vec<SseEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn finish(mut self) -> Result<Vec<SseEvent>, SseError> {
        if self.poisoned {
            return Err(SseError::Poisoned);
        }
        self.push_inner(&[], false)?;
        if !self.buffer.is_empty() || self.event_type.is_some() || self.data_line_count != 0 {
            return Err(SseError::IncompleteEvent);
        }
        Ok(self.events)
    }

    fn push_inner(&mut self, chunk: &[u8], stop_after_first_event: bool) -> Result<bool, SseError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or(SseError::TotalBytesExceeded)?;
        if self.total_bytes > self.limits.max_total_bytes {
            return Err(SseError::TotalBytesExceeded);
        }
        self.buffer.extend(chunk.iter().copied());
        let initial_event_count = self.events.len();
        loop {
            if self.skip_lf_after_cr {
                if self.buffer.front() == Some(&b'\n') {
                    self.buffer.pop_front();
                    self.skip_lf_after_cr = false;
                    continue;
                }
                if self.buffer.is_empty() {
                    break;
                }
                self.skip_lf_after_cr = false;
            }
            let Some((line_end, ended_by_cr)) = next_line_ending(&self.buffer) else {
                break;
            };
            if line_end > self.limits.max_line_bytes {
                return Err(SseError::LineBytesExceeded);
            }
            let line = self.buffer.drain(..line_end).collect::<Vec<_>>();
            self.buffer.pop_front();
            self.skip_lf_after_cr = ended_by_cr;
            self.process_line(&line)?;
            if stop_after_first_event && self.events.len() > initial_event_count {
                return Ok(true);
            }
        }
        if self.buffer.len() > self.limits.max_line_bytes {
            return Err(SseError::LineBytesExceeded);
        }
        Ok(self.events.len() > initial_event_count)
    }

    fn process_line(&mut self, line: &[u8]) -> Result<(), SseError> {
        if line.is_empty() {
            self.dispatch();
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let line = std::str::from_utf8(line).map_err(|_| SseError::InvalidUtf8)?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event_type = Some(value.to_owned()),
            "data" => {
                self.data_line_count = self
                    .data_line_count
                    .checked_add(1)
                    .ok_or(SseError::DataLineLimitExceeded)?;
                if self.data_line_count > MAX_DATA_LINES_PER_EVENT {
                    return Err(SseError::DataLineLimitExceeded);
                }
                if self.data_line_count > 1 {
                    self.event_data.push('\n');
                }
                self.event_data.push_str(value);
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) {
        if self.data_line_count == 0 {
            self.event_type = None;
        } else {
            self.events.push(SseEvent {
                event: self
                    .event_type
                    .take()
                    .filter(|event| !event.is_empty())
                    .unwrap_or_else(|| "message".to_owned()),
                data: std::mem::take(&mut self.event_data),
            });
        }
        self.data_line_count = 0;
    }
}

fn next_line_ending(buffer: &VecDeque<u8>) -> Option<(usize, bool)> {
    for (index, byte) in buffer.iter().copied().enumerate() {
        match byte {
            b'\n' => return Some((index, false)),
            b'\r' => return Some((index, true)),
            _ => {}
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SseError {
    InvalidLimits,
    Poisoned,
    TotalBytesExceeded,
    LineBytesExceeded,
    DataLineLimitExceeded,
    InvalidUtf8,
    IncompleteEvent,
}

impl fmt::Display for SseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "invalid SSE parser limits",
            Self::Poisoned => "SSE parser must be discarded after a framing error",
            Self::TotalBytesExceeded => "SSE stream exceeds its byte limit",
            Self::LineBytesExceeded => "SSE line exceeds its byte limit",
            Self::DataLineLimitExceeded => "SSE event exceeds its data-line limit",
            Self::InvalidUtf8 => "SSE line is not valid UTF-8 after framing",
            Self::IncompleteEvent => "SSE stream ended with an incomplete event",
        })
    }
}

impl Error for SseError {}
