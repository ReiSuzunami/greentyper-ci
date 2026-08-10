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
use greentyper_core::provider::chat_completions::{
    ChatCompletionsSseDecoder, normalize_chat_completions_events,
};
use greentyper_core::provider::messages::{MessagesSseDecoder, normalize_messages_events};
use greentyper_core::provider::responses::{
    ResponsesEventKind, ResponsesSseDecoder, normalize_responses_events,
};
use greentyper_core::provider::{
    DeterministicProvider, ProviderDialect, ProviderEpoch, ProviderError, ProviderEvent,
    ProviderPricingSource, ProviderProfileSnapshot, ProviderRequest, ProviderRuntime,
    ProviderToolCall, ProviderToolOutput,
};
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{StatusCode, Url};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, InMemoryCredentialVault, ProviderCredentialScope,
    SecretValue,
};
use crate::provider_http_policy::{bearer_header, credential_header, validate_provider_endpoint};

const FIXTURE_PROFILE: &str = "responses-loopback";
const FIXTURE_MODEL: &str = "fixture-model";
const FIXTURE_ROUTE: &str = "/v1/responses";
const OPENAI_TEMPLATE: &str = "openai";
const OPENAI_COMPATIBLE_TEMPLATE: &str = "openai-compatible";
const DEEPSEEK_TEMPLATE: &str = "deepseek";
const FIXTURE_CREDENTIAL_REFERENCE: &str = "responses-loopback-synthetic";
const SYNTHETIC_AUTHORIZATION: &str = "Bearer greentyper-synthetic-provider-token-v1";
const SYNTHETIC_SECRET: &[u8] = b"greentyper-synthetic-provider-token-v1";
const HTTP_TIMEOUT: Duration = Duration::from_millis(200);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(300);
const MESSAGES_MAX_TOKENS: u32 = 4096;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const SERVER_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;
const PRIVATE_ERROR_BODY: &[u8] = b"provider-private-error-marker";
const SUCCESS_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/responses/v1/http-text.sse");
#[cfg(test)]
const CHAT_TEXT_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/chat_completions/v1/http-text.sse");
#[cfg(test)]
const CHAT_TOOL_CALL_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/chat_completions/v1/http-tool-call.sse");
#[cfg(test)]
const CHAT_TOOL_CONTINUATION_SSE: &[u8] = include_bytes!(
    "../../../tests/fixtures/provider/chat_completions/v1/http-tool-continuation.sse"
);
#[cfg(test)]
const MESSAGES_TEXT_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/messages/v1/http-text.sse");
#[cfg(test)]
const MESSAGES_TOOL_CALL_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/messages/v1/http-tool-call.sse");
#[cfg(test)]
const MESSAGES_TOOL_CONTINUATION_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/messages/v1/http-tool-continuation.sse");
#[cfg(test)]
const TOOL_CALL_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/responses/v1/http-tool-call.sse");
#[cfg(test)]
const TOOL_CONTINUATION_SSE: &[u8] =
    include_bytes!("../../../tests/fixtures/provider/responses/v1/http-tool-continuation.sse");

pub(crate) fn has_provider_adapter(template: &str, dialect: ProviderDialect) -> bool {
    matches!(
        (template, dialect),
        (
            OPENAI_TEMPLATE | OPENAI_COMPATIBLE_TEMPLATE,
            ProviderDialect::Responses | ProviderDialect::ChatCompletions
        ) | (DEEPSEEK_TEMPLATE, ProviderDialect::Messages)
    )
}

fn insert_output_token_limit(
    body: &mut serde_json::Value,
    field: &'static str,
    request: &ProviderRequest,
) {
    let Some(limit) = request.config.resolved().max_output_tokens() else {
        return;
    };
    body.as_object_mut()
        .expect("Provider request body must be an object")
        .insert(field.to_owned(), serde_json::json!(*limit.value()));
}

pub(crate) struct ResponsesHttpProvider<V> {
    client: Client,
    endpoint: Url,
    profile: ProviderProfileSnapshot,
    credential_scope: ProviderCredentialScope,
    vault: V,
    local_echo_enabled: bool,
    pending_continuation: Option<PendingContinuation>,
}

struct PendingContinuation {
    response_id: String,
    call_id: String,
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
        if !has_provider_adapter(profile.template(), ProviderDialect::Responses) {
            return Err(ProviderError::InvalidConfiguration(
                "Provider Profile template has no configured runtime adapter",
            ));
        }
        if !profile.supports(ProviderDialect::Responses) {
            return Err(ProviderError::InvalidConfiguration(
                "Responses Provider Profile does not declare Responses support",
            ));
        }
        let endpoint = profile.endpoint(ProviderDialect::Responses).ok_or(
            ProviderError::InvalidConfiguration("Responses Provider Profile has no endpoint"),
        )?;
        let endpoint = validate_provider_endpoint(&endpoint, profile.allow_insecure_loopback())?;
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
            local_echo_enabled: false,
            pending_continuation: None,
        })
    }

    fn enable_local_echo(&mut self) {
        self.local_echo_enabled = true;
    }

    fn send_request(
        &self,
        body: serde_json::Value,
        max_output_bytes: usize,
    ) -> Result<(String, Vec<ProviderEvent>), ProviderError> {
        let secret = self
            .vault
            .resolve(&self.credential_scope)
            .map_err(map_credential_resolve_error)?;
        let authorization = bearer_header(&secret)?;
        let body = serde_json::to_vec(&body)
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
        let response_id = match events.first().map(|event| &event.kind) {
            Some(ResponsesEventKind::Created { response_id }) => response_id.clone(),
            _ => {
                return Err(ProviderError::InvalidResponse(
                    "Responses HTTP stream omitted response creation",
                ));
            }
        };
        let events = normalize_responses_events(&events)?;
        Ok((response_id, events))
    }

    fn take_pending_continuation(
        &mut self,
        call_id: &str,
    ) -> Result<PendingContinuation, ProviderError> {
        let pending = self
            .pending_continuation
            .as_ref()
            .ok_or(ProviderError::InvalidRequest(
                "Responses Provider has no pending Tool continuation",
            ))?;
        if call_id != pending.call_id {
            return Err(ProviderError::InvalidRequest(
                "Responses Tool output does not match the pending call",
            ));
        }
        self.pending_continuation
            .take()
            .ok_or(ProviderError::InvalidRequest(
                "Responses Provider has no pending Tool continuation",
            ))
    }
}

impl<V: CredentialVault> ProviderRuntime for ResponsesHttpProvider<V> {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        Some(ProviderDialect::Responses)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.provider.profile() != self.profile.profile()
            || request.provider.profile_snapshot() != Some(&self.profile)
            || request
                .provider
                .dialect()
                .is_some_and(|dialect| dialect != ProviderDialect::Responses)
        {
            return Err(ProviderError::InvalidConfiguration(
                "Responses provider identity does not match its frozen Profile",
            ));
        }
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        self.pending_continuation = None;
        let mut body = if self.local_echo_enabled {
            serde_json::json!({
                "input": request.input,
                "model": request.provider.model(),
                "stream": true,
                "tool_choice": "auto",
                "tools": [local_echo_tool_definition()],
            })
        } else {
            serde_json::json!({
                "input": request.input,
                "model": request.provider.model(),
                "stream": true,
            })
        };
        insert_output_token_limit(&mut body, "max_output_tokens", request);
        let (response_id, events) = self.send_request(body, max_output_bytes)?;
        let events = if self.local_echo_enabled {
            normalize_local_echo_calls(events)?
        } else {
            events
        };
        let mut calls = events.iter().filter_map(|event| match event {
            ProviderEvent::FunctionCall(call) => Some(call),
            ProviderEvent::TextDelta(_) | ProviderEvent::Completed(_) => None,
        });
        if let (Some(call), None) = (calls.next(), calls.next()) {
            self.pending_continuation = Some(PendingContinuation {
                response_id,
                call_id: call.call_id().to_owned(),
            });
        }
        Ok(events)
    }

    fn continue_after_tool(
        &mut self,
        request: &ProviderRequest,
        output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        if request.provider.profile() != self.profile.profile()
            || request.provider.profile_snapshot() != Some(&self.profile)
        {
            return Err(ProviderError::InvalidConfiguration(
                "Responses provider identity does not match its frozen Profile",
            ));
        }
        if !self.local_echo_enabled {
            return Err(ProviderError::InvalidRequest(
                "Responses Provider has no enabled Tool continuation",
            ));
        }
        let pending = self.take_pending_continuation(output.call_id())?;
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        let mut body = serde_json::json!({
            "input": [{
                "type": "function_call_output",
                "call_id": output.call_id(),
                "output": output.output(),
            }],
            "model": request.provider.model(),
            "previous_response_id": pending.response_id,
            "stream": true,
            "tool_choice": "none",
            "tools": [local_echo_tool_definition()],
        });
        insert_output_token_limit(&mut body, "max_output_tokens", request);
        self.send_request(body, max_output_bytes)
            .map(|(_, events)| events)
    }
}

pub(crate) struct ChatCompletionsHttpProvider<V> {
    client: Client,
    endpoint: Url,
    profile: ProviderProfileSnapshot,
    credential_scope: ProviderCredentialScope,
    vault: V,
    local_echo_enabled: bool,
    pending_continuation: Option<ChatPendingContinuation>,
}

struct ChatPendingContinuation {
    call_id: String,
    input: String,
    arguments_json: String,
}

impl<V> fmt::Debug for ChatCompletionsHttpProvider<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatCompletionsHttpProvider")
            .field("transport", &"blocking-http-sse")
            .field("authorization", &"redacted")
            .finish()
    }
}

impl<V: CredentialVault> ChatCompletionsHttpProvider<V> {
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

    fn with_client_builder(
        profile: ProviderProfileSnapshot,
        vault: V,
        timeout: Duration,
        client: ClientBuilder,
    ) -> Result<Self, ProviderError> {
        if !has_provider_adapter(profile.template(), ProviderDialect::ChatCompletions) {
            return Err(ProviderError::InvalidConfiguration(
                "Provider Profile template has no configured runtime adapter",
            ));
        }
        if !profile.supports(ProviderDialect::ChatCompletions) {
            return Err(ProviderError::InvalidConfiguration(
                "Chat Completions Provider Profile does not declare Chat Completions support",
            ));
        }
        let endpoint = profile.endpoint(ProviderDialect::ChatCompletions).ok_or(
            ProviderError::InvalidConfiguration(
                "Chat Completions Provider Profile has no endpoint",
            ),
        )?;
        let endpoint = validate_provider_endpoint(&endpoint, profile.allow_insecure_loopback())?;
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
            .map_err(|_| ProviderError::unavailable("Chat Completions HTTP client setup failed"))?;
        Ok(Self {
            client,
            endpoint,
            profile,
            credential_scope,
            vault,
            local_echo_enabled: false,
            pending_continuation: None,
        })
    }

    fn enable_local_echo(&mut self) {
        self.local_echo_enabled = true;
    }

    fn send_request(
        &self,
        body: serde_json::Value,
        max_output_bytes: usize,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        let secret = self
            .vault
            .resolve(&self.credential_scope)
            .map_err(map_credential_resolve_error)?;
        let authorization = bearer_header(&secret)?;
        let body = serde_json::to_vec(&body).map_err(|_| {
            ProviderError::InvalidRequest("Chat Completions request could not be encoded")
        })?;
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream")
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| ProviderError::unavailable("Chat Completions HTTP request failed"))?;
        if response.status() != StatusCode::OK {
            return Err(classify_chat_http_status(response.status()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream")) {
            return Err(ProviderError::InvalidResponse(
                "Chat Completions HTTP response has the wrong content type",
            ));
        }

        let mut decoder = ChatCompletionsSseDecoder::new(max_output_bytes).map_err(|_| {
            ProviderError::InvalidConfiguration("Chat Completions decoder limits are invalid")
        })?;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|_| ProviderError::unavailable("Chat Completions HTTP stream failed"))?;
            if read == 0 {
                break;
            }
            decoder.push(&buffer[..read]).map_err(|_| {
                ProviderError::InvalidResponse("Chat Completions HTTP stream was rejected")
            })?;
        }
        let events = decoder.finish().map_err(|_| {
            ProviderError::InvalidResponse("Chat Completions HTTP stream ended invalidly")
        })?;
        normalize_chat_completions_events(&events)
    }

    fn take_pending_continuation(
        &mut self,
        call_id: &str,
    ) -> Result<ChatPendingContinuation, ProviderError> {
        let pending = self
            .pending_continuation
            .as_ref()
            .ok_or(ProviderError::InvalidRequest(
                "Chat Completions Provider has no pending Tool continuation",
            ))?;
        if call_id != pending.call_id {
            return Err(ProviderError::InvalidRequest(
                "Chat Completions Tool output does not match the pending call",
            ));
        }
        self.pending_continuation
            .take()
            .ok_or(ProviderError::InvalidRequest(
                "Chat Completions Provider has no pending Tool continuation",
            ))
    }

    fn require_request_identity(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if request.provider.profile() != self.profile.profile()
            || request.provider.profile_snapshot() != Some(&self.profile)
            || request.provider.dialect() != Some(ProviderDialect::ChatCompletions)
        {
            return Err(ProviderError::InvalidConfiguration(
                "Chat Completions provider identity does not match its frozen Profile and dialect",
            ));
        }
        Ok(())
    }
}

impl<V: CredentialVault> ProviderRuntime for ChatCompletionsHttpProvider<V> {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        Some(ProviderDialect::ChatCompletions)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.require_request_identity(request)?;
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        self.pending_continuation = None;
        let mut body = if self.local_echo_enabled {
            serde_json::json!({
                "messages": [{"role": "user", "content": request.input}],
                "model": request.provider.model(),
                "stream": true,
                "stream_options": {"include_usage": true},
                "tool_choice": "auto",
                "tools": [chat_local_echo_tool_definition()],
            })
        } else {
            serde_json::json!({
                "messages": [{"role": "user", "content": request.input}],
                "model": request.provider.model(),
                "stream": true,
                "stream_options": {"include_usage": true},
            })
        };
        insert_output_token_limit(&mut body, "max_completion_tokens", request);
        let events = self.send_request(body, max_output_bytes)?;
        let events = if self.local_echo_enabled {
            normalize_local_echo_calls(events)?
        } else {
            events
        };
        let calls = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::FunctionCall(call) => Some(call),
                ProviderEvent::TextDelta(_) | ProviderEvent::Completed(_) => None,
            })
            .collect::<Vec<_>>();
        if let [call] = calls.as_slice() {
            self.pending_continuation = Some(ChatPendingContinuation {
                call_id: call.call_id().to_owned(),
                input: request.input.clone(),
                arguments_json: call.arguments_json().to_owned(),
            });
        }
        Ok(events)
    }

    fn continue_after_tool(
        &mut self,
        request: &ProviderRequest,
        output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.require_request_identity(request)?;
        if !self.local_echo_enabled {
            return Err(ProviderError::InvalidRequest(
                "Chat Completions Provider has no enabled Tool continuation",
            ));
        }
        let pending = self.take_pending_continuation(output.call_id())?;
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        let mut body = serde_json::json!({
            "messages": [
                {"role": "user", "content": pending.input},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": pending.call_id,
                        "type": "function",
                        "function": {
                            "name": "local_echo",
                            "arguments": pending.arguments_json,
                        },
                    }],
                },
                {
                    "role": "tool",
                    "tool_call_id": output.call_id(),
                    "content": output.output(),
                },
            ],
            "model": request.provider.model(),
            "stream": true,
            "stream_options": {"include_usage": true},
            "tool_choice": "none",
            "tools": [chat_local_echo_tool_definition()],
        });
        insert_output_token_limit(&mut body, "max_completion_tokens", request);
        let events = self.send_request(body, max_output_bytes)?;
        if events
            .iter()
            .any(|event| matches!(event, ProviderEvent::FunctionCall(_)))
        {
            return Err(ProviderError::InvalidResponse(
                "Chat Completions continuation returned another Tool call",
            ));
        }
        Ok(events)
    }
}

pub(crate) struct MessagesHttpProvider<V> {
    client: Client,
    endpoint: Url,
    profile: ProviderProfileSnapshot,
    credential_scope: ProviderCredentialScope,
    vault: V,
    local_echo_enabled: bool,
    pending_continuation: Option<MessagesPendingContinuation>,
}

#[derive(Clone)]
struct MessagesPendingContinuation {
    call_id: String,
    input: String,
    arguments_json: String,
}

impl<V> fmt::Debug for MessagesHttpProvider<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessagesHttpProvider")
            .field("transport", &"blocking-http-sse")
            .field("authorization", &"redacted")
            .finish()
    }
}

impl<V: CredentialVault> MessagesHttpProvider<V> {
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

    fn with_client_builder(
        profile: ProviderProfileSnapshot,
        vault: V,
        timeout: Duration,
        client: ClientBuilder,
    ) -> Result<Self, ProviderError> {
        if !has_provider_adapter(profile.template(), ProviderDialect::Messages) {
            return Err(ProviderError::InvalidConfiguration(
                "Provider Profile template has no configured runtime adapter",
            ));
        }
        if !profile.supports(ProviderDialect::Messages) {
            return Err(ProviderError::InvalidConfiguration(
                "Messages Provider Profile does not declare Messages support",
            ));
        }
        let endpoint = profile.endpoint(ProviderDialect::Messages).ok_or(
            ProviderError::InvalidConfiguration("Messages Provider Profile has no endpoint"),
        )?;
        let endpoint = validate_provider_endpoint(&endpoint, profile.allow_insecure_loopback())?;
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
            .map_err(|_| ProviderError::unavailable("Messages HTTP client setup failed"))?;
        Ok(Self {
            client,
            endpoint,
            profile,
            credential_scope,
            vault,
            local_echo_enabled: false,
            pending_continuation: None,
        })
    }

    fn enable_local_echo(&mut self) {
        self.local_echo_enabled = true;
    }

    fn send_request(
        &self,
        body: serde_json::Value,
        max_output_bytes: usize,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        let secret = self
            .vault
            .resolve(&self.credential_scope)
            .map_err(map_credential_resolve_error)?;
        let api_key = credential_header(&secret)?;
        let body = serde_json::to_vec(&body)
            .map_err(|_| ProviderError::InvalidRequest("Messages request could not be encoded"))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ProviderError::InvalidRequest(
                "Messages request exceeds its byte limit",
            ));
        }
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream")
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| ProviderError::unavailable("Messages HTTP request failed"))?;
        if response.status() != StatusCode::OK {
            return Err(classify_messages_http_status(response.status()));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream")) {
            return Err(ProviderError::InvalidResponse(
                "Messages HTTP response has the wrong content type",
            ));
        }

        let mut decoder = MessagesSseDecoder::new(max_output_bytes).map_err(|_| {
            ProviderError::InvalidConfiguration("Messages decoder limits are invalid")
        })?;
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|_| ProviderError::unavailable("Messages HTTP stream failed"))?;
            if read == 0 {
                break;
            }
            decoder
                .push(&buffer[..read])
                .map_err(|_| ProviderError::InvalidResponse("Messages HTTP stream was rejected"))?;
        }
        let events = decoder
            .finish()
            .map_err(|_| ProviderError::InvalidResponse("Messages HTTP stream ended invalidly"))?;
        normalize_messages_events(&events)
    }

    fn require_request_identity(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if request.provider.profile() != self.profile.profile()
            || request.provider.profile_snapshot() != Some(&self.profile)
            || request.provider.dialect() != Some(ProviderDialect::Messages)
        {
            return Err(ProviderError::InvalidConfiguration(
                "Messages provider identity does not match its frozen Profile and dialect",
            ));
        }
        Ok(())
    }

    fn take_pending_continuation(
        &mut self,
        call_id: &str,
    ) -> Result<MessagesPendingContinuation, ProviderError> {
        let pending = self
            .pending_continuation
            .as_ref()
            .ok_or(ProviderError::InvalidRequest(
                "Messages Provider has no pending Tool continuation",
            ))?;
        if call_id != pending.call_id {
            return Err(ProviderError::InvalidRequest(
                "Messages Tool output does not match the pending call",
            ));
        }
        self.pending_continuation
            .take()
            .ok_or(ProviderError::InvalidRequest(
                "Messages Provider has no pending Tool continuation",
            ))
    }
}

impl<V: CredentialVault> ProviderRuntime for MessagesHttpProvider<V> {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        Some(ProviderDialect::Messages)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.require_request_identity(request)?;
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        let max_output_tokens = request
            .config
            .resolved()
            .max_output_tokens()
            .map_or(MESSAGES_MAX_TOKENS, |limit| *limit.value());
        self.pending_continuation = None;
        let body = if self.local_echo_enabled {
            serde_json::json!({
                "max_tokens": max_output_tokens,
                "messages": [{"role": "user", "content": request.input}],
                "model": request.provider.model(),
                "stream": true,
                "thinking": {"type": "disabled"},
                "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
                "tools": [messages_local_echo_tool_definition()],
            })
        } else {
            serde_json::json!({
                "max_tokens": max_output_tokens,
                "messages": [{"role": "user", "content": request.input}],
                "model": request.provider.model(),
                "stream": true,
                "thinking": {"type": "disabled"},
            })
        };
        let events = self.send_request(body, max_output_bytes)?;
        let events = if self.local_echo_enabled {
            normalize_local_echo_calls(events)?
        } else {
            events
        };
        let calls = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::FunctionCall(call) => Some(call),
                ProviderEvent::TextDelta(_) | ProviderEvent::Completed(_) => None,
            })
            .collect::<Vec<_>>();
        if let [call] = calls.as_slice() {
            self.pending_continuation = Some(MessagesPendingContinuation {
                call_id: call.call_id().to_owned(),
                input: request.input.clone(),
                arguments_json: call.arguments_json().to_owned(),
            });
        }
        Ok(events)
    }

    fn continue_after_tool(
        &mut self,
        request: &ProviderRequest,
        output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.require_request_identity(request)?;
        if !self.local_echo_enabled {
            return Err(ProviderError::InvalidRequest(
                "Messages Provider has no enabled Tool continuation",
            ));
        }
        let pending = self.take_pending_continuation(output.call_id())?;
        let arguments: serde_json::Value = serde_json::from_str(&pending.arguments_json)
            .map_err(|_| ProviderError::InvalidResponse("Messages Tool input was invalid"))?;
        let max_output_bytes =
            usize::try_from(*request.config.resolved().max_output_bytes().value())
                .map_err(|_| ProviderError::InvalidConfiguration("output byte limit is invalid"))?;
        let max_output_tokens = request
            .config
            .resolved()
            .max_output_tokens()
            .map_or(MESSAGES_MAX_TOKENS, |limit| *limit.value());
        let body = serde_json::json!({
            "max_tokens": max_output_tokens,
            "messages": [
                {"role": "user", "content": pending.input},
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": pending.call_id,
                        "name": "local_echo",
                        "input": arguments,
                    }],
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": output.call_id(),
                        "content": output.output(),
                    }],
                },
            ],
            "model": request.provider.model(),
            "stream": true,
            "thinking": {"type": "disabled"},
            "tool_choice": {"type": "none"},
            "tools": [messages_local_echo_tool_definition()],
        });
        let events = self.send_request(body, max_output_bytes)?;
        if events
            .iter()
            .any(|event| matches!(event, ProviderEvent::FunctionCall(_)))
        {
            return Err(ProviderError::InvalidResponse(
                "Messages continuation returned another Tool call",
            ));
        }
        Ok(events)
    }
}

fn local_echo_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "name": "local_echo",
        "description": "Return the supplied message unchanged.",
        "parameters": {
            "type": "object",
            "properties": {
                "message": {"type": "string"},
            },
            "required": ["message"],
            "additionalProperties": false,
        },
        "strict": true,
    })
}

fn chat_local_echo_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "local_echo",
            "description": "Return the supplied message unchanged.",
            "parameters": {
                "type": "object",
                "properties": {
                    "message": {"type": "string"},
                },
                "required": ["message"],
                "additionalProperties": false,
            },
            "strict": true,
        },
    })
}

fn messages_local_echo_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": "local_echo",
        "description": "Return the supplied message unchanged.",
        "input_schema": {
            "type": "object",
            "properties": {
                "message": {"type": "string"},
            },
            "required": ["message"],
            "additionalProperties": false,
        },
    })
}

fn normalize_local_echo_calls(
    events: Vec<ProviderEvent>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    events
        .into_iter()
        .map(|event| match event {
            ProviderEvent::FunctionCall(call) if call.tool() == "local_echo" => {
                Ok(ProviderEvent::FunctionCall(ProviderToolCall::new(
                    call.call_id(),
                    "local.echo",
                    call.arguments_json(),
                )?))
            }
            ProviderEvent::FunctionCall(_) => Err(ProviderError::InvalidResponse(
                "Provider returned an unconfigured Tool",
            )),
            other => Ok(other),
        })
        .collect()
}

pub(crate) enum ConfiguredProvider<V> {
    Simulator(DeterministicProvider),
    Responses(Box<ResponsesHttpProvider<V>>),
    ChatCompletions(Box<ChatCompletionsHttpProvider<V>>),
    Messages(Box<MessagesHttpProvider<V>>),
}

impl<V: CredentialVault> ConfiguredProvider<V> {
    pub(crate) fn enable_local_echo(&mut self) {
        match self {
            Self::Responses(provider) => provider.enable_local_echo(),
            Self::ChatCompletions(provider) => provider.enable_local_echo(),
            Self::Messages(provider) => provider.enable_local_echo(),
            Self::Simulator(_) => {}
        }
    }

    pub(crate) fn for_new_turn(
        profile: Option<ProviderProfileSnapshot>,
        vault: V,
    ) -> Result<Self, ProviderError> {
        match profile {
            Some(profile) => {
                Self::for_new_turn_with_dialect(profile, ProviderDialect::Responses, vault)
            }
            None => Ok(Self::Simulator(DeterministicProvider::default())),
        }
    }

    pub(crate) fn for_new_turn_with_dialect(
        profile: ProviderProfileSnapshot,
        dialect: ProviderDialect,
        vault: V,
    ) -> Result<Self, ProviderError> {
        match dialect {
            ProviderDialect::Responses => ResponsesHttpProvider::new(profile, vault)
                .map(Box::new)
                .map(Self::Responses),
            ProviderDialect::ChatCompletions => ChatCompletionsHttpProvider::new(profile, vault)
                .map(Box::new)
                .map(Self::ChatCompletions),
            ProviderDialect::Messages => MessagesHttpProvider::new(profile, vault)
                .map(Box::new)
                .map(Self::Messages),
        }
    }

    pub(crate) fn from_epoch(epoch: &ProviderEpoch, vault: V) -> Result<Self, ProviderError> {
        match (epoch.profile(), epoch.profile_snapshot()) {
            ("simulator", None) => Ok(Self::Simulator(DeterministicProvider::default())),
            ("simulator", Some(_)) => Err(ProviderError::InvalidConfiguration(
                "simulator Provider Epoch cannot carry a Profile snapshot",
            )),
            (_, Some(profile)) => match epoch.dialect().unwrap_or(ProviderDialect::Responses) {
                ProviderDialect::Responses => ResponsesHttpProvider::new(profile.clone(), vault)
                    .map(Box::new)
                    .map(Self::Responses),
                ProviderDialect::ChatCompletions => {
                    ChatCompletionsHttpProvider::new(profile.clone(), vault)
                        .map(Box::new)
                        .map(Self::ChatCompletions)
                }
                ProviderDialect::Messages => MessagesHttpProvider::new(profile.clone(), vault)
                    .map(Box::new)
                    .map(Self::Messages),
            },
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
            Self::ChatCompletions(provider) => provider.profile_snapshot(),
            Self::Messages(provider) => provider.profile_snapshot(),
        }
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        match self {
            Self::Simulator(provider) => provider.dialect(),
            Self::Responses(provider) => provider.dialect(),
            Self::ChatCompletions(provider) => provider.dialect(),
            Self::Messages(provider) => provider.dialect(),
        }
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        match self {
            Self::Simulator(provider) => provider.run(request),
            Self::Responses(provider) => provider.run(request),
            Self::ChatCompletions(provider) => provider.run(request),
            Self::Messages(provider) => provider.run(request),
        }
    }

    fn continue_after_tool(
        &mut self,
        request: &ProviderRequest,
        output: &ProviderToolOutput,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        match self {
            Self::Simulator(provider) => provider.continue_after_tool(request, output),
            Self::Responses(provider) => provider.continue_after_tool(request, output),
            Self::ChatCompletions(provider) => provider.continue_after_tool(request, output),
            Self::Messages(provider) => provider.continue_after_tool(request, output),
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
            || profile.template() != OPENAI_COMPATIBLE_TEMPLATE
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

    fn dialect(&self) -> Option<ProviderDialect> {
        self.inner.dialect()
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

fn classify_chat_http_status(status: StatusCode) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ProviderError::InvalidConfiguration("Provider credential was rejected")
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::unavailable("Chat Completions HTTP request was temporarily rejected")
        }
        status if status.is_server_error() => {
            ProviderError::unavailable("Chat Completions HTTP service failed")
        }
        status if status.is_redirection() => {
            ProviderError::InvalidResponse("Chat Completions HTTP redirect was rejected")
        }
        status if status.is_client_error() => {
            ProviderError::InvalidRequest("Chat Completions HTTP request was rejected")
        }
        _ => ProviderError::InvalidResponse("Chat Completions HTTP status was invalid"),
    }
}

fn classify_messages_http_status(status: StatusCode) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            ProviderError::InvalidConfiguration("Provider credential was rejected")
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::unavailable("Messages HTTP request was temporarily rejected")
        }
        status if status.is_server_error() => {
            ProviderError::unavailable("Messages HTTP service failed")
        }
        status if status.is_redirection() => {
            ProviderError::InvalidResponse("Messages HTTP redirect was rejected")
        }
        status if status.is_client_error() => {
            ProviderError::InvalidRequest("Messages HTTP request was rejected")
        }
        _ => ProviderError::InvalidResponse("Messages HTTP status was invalid"),
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
template = "{OPENAI_COMPATIBLE_TEMPLATE}"
credential = "{FIXTURE_CREDENTIAL_REFERENCE}"
base_url = {base_url}
dialects = ["responses"]
allow_insecure_loopback = {allow_insecure_loopback}

[providers.{FIXTURE_PROFILE}.routes]
responses = "{FIXTURE_ROUTE}"
models = "/v1/models"

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
    parse_fixture_headers_for(headers, FIXTURE_ROUTE)
}

fn parse_fixture_headers_for(
    headers: &str,
    expected_route: &str,
) -> Result<usize, ProviderHttpError> {
    let mut lines = headers.split("\r\n");
    let expected_request_line = format!("POST {expected_route} HTTP/1.1");
    if lines.next() != Some(expected_request_line.as_str()) {
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use greentyper_core::agent_team::{
        Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope, TaskSpec,
        TeamCommand,
    };
    use greentyper_core::config::{ConfigEpoch, ConfigLayers};
    use greentyper_core::model::{ConfigEpochId, ProviderEpochId, ThreadId, TurnId};
    use greentyper_core::provider::ProviderEpoch;
    use greentyper_core::runtime::ProviderTurnOutcome;
    use greentyper_core::tool_runtime::{
        ApprovalDecision, AuthorizedToolCall, ToolEffectExecutor, ToolExecution, ToolResources,
    };
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use rustls::{ServerConfig, ServerConnection, StreamOwned};

    use crate::credential_vault::InMemoryCredentialVault;
    use crate::provider_connection::{
        ModelsHttpConnectionTester, ProviderConnectionFailureCategory,
        ProviderConnectionTestStatus, ProviderConnectionTester,
    };

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

    fn provider_request_with_dialect(
        profile: ProviderProfileSnapshot,
        input: &str,
        dialect: ProviderDialect,
    ) -> ProviderRequest {
        provider_request_with_output_tokens(profile, input, dialect, None)
    }

    fn provider_request_with_output_tokens(
        profile: ProviderProfileSnapshot,
        input: &str,
        dialect: ProviderDialect,
        max_output_tokens: impl Into<Option<u32>>,
    ) -> ProviderRequest {
        let mut layers = ConfigLayers::default();
        layers.cli.max_output_tokens = max_output_tokens.into();
        ProviderRequest {
            thread: ThreadId::new(1).expect("thread"),
            turn: TurnId::new(1).expect("turn"),
            config: ConfigEpoch::freeze(ConfigEpochId::new(1).expect("Config Epoch"), &layers)
                .expect("Config"),
            provider: ProviderEpoch::with_profile_snapshot_and_dialect(
                ProviderEpochId::new(1).expect("Provider Epoch"),
                profile.profile(),
                FIXTURE_MODEL,
                profile.clone(),
                Some(dialect),
            )
            .expect("Provider Epoch"),
            input: input.to_owned(),
        }
    }

    fn chat_fixture_profile(base_url: &str, name: &str) -> ProviderProfileSnapshot {
        let encoded_base_url = serde_json::to_string(base_url).expect("encode Chat origin");
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "chat-loopback"
model = "{FIXTURE_MODEL}"

[providers.chat-loopback]
template = "{OPENAI_COMPATIBLE_TEMPLATE}"
credential = "chat-loopback-synthetic"
base_url = {encoded_base_url}
dialects = ["chat_completions"]
allow_insecure_loopback = true

[providers.chat-loopback.routes]
chat_completions = "/v1/chat/completions"
models = "/v1/models"

[providers.chat-loopback.pricing]
source = "unknown"
"#,
        ))
        .expect("parse Chat fixture Config");
        ConfigRuntime::open(test_config_paths(name), document)
            .expect("resolve Chat fixture Config")
            .selected_provider_profile()
            .expect("resolve Chat Profile")
            .expect("external Chat Profile")
    }

    fn bound_chat_vault(profile: &ProviderProfileSnapshot) -> InMemoryCredentialVault {
        let scope = ProviderCredentialScope::from_profile(profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind Chat credential");
        vault
    }

    fn messages_fixture_profile(base_url: &str, name: &str) -> ProviderProfileSnapshot {
        let encoded_base_url = serde_json::to_string(base_url).expect("encode Messages origin");
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "messages-loopback"
model = "{FIXTURE_MODEL}"

[providers.messages-loopback]
template = "{DEEPSEEK_TEMPLATE}"
credential = "messages-loopback-synthetic"
base_url = {encoded_base_url}
dialects = ["messages"]
allow_insecure_loopback = true

[providers.messages-loopback.routes]
messages = "/anthropic/v1/messages"
models = "/models"

[providers.messages-loopback.pricing]
source = "unknown"
"#,
        ))
        .expect("parse Messages fixture Config");
        ConfigRuntime::open(test_config_paths(name), document)
            .expect("resolve Messages fixture Config")
            .selected_provider_profile()
            .expect("resolve Messages Profile")
            .expect("external Messages Profile")
    }

    fn bound_messages_vault(profile: &ProviderProfileSnapshot) -> InMemoryCredentialVault {
        let scope = ProviderCredentialScope::from_profile(profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind Messages credential");
        vault
    }

    fn read_messages_request_body(stream: &mut impl Read) -> serde_json::Value {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (body_start, content_length) = loop {
            let read = stream.read(&mut chunk).expect("read Messages request");
            assert_ne!(read, 0, "Messages request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            let Some(header_end) = find_header_end(&bytes) else {
                continue;
            };
            let headers =
                std::str::from_utf8(&bytes[..header_end]).expect("Messages request headers UTF-8");
            let mut lines = headers.split("\r\n");
            assert_eq!(lines.next(), Some("POST /anthropic/v1/messages HTTP/1.1"));
            let mut api_key = None;
            let mut anthropic_version = None;
            let mut authorization = None;
            let mut content_length = None;
            let mut content_type = None;
            for line in lines {
                let (name, value) = line.split_once(':').expect("well-formed Messages header");
                let value = value.trim();
                if name.eq_ignore_ascii_case("x-api-key") {
                    assert!(api_key.replace(value).is_none());
                } else if name.eq_ignore_ascii_case("anthropic-version") {
                    assert!(anthropic_version.replace(value).is_none());
                } else if name.eq_ignore_ascii_case("authorization") {
                    assert!(authorization.replace(value).is_none());
                } else if name.eq_ignore_ascii_case("content-length") {
                    assert!(
                        content_length
                            .replace(value.parse::<usize>().expect("Messages content length"))
                            .is_none()
                    );
                } else if name.eq_ignore_ascii_case("content-type") {
                    assert!(content_type.replace(value).is_none());
                }
            }
            assert_eq!(api_key, Some("greentyper-synthetic-provider-token-v1"));
            assert_eq!(anthropic_version, Some("2023-06-01"));
            assert_eq!(authorization, None);
            assert_eq!(content_type, Some("application/json"));
            break (
                header_end + 4,
                content_length.expect("Messages content length header"),
            );
        };
        let expected_len = body_start + content_length;
        while bytes.len() < expected_len {
            let read = stream.read(&mut chunk).expect("read Messages request body");
            assert_ne!(read, 0, "Messages request body ended early");
            bytes.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(
            bytes.len(),
            expected_len,
            "Messages request had trailing bytes"
        );
        serde_json::from_slice(&bytes[body_start..]).expect("Messages request body JSON")
    }

    fn read_request_head(stream: &mut TcpStream) -> String {
        configure_fixture_stream(stream).expect("configure connection-test fixture");
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 512];
        while find_header_end(&bytes).is_none() {
            let read = stream
                .read(&mut chunk)
                .expect("read connection-test request");
            assert!(read > 0, "connection-test request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            assert!(bytes.len() <= MAX_REQUEST_BYTES);
        }
        let header_end = find_header_end(&bytes).expect("connection-test header end");
        std::str::from_utf8(&bytes[..header_end])
            .expect("connection-test headers are UTF-8")
            .to_owned()
    }

    #[test]
    fn models_connection_test_uses_the_frozen_profile_without_exposing_credentials() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("connection-test listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let runtime =
            fixture_config_runtime(&base_url, test_config_paths("models-connection-success"))
                .expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        let expected_fingerprint = profile.fingerprint();
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind credential");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection test");
            let headers = read_request_head(&mut stream);
            assert!(headers.starts_with("GET /v1/models HTTP/1.1\r\n"));
            assert!(headers.contains(&format!("\r\nauthorization: {SYNTHETIC_AUTHORIZATION}\r\n")));
            write_fixture_response(
                &mut stream,
                "200 OK",
                "application/json",
                br#"{"data":[]}"#,
                false,
            )
            .expect("write models response");
        });

        let mut tester = ModelsHttpConnectionTester::with_timeout(&vault, HTTP_TIMEOUT);
        let outcome = tester.test(&profile);
        assert_eq!(
            outcome,
            ProviderConnectionTestStatus::Succeeded {
                profile: FIXTURE_PROFILE.to_owned(),
                fingerprint: expected_fingerprint,
            }
        );
        let encoded = serde_json::to_string(&outcome).expect("serialize connection status");
        assert!(!encoded.contains(FIXTURE_CREDENTIAL_REFERENCE));
        assert!(!encoded.contains(std::str::from_utf8(SYNTHETIC_SECRET).unwrap()));
        assert!(!encoded.contains(&base_url));
        server.join().expect("join models server");
    }

    #[test]
    fn official_openai_template_is_runnable_through_the_current_responses_adapter() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("official template listener");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let encoded_base_url = serde_json::to_string(&base_url).expect("encode fixture origin");
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "official-openai"
model = "gpt-5.6-sol"

[providers.official-openai]
template = "openai"
credential = "synthetic-official-openai-reference"
base_url = {encoded_base_url}
allow_insecure_loopback = true

[providers.official-openai.pricing]
source = "unknown"
"#,
        ))
        .expect("parse official OpenAI fixture");
        let runtime = ConfigRuntime::open(test_config_paths("official-openai-template"), document)
            .expect("resolve official OpenAI template");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve official OpenAI Profile")
            .expect("external Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");

        let mut provider_vault = InMemoryCredentialVault::default();
        provider_vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind adapter credential");
        ResponsesHttpProvider::with_timeout(profile.clone(), provider_vault, HTTP_TIMEOUT)
            .expect("official OpenAI Profile must construct Responses adapter");

        let mut probe_vault = InMemoryCredentialVault::default();
        probe_vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind probe credential");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept official models probe");
            let headers = read_request_head(&mut stream);
            assert!(headers.starts_with("GET /v1/models HTTP/1.1\r\n"));
            write_fixture_response(
                &mut stream,
                "200 OK",
                "application/json",
                br#"{"data":[]}"#,
                false,
            )
            .expect("write official models response");
        });
        let mut tester = ModelsHttpConnectionTester::with_timeout(&probe_vault, HTTP_TIMEOUT);
        assert!(matches!(
            tester.test(&profile),
            ProviderConnectionTestStatus::Succeeded { .. }
        ));
        server.join().expect("join official models server");
    }

    #[test]
    fn chat_completions_adapter_uses_the_explicit_frozen_dialect_and_route() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Chat listener");
        let address = listener.local_addr().expect("Chat listener address");
        let base_url = format!("http://{address}");
        let encoded_base_url = serde_json::to_string(&base_url).expect("encode Chat origin");
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "chat-loopback"
model = "{FIXTURE_MODEL}"

[providers.chat-loopback]
template = "{OPENAI_COMPATIBLE_TEMPLATE}"
credential = "chat-loopback-synthetic"
base_url = {encoded_base_url}
dialects = ["chat_completions"]
allow_insecure_loopback = true

[providers.chat-loopback.routes]
chat_completions = "/v1/chat/completions"
models = "/v1/models"

[providers.chat-loopback.pricing]
source = "unknown"
"#,
        ))
        .expect("parse Chat fixture Config");
        let runtime = ConfigRuntime::open(test_config_paths("chat-adapter"), document)
            .expect("resolve Chat fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve Chat Profile")
            .expect("external Chat Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind Chat credential");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Chat request");
            configure_fixture_stream(&stream).expect("configure Chat request");
            let body = read_test_request_body_for(&mut stream, "/v1/chat/completions");
            assert_eq!(
                body,
                serde_json::json!({
                    "max_completion_tokens": 2048,
                    "messages": [{"role": "user", "content": "hello Chat"}],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "stream_options": {"include_usage": true},
                })
            );
            write_fixture_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                CHAT_TEXT_SSE,
                true,
            )
            .expect("write Chat response");
        });

        let mut provider =
            ChatCompletionsHttpProvider::with_timeout(profile.clone(), vault, HTTP_TIMEOUT)
                .expect("Chat Completions provider");
        let request = provider_request_with_output_tokens(
            profile,
            "hello Chat",
            ProviderDialect::ChatCompletions,
            2_048,
        );
        let events = provider.run(&request).expect("Chat response");
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::TextDelta(first), ProviderEvent::TextDelta(second), ProviderEvent::Completed(usage)]
                if first == "Hello "
                    && second == "Chat"
                    && usage.input_tokens() == Some(4)
                    && usage.output_tokens() == Some(2)
        ));
        assert_eq!(provider.dialect(), Some(ProviderDialect::ChatCompletions));
        server.join().expect("join Chat server");
    }

    #[test]
    fn chat_completions_provider_fails_before_network_without_credential_or_dialect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Chat guard listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking Chat guard listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let profile = chat_fixture_profile(&base_url, "chat-pre-network-guards");

        assert!(matches!(
            ChatCompletionsHttpProvider::with_timeout(
                profile.clone(),
                InMemoryCredentialVault::default(),
                HTTP_TIMEOUT,
            ),
            Err(ProviderError::InvalidConfiguration(
                "Provider credential binding was not found"
            ))
        ));

        let mut provider = ChatCompletionsHttpProvider::with_timeout(
            profile.clone(),
            bound_chat_vault(&profile),
            HTTP_TIMEOUT,
        )
        .expect("Chat provider");
        let request = ProviderRequest {
            thread: ThreadId::new(1).expect("thread"),
            turn: TurnId::new(1).expect("turn"),
            config: ConfigEpoch::freeze(
                ConfigEpochId::new(1).expect("Config Epoch"),
                &ConfigLayers::default(),
            )
            .expect("Config"),
            provider: ProviderEpoch::with_profile_snapshot(
                ProviderEpochId::new(1).expect("Provider Epoch"),
                profile.profile(),
                FIXTURE_MODEL,
                profile.clone(),
            )
            .expect("Provider Epoch without dialect"),
            input: "must not send".into(),
        };
        assert!(matches!(
            provider.run(&request),
            Err(ProviderError::InvalidConfiguration(
                "Chat Completions provider identity does not match its frozen Profile and dialect"
            ))
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        let debug = format!("{provider:?}");
        assert!(!debug.contains("chat-loopback-synthetic"));
        assert!(!debug.contains(&base_url));
        assert!(!debug.contains(std::str::from_utf8(SYNTHETIC_SECRET).unwrap()));
    }

    #[test]
    fn chat_completions_http_failures_are_fixed_and_redacted() {
        for (index, status, content_type, body) in [
            (
                0,
                "503 Service Unavailable",
                "application/json",
                PRIVATE_ERROR_BODY,
            ),
            (1, "200 OK", "application/json", PRIVATE_ERROR_BODY),
            (
                2,
                "200 OK",
                "text/event-stream",
                b"data: {\"private\":\"provider-private-error-marker\"}\n\n".as_slice(),
            ),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Chat failure listener");
            let address = listener
                .local_addr()
                .expect("Chat failure listener address");
            let base_url = format!("http://{address}");
            let profile = chat_fixture_profile(&base_url, &format!("chat-failure-{index}"));
            let mut provider = ChatCompletionsHttpProvider::with_timeout(
                profile.clone(),
                bound_chat_vault(&profile),
                HTTP_TIMEOUT,
            )
            .expect("Chat provider");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept Chat failure request");
                configure_fixture_stream(&stream).expect("configure Chat failure request");
                let _body = read_test_request_body_for(&mut stream, "/v1/chat/completions");
                write_fixture_response(&mut stream, status, content_type, body, true)
                    .expect("write Chat failure response");
            });

            let request = provider_request_with_dialect(
                profile,
                "private input must not enter the error",
                ProviderDialect::ChatCompletions,
            );
            let error = provider
                .run(&request)
                .expect_err("Chat failure must not produce output");
            if index == 0 {
                assert!(matches!(error, ProviderError::Unavailable { .. }));
            } else {
                assert!(matches!(error, ProviderError::InvalidResponse(_)));
            }
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("provider-private-error-marker"));
            assert!(!rendered.contains("private input"));
            assert!(!rendered.contains("chat-loopback-synthetic"));
            assert!(!rendered.contains(std::str::from_utf8(SYNTHETIC_SECRET).unwrap()));
            server.join().expect("join Chat failure server");
        }
    }

    #[test]
    fn chat_completions_provider_continues_one_approved_function_call_over_http() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Chat Tool listener");
        let address = listener.local_addr().expect("Chat Tool listener address");
        let server = thread::spawn(move || {
            let (mut initial, _) = listener.accept().expect("accept initial Chat request");
            configure_fixture_stream(&initial).expect("configure initial Chat request");
            let initial_body = read_test_request_body_for(&mut initial, "/v1/chat/completions");
            assert_eq!(
                initial_body,
                serde_json::json!({
                    "max_completion_tokens": 6000,
                    "messages": [{
                        "role": "user",
                        "content": "echo through Chat",
                    }],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "stream_options": {"include_usage": true},
                    "tool_choice": "auto",
                    "tools": [{
                        "type": "function",
                        "function": {
                            "name": "local_echo",
                            "description": "Return the supplied message unchanged.",
                            "parameters": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"],
                                "additionalProperties": false,
                            },
                            "strict": true,
                        },
                    }],
                })
            );
            write_fixture_response(
                &mut initial,
                "200 OK",
                "text/event-stream",
                CHAT_TOOL_CALL_SSE,
                true,
            )
            .expect("write Chat Tool call response");

            let (mut continuation, _) = listener.accept().expect("accept Chat continuation");
            configure_fixture_stream(&continuation).expect("configure Chat continuation");
            let continuation_body =
                read_test_request_body_for(&mut continuation, "/v1/chat/completions");
            assert_eq!(
                continuation_body,
                serde_json::json!({
                    "max_completion_tokens": 6000,
                    "messages": [
                        {"role": "user", "content": "echo through Chat"},
                        {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_chat_echo_1",
                                "type": "function",
                                "function": {
                                    "name": "local_echo",
                                    "arguments": "{\"message\":\"tool says hi\"}",
                                },
                            }],
                        },
                        {
                            "role": "tool",
                            "tool_call_id": "call_chat_echo_1",
                            "content": "tool says hi",
                        },
                    ],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "stream_options": {"include_usage": true},
                    "tool_choice": "none",
                    "tools": [{
                        "type": "function",
                        "function": {
                            "name": "local_echo",
                            "description": "Return the supplied message unchanged.",
                            "parameters": {
                                "type": "object",
                                "properties": {"message": {"type": "string"}},
                                "required": ["message"],
                                "additionalProperties": false,
                            },
                            "strict": true,
                        },
                    }],
                })
            );
            write_fixture_response(
                &mut continuation,
                "200 OK",
                "text/event-stream",
                CHAT_TOOL_CONTINUATION_SSE,
                true,
            )
            .expect("write Chat Tool continuation response");
        });

        let base_url = format!("http://{address}");
        let encoded_base_url = serde_json::to_string(&base_url).expect("encode Chat Tool origin");
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "chat-loopback"
model = "{FIXTURE_MODEL}"

[providers.chat-loopback]
template = "{OPENAI_COMPATIBLE_TEMPLATE}"
credential = "chat-loopback-synthetic"
base_url = {encoded_base_url}
dialects = ["chat_completions"]
allow_insecure_loopback = true

[providers.chat-loopback.routes]
chat_completions = "/v1/chat/completions"
models = "/v1/models"

[providers.chat-loopback.pricing]
source = "unknown"
"#,
        ))
        .expect("parse Chat Tool fixture Config");
        let runtime = ConfigRuntime::open(test_config_paths("chat-tool"), document)
            .expect("resolve Chat Tool Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve Chat Tool Profile")
            .expect("external Chat Tool Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(SYNTHETIC_SECRET.to_vec()).expect("synthetic secret"),
            )
            .expect("bind Chat Tool credential");
        let mut provider =
            ChatCompletionsHttpProvider::with_timeout(profile, vault, Duration::from_secs(2))
                .expect("Chat Completions provider");
        provider.enable_local_echo();
        let mut layers = runtime.config_layers().expect("Config layers").clone();
        layers.cli.max_output_tokens = Some(6_000);

        let runtime_path = test_ledger_path("chat-tool-continuation", "runtime");
        let team_path = test_ledger_path("chat-tool-continuation", "team");
        let tool_path = test_ledger_path("chat-tool-continuation", "tool");
        let (mut kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
                .expect("open Kernel");
        assert!(recovery.into_sessions().is_empty());
        let operation = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "exercise one Chat HTTP Tool continuation",
                    TaskScope::from_labels(["provider-chat-tool-http"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Tool("local.echo".into()),
                    Capability::Process,
                ]),
            })
            .expect("admit root");
        kernel
            .acknowledge_team_operation(operation.operation)
            .expect("acknowledge root admission");
        let root = match operation.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        let outcome = kernel
            .execute_provider_turn(root, &layers, "echo through Chat", &mut provider, |_| {
                Ok(ToolResources::default().with_process("greentyper.local.echo.v1"))
            })
            .expect("prepare Chat Tool approval");
        let approval = match outcome {
            ProviderTurnOutcome::ApprovalRequired(approval) => approval,
            other => panic!("unexpected Provider outcome: {other:?}"),
        };
        let output = kernel
            .resolve_provider_tool_call(
                approval,
                ApprovalDecision::Grant {
                    expires_at_unix_ms: u64::MAX,
                },
                &mut EchoExecutor,
                &mut provider,
            )
            .expect("continue after Chat Tool output");
        assert_eq!(output.text(), "Echoed: tool says hi");
        assert_eq!(output.usage_records().len(), 2);
        assert_eq!(
            kernel
                .pending_provider_epoch()
                .expect("pending Chat Provider Epoch")
                .dialect(),
            Some(ProviderDialect::ChatCompletions)
        );
        server.join().expect("join Chat Tool server");

        drop(kernel);
        fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
        fs::remove_file(team_path).expect("cleanup Team Ledger");
        fs::remove_file(tool_path).expect("cleanup Tool Ledger");
    }

    #[test]
    fn messages_adapter_uses_deepseek_headers_frozen_dialect_and_route() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Messages listener");
        let address = listener.local_addr().expect("Messages listener address");
        let base_url = format!("http://{address}");
        let profile = messages_fixture_profile(&base_url, "messages-adapter");
        let vault = bound_messages_vault(&profile);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept Messages request");
            configure_fixture_stream(&stream).expect("configure Messages request");
            let body = read_messages_request_body(&mut stream);
            assert_eq!(
                body,
                serde_json::json!({
                    "max_tokens": 3072,
                    "messages": [{"role": "user", "content": "hello Messages"}],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "thinking": {"type": "disabled"},
                })
            );
            write_fixture_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                MESSAGES_TEXT_SSE,
                true,
            )
            .expect("write Messages response");
        });

        assert!(has_provider_adapter(
            DEEPSEEK_TEMPLATE,
            ProviderDialect::Messages
        ));
        assert!(!has_provider_adapter(
            "opencode-go",
            ProviderDialect::Messages
        ));
        assert!(!has_provider_adapter(
            OPENAI_TEMPLATE,
            ProviderDialect::Messages
        ));
        let mut provider = ConfiguredProvider::for_new_turn_with_dialect(
            profile.clone(),
            ProviderDialect::Messages,
            vault,
        )
        .expect("configured Messages provider");
        let request = provider_request_with_output_tokens(
            profile,
            "hello Messages",
            ProviderDialect::Messages,
            3_072,
        );
        let events = provider.run(&request).expect("Messages response");
        assert!(matches!(
            events.as_slice(),
            [ProviderEvent::TextDelta(first), ProviderEvent::TextDelta(second), ProviderEvent::Completed(usage)]
                if first == "Hello "
                    && second == "Messages"
                    && usage.input_tokens() == Some(4)
                    && usage.output_tokens() == Some(2)
        ));
        assert_eq!(provider.dialect(), Some(ProviderDialect::Messages));
        server.join().expect("join Messages server");
    }

    #[test]
    fn messages_provider_fails_before_network_without_credential_or_exact_template() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Messages guard listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking Messages guard listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let profile = messages_fixture_profile(&base_url, "messages-pre-network-guards");
        assert!(matches!(
            MessagesHttpProvider::with_timeout(
                profile.clone(),
                InMemoryCredentialVault::default(),
                HTTP_TIMEOUT,
            ),
            Err(ProviderError::InvalidConfiguration(
                "Provider credential binding was not found"
            ))
        ));

        let encoded_base_url = serde_json::to_string(&base_url).expect("encode OpenCode origin");
        let opencode = ConfigDocument::parse(&format!(
            r#"
schema_version = 1
[provider]
profile = "opencode-messages"
model = "fixture-model"
[providers.opencode-messages]
template = "opencode-go"
credential = "opencode-messages-synthetic"
base_url = {encoded_base_url}
dialects = ["messages"]
allow_insecure_loopback = true
[providers.opencode-messages.routes]
messages = "/messages"
[providers.opencode-messages.pricing]
source = "unknown"
"#,
        ))
        .expect("parse OpenCode Messages fixture");
        let opencode = ConfigRuntime::open(test_config_paths("opencode-messages-guard"), opencode)
            .expect("resolve OpenCode Messages fixture")
            .selected_provider_profile()
            .expect("resolve OpenCode Messages Profile")
            .expect("external OpenCode Messages Profile");
        assert!(matches!(
            MessagesHttpProvider::with_timeout(
                opencode,
                InMemoryCredentialVault::default(),
                HTTP_TIMEOUT,
            ),
            Err(ProviderError::InvalidConfiguration(
                "Provider Profile template has no configured runtime adapter"
            ))
        ));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn messages_pending_continuation_is_consumed_once_after_identity_matches() {
        let profile =
            messages_fixture_profile("http://127.0.0.1:9", "messages-pending-continuation");
        let mut provider = MessagesHttpProvider::with_timeout(
            profile.clone(),
            bound_messages_vault(&profile),
            HTTP_TIMEOUT,
        )
        .expect("Messages provider");
        provider.pending_continuation = Some(MessagesPendingContinuation {
            call_id: "toolu_once".into(),
            input: "input".into(),
            arguments_json: "{}".into(),
        });

        assert!(provider.take_pending_continuation("other").is_err());
        assert!(provider.pending_continuation.is_some());
        let pending = provider
            .take_pending_continuation("toolu_once")
            .expect("matching continuation");
        assert_eq!(pending.call_id, "toolu_once");
        assert!(provider.pending_continuation.is_none());
        assert!(provider.take_pending_continuation("toolu_once").is_err());
    }

    #[test]
    fn messages_http_failures_are_fixed_and_redacted() {
        for (index, status, content_type, body) in [
            (
                0,
                "503 Service Unavailable",
                "application/json",
                PRIVATE_ERROR_BODY,
            ),
            (1, "200 OK", "application/json", PRIVATE_ERROR_BODY),
            (
                2,
                "200 OK",
                "text/event-stream",
                b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"provider-private-error-marker\"}}\n\n".as_slice(),
            ),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Messages failure listener");
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let profile = messages_fixture_profile(&base_url, &format!("messages-failure-{index}"));
            let mut provider = MessagesHttpProvider::with_timeout(
                profile.clone(),
                bound_messages_vault(&profile),
                HTTP_TIMEOUT,
            )
            .expect("Messages provider");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept Messages failure request");
                configure_fixture_stream(&stream).expect("configure Messages failure request");
                let _body = read_messages_request_body(&mut stream);
                write_fixture_response(&mut stream, status, content_type, body, true)
                    .expect("write Messages failure response");
            });
            let request = provider_request_with_dialect(
                profile,
                "private input must not enter the error",
                ProviderDialect::Messages,
            );
            let error = provider
                .run(&request)
                .expect_err("Messages failure must not produce output");
            if index == 0 || index == 2 {
                assert!(matches!(error, ProviderError::Unavailable { .. }));
            } else {
                assert!(matches!(error, ProviderError::InvalidResponse(_)));
            }
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("provider-private-error-marker"));
            assert!(!rendered.contains("private input"));
            assert!(!rendered.contains("messages-loopback-synthetic"));
            assert!(!rendered.contains(std::str::from_utf8(SYNTHETIC_SECRET).unwrap()));
            server.join().expect("join Messages failure server");
        }
    }

    #[test]
    fn messages_provider_continues_one_approved_tool_use_over_http() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Messages Tool listener");
        let address = listener
            .local_addr()
            .expect("Messages Tool listener address");
        let server = thread::spawn(move || {
            let (mut initial, _) = listener.accept().expect("accept initial Messages request");
            configure_fixture_stream(&initial).expect("configure initial Messages request");
            let initial_body = read_messages_request_body(&mut initial);
            assert_eq!(
                initial_body,
                serde_json::json!({
                    "max_tokens": 6001,
                    "messages": [{"role": "user", "content": "echo through Messages"}],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "thinking": {"type": "disabled"},
                    "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
                    "tools": [{
                        "name": "local_echo",
                        "description": "Return the supplied message unchanged.",
                        "input_schema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"],
                            "additionalProperties": false,
                        },
                    }],
                })
            );
            write_fixture_response(
                &mut initial,
                "200 OK",
                "text/event-stream",
                MESSAGES_TOOL_CALL_SSE,
                true,
            )
            .expect("write Messages Tool call response");

            let (mut continuation, _) = listener.accept().expect("accept Messages continuation");
            configure_fixture_stream(&continuation)
                .expect("configure Messages continuation request");
            let continuation_body = read_messages_request_body(&mut continuation);
            assert_eq!(
                continuation_body,
                serde_json::json!({
                    "max_tokens": 6001,
                    "messages": [
                        {"role": "user", "content": "echo through Messages"},
                        {
                            "role": "assistant",
                            "content": [{
                                "type": "tool_use",
                                "id": "toolu_messages_echo_001",
                                "name": "local_echo",
                                "input": {"message": "tool says hi"},
                            }],
                        },
                        {
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": "toolu_messages_echo_001",
                                "content": "tool says hi",
                            }],
                        },
                    ],
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "thinking": {"type": "disabled"},
                    "tool_choice": {"type": "none"},
                    "tools": [{
                        "name": "local_echo",
                        "description": "Return the supplied message unchanged.",
                        "input_schema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"],
                            "additionalProperties": false,
                        },
                    }],
                })
            );
            write_fixture_response(
                &mut continuation,
                "200 OK",
                "text/event-stream",
                MESSAGES_TOOL_CONTINUATION_SSE,
                true,
            )
            .expect("write Messages Tool continuation response");
        });

        let base_url = format!("http://{address}");
        let profile = messages_fixture_profile(&base_url, "messages-tool");
        let mut provider = MessagesHttpProvider::with_timeout(
            profile.clone(),
            bound_messages_vault(&profile),
            Duration::from_secs(2),
        )
        .expect("Messages provider");
        provider.enable_local_echo();
        let mut layers = ConfigLayers::default();
        layers.cli.provider_profile = Some(profile.profile().to_owned());
        layers.cli.provider_model = Some(FIXTURE_MODEL.to_owned());
        layers.cli.max_output_tokens = Some(6_001);

        let runtime_path = test_ledger_path("messages-tool-continuation", "runtime");
        let team_path = test_ledger_path("messages-tool-continuation", "team");
        let tool_path = test_ledger_path("messages-tool-continuation", "tool");
        let (mut kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
                .expect("open Kernel");
        assert!(recovery.into_sessions().is_empty());
        let operation = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "exercise one Messages HTTP Tool continuation",
                    TaskScope::from_labels(["provider-messages-tool-http"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Tool("local.echo".into()),
                    Capability::Process,
                ]),
            })
            .expect("admit root");
        kernel
            .acknowledge_team_operation(operation.operation)
            .expect("acknowledge root admission");
        let root = match operation.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        let outcome = kernel
            .execute_provider_turn(
                root,
                &layers,
                "echo through Messages",
                &mut provider,
                |_| Ok(ToolResources::default().with_process("greentyper.local.echo.v1")),
            )
            .expect("prepare Messages Tool approval");
        let approval = match outcome {
            ProviderTurnOutcome::ApprovalRequired(approval) => approval,
            other => panic!("unexpected Provider outcome: {other:?}"),
        };
        let output = kernel
            .resolve_provider_tool_call(
                approval,
                ApprovalDecision::Grant {
                    expires_at_unix_ms: u64::MAX,
                },
                &mut EchoExecutor,
                &mut provider,
            )
            .expect("continue after Messages Tool output");
        assert_eq!(output.text(), "Echoed: tool says hi");
        assert_eq!(output.usage_records().len(), 2);
        assert_eq!(
            kernel
                .pending_provider_epoch()
                .expect("pending Messages Provider Epoch")
                .dialect(),
            Some(ProviderDialect::Messages)
        );
        server.join().expect("join Messages Tool server");

        drop(kernel);
        fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
        fs::remove_file(team_path).expect("cleanup Team Ledger");
        fs::remove_file(tool_path).expect("cleanup Tool Ledger");
    }

    #[test]
    fn catalog_templates_without_a_product_adapter_fail_before_credential_lookup() {
        let document = ConfigDocument::parse(
            r#"
schema_version = 1

[provider]
profile = "deepseek-main"
model = "deepseek-v4-flash"

[providers.deepseek-main]
template = "deepseek"
credential = "synthetic-deepseek-reference"
"#,
        )
        .expect("parse DeepSeek catalog fixture");
        let runtime = ConfigRuntime::open(test_config_paths("deepseek-adapter-gate"), document)
            .expect("resolve DeepSeek template");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve DeepSeek Profile")
            .expect("external Profile");
        let error = ResponsesHttpProvider::with_timeout(
            profile,
            InMemoryCredentialVault::default(),
            HTTP_TIMEOUT,
        )
        .expect_err("DeepSeek must not route through the OpenAI Responses adapter");
        assert_eq!(
            error,
            ProviderError::InvalidConfiguration(
                "Provider Profile template has no configured runtime adapter"
            )
        );
    }

    #[test]
    fn models_connection_test_classifies_upstream_failure_without_exposing_its_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("connection-test listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let runtime = fixture_config_runtime(
            &base_url,
            test_config_paths("models-connection-upstream-failure"),
        )
        .expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind credential");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection test");
            let _headers = read_request_head(&mut stream);
            write_fixture_response(
                &mut stream,
                "503 Service Unavailable",
                "application/json",
                PRIVATE_ERROR_BODY,
                false,
            )
            .expect("write failure response");
        });

        let mut tester = ModelsHttpConnectionTester::with_timeout(&vault, HTTP_TIMEOUT);
        let outcome = tester.test(&profile);
        assert_eq!(
            outcome,
            ProviderConnectionTestStatus::Failed {
                category: ProviderConnectionFailureCategory::Unavailable,
                retryable: true,
            }
        );
        let encoded = serde_json::to_string(&outcome).expect("serialize connection status");
        assert!(!encoded.contains(std::str::from_utf8(PRIVATE_ERROR_BODY).unwrap()));
        assert!(!encoded.contains(FIXTURE_CREDENTIAL_REFERENCE));
        assert!(!encoded.contains(&base_url));
        server.join().expect("join models server");
    }

    #[test]
    fn models_connection_test_fails_before_network_when_the_credential_is_missing() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("connection-test listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking connection-test listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let runtime = fixture_config_runtime(
            &base_url,
            test_config_paths("models-connection-missing-credential"),
        )
        .expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        let vault = InMemoryCredentialVault::default();

        let mut tester = ModelsHttpConnectionTester::with_timeout(&vault, HTTP_TIMEOUT);
        assert_eq!(
            tester.test(&profile),
            ProviderConnectionTestStatus::Failed {
                category: ProviderConnectionFailureCategory::CredentialMissing,
                retryable: false,
            }
        );
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
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
    fn mismatched_tool_output_does_not_consume_the_pending_continuation() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let runtime =
            fixture_config_runtime(&base_url, test_config_paths("mismatched-continuation-call"))
                .expect("valid fixture Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve fixture Provider Profile")
            .expect("custom Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind credential");
        let mut provider = ResponsesHttpProvider::with_timeout(profile, vault, HTTP_TIMEOUT)
            .expect("Responses Provider");
        provider.pending_continuation = Some(PendingContinuation {
            response_id: "resp_pending_1".into(),
            call_id: "call_pending_1".into(),
        });

        assert!(matches!(
            provider.take_pending_continuation("call_wrong"),
            Err(ProviderError::InvalidRequest(
                "Responses Tool output does not match the pending call"
            ))
        ));
        assert_eq!(
            provider
                .pending_continuation
                .as_ref()
                .map(|pending| pending.call_id.as_str()),
            Some("call_pending_1")
        );
        let pending = provider
            .take_pending_continuation("call_pending_1")
            .expect("consume matching continuation");
        assert_eq!(pending.response_id, "resp_pending_1");
        assert!(provider.pending_continuation.is_none());
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
        assert!(validate_provider_endpoint("https://provider.example/v1/responses", false).is_ok());
        assert!(validate_provider_endpoint("http://127.0.0.1/v1/responses", true).is_ok());
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
                validate_provider_endpoint(endpoint, allow_insecure_loopback).is_err(),
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

    #[test]
    fn responses_provider_continues_one_approved_function_call_over_http() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            let (mut initial, _) = listener.accept().expect("accept initial request");
            configure_fixture_stream(&initial).expect("configure initial request");
            let initial_body = read_test_request_body(&mut initial);
            assert_eq!(
                initial_body,
                serde_json::json!({
                    "input": "echo through the approved tool",
                    "max_output_tokens": 6002,
                    "model": FIXTURE_MODEL,
                    "stream": true,
                    "tool_choice": "auto",
                    "tools": [{
                        "type": "function",
                        "name": "local_echo",
                        "description": "Return the supplied message unchanged.",
                        "parameters": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"],
                            "additionalProperties": false,
                        },
                        "strict": true,
                    }],
                })
            );
            write_fixture_response(
                &mut initial,
                "200 OK",
                "text/event-stream",
                TOOL_CALL_SSE,
                true,
            )
            .expect("write Tool call response");

            let (mut continuation, _) = listener.accept().expect("accept continuation request");
            configure_fixture_stream(&continuation).expect("configure continuation request");
            let continuation_body = read_test_request_body(&mut continuation);
            assert_eq!(
                continuation_body,
                serde_json::json!({
                    "input": [{
                        "type": "function_call_output",
                        "call_id": "call_http_echo_1",
                        "output": "tool says hi",
                    }],
                    "max_output_tokens": 6002,
                    "model": FIXTURE_MODEL,
                    "previous_response_id": "resp_http_tool_1",
                    "stream": true,
                    "tool_choice": "none",
                    "tools": [{
                        "type": "function",
                        "name": "local_echo",
                        "description": "Return the supplied message unchanged.",
                        "parameters": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"],
                            "additionalProperties": false,
                        },
                        "strict": true,
                    }],
                })
            );
            write_fixture_response(
                &mut continuation,
                "200 OK",
                "text/event-stream",
                TOOL_CONTINUATION_SSE,
                true,
            )
            .expect("write Tool continuation response");
        });

        let base_url = format!("http://{address}");
        let runtime =
            provider_config_runtime(&base_url, true, test_config_paths("tool-continuation-http"))
                .expect("valid loopback Provider Config");
        let profile = runtime
            .selected_provider_profile()
            .expect("resolve Provider Profile")
            .expect("Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(&scope, SecretValue::new(SYNTHETIC_SECRET.to_vec()).unwrap())
            .expect("bind credential");
        let mut provider =
            ResponsesHttpProvider::with_timeout(profile, vault, Duration::from_secs(2))
                .expect("Responses provider");
        provider.enable_local_echo();

        let runtime_path = test_ledger_path("tool-continuation", "runtime");
        let team_path = test_ledger_path("tool-continuation", "team");
        let tool_path = test_ledger_path("tool-continuation", "tool");
        let (mut kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
                .expect("open Kernel");
        assert!(recovery.into_sessions().is_empty());
        let operation = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "exercise one HTTP Tool continuation",
                    TaskScope::from_labels(["provider-tool-http"]),
                ),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Tool("local.echo".into()),
                    Capability::Process,
                ]),
            })
            .expect("admit root");
        kernel
            .acknowledge_team_operation(operation.operation)
            .expect("acknowledge root admission");
        let root = match operation.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        let mut layers = runtime.config_layers().expect("Config layers").clone();
        layers.cli.max_output_tokens = Some(6_002);
        let outcome = kernel
            .execute_provider_turn(
                root,
                &layers,
                "echo through the approved tool",
                &mut provider,
                |_| Ok(ToolResources::default().with_process("greentyper.local.echo.v1")),
            )
            .expect("prepare Tool approval");
        let approval = match outcome {
            ProviderTurnOutcome::ApprovalRequired(approval) => approval,
            other => panic!("unexpected Provider outcome: {other:?}"),
        };
        let output = kernel
            .resolve_provider_tool_call(
                approval,
                ApprovalDecision::Grant {
                    expires_at_unix_ms: u64::MAX,
                },
                &mut EchoExecutor,
                &mut provider,
            )
            .expect("continue after Tool output");
        assert_eq!(output.text(), "Echoed: tool says hi");
        assert_eq!(output.usage_records().len(), 2);
        server.join().expect("join HTTP server");

        drop(kernel);
        fs::remove_file(runtime_path).expect("cleanup Runtime Ledger");
        fs::remove_file(team_path).expect("cleanup Team Ledger");
        fs::remove_file(tool_path).expect("cleanup Tool Ledger");
    }

    struct EchoExecutor;

    impl ToolEffectExecutor for EchoExecutor {
        fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
            assert_eq!(call.tool(), "local.echo");
            assert_eq!(
                call.arguments().canonical_json(),
                r#"{"message":"tool says hi"}"#
            );
            ToolExecution::Succeeded {
                output: b"tool says hi".to_vec(),
            }
        }
    }

    fn test_ledger_path(name: &str, ledger: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "greentyper-provider-http-{name}-{ledger}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn read_test_request_body(stream: &mut impl Read) -> serde_json::Value {
        read_test_request_body_for(stream, FIXTURE_ROUTE)
    }

    fn read_test_request_body_for(
        stream: &mut impl Read,
        expected_route: &str,
    ) -> serde_json::Value {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (body_start, content_length) = loop {
            let read = stream.read(&mut chunk).expect("read request");
            assert_ne!(read, 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..read]);
            let Some(header_end) = find_header_end(&bytes) else {
                continue;
            };
            let headers = std::str::from_utf8(&bytes[..header_end]).expect("request headers UTF-8");
            let content_length =
                parse_fixture_headers_for(headers, expected_route).expect("canonical headers");
            break (header_end + 4, content_length);
        };
        let expected_len = body_start + content_length;
        while bytes.len() < expected_len {
            let read = stream.read(&mut chunk).expect("read request body");
            assert_ne!(read, 0, "request body ended early");
            bytes.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(bytes.len(), expected_len, "request had trailing bytes");
        serde_json::from_slice(&bytes[body_start..]).expect("request body JSON")
    }
}
