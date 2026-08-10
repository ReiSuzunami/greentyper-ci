use greentyper_core::provider::ProviderEvent;
use greentyper_core::provider::chat_completions::{
    ChatCompletionsError, ChatCompletionsEvent, ChatCompletionsEventKind,
    ChatCompletionsSseDecoder, ChatCompletionsUsage, normalize_chat_completions_events,
};

const TEXT_AND_FUNCTION_CALL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/chat_completions/v1/text-and-function-call.sse"
));
const INCOMPLETE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/chat_completions/v1/incomplete.sse"
));
const DEEPSEEK_USAGE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/chat_completions/v1/deepseek-usage.sse"
));

fn decode(stream: &[u8]) -> Result<Vec<ChatCompletionsEvent>, ChatCompletionsError> {
    let mut decoder = ChatCompletionsSseDecoder::new(512 * 1024)?;
    for chunk in stream.chunks(13) {
        decoder.push(chunk)?;
    }
    decoder.finish()
}

fn push_data(
    decoder: &mut ChatCompletionsSseDecoder,
    data: &str,
) -> Result<(), ChatCompletionsError> {
    decoder.push(format!("data: {data}\n\n").as_bytes())
}

#[test]
fn chat_completions_sse_assembles_text_one_function_call_and_usage() {
    assert_eq!(
        decode(TEXT_AND_FUNCTION_CALL).expect("complete Chat Completions stream"),
        vec![
            ChatCompletionsEvent::new(ChatCompletionsEventKind::TextDelta {
                completion_id: "chatcmpl_fixture_1".into(),
                choice_index: 0,
                delta: "Hello ".into(),
            }),
            ChatCompletionsEvent::new(ChatCompletionsEventKind::TextDelta {
                completion_id: "chatcmpl_fixture_1".into(),
                choice_index: 0,
                delta: "中".into(),
            }),
            ChatCompletionsEvent::new(ChatCompletionsEventKind::FunctionCall {
                completion_id: "chatcmpl_fixture_1".into(),
                choice_index: 0,
                call_id: "call_fixture_1".into(),
                name: "weather".into(),
                arguments_json: "{\"city\":\"香港\",\"unit\":\"c\"}".into(),
            }),
            ChatCompletionsEvent::new(ChatCompletionsEventKind::Completed {
                completion_id: "chatcmpl_fixture_1".into(),
                usage: Some(ChatCompletionsUsage {
                    input_tokens: Some(11),
                    cached_input_tokens: Some(3),
                    output_tokens: Some(7),
                    reasoning_output_tokens: Some(2),
                    total_tokens: Some(18),
                }),
                service_tier: Some("default".into()),
            }),
        ]
    );
}

#[test]
fn chat_completions_preserves_deepseek_cache_usage_and_rejects_conflicts() {
    let events = decode(DEEPSEEK_USAGE).expect("complete DeepSeek Chat stream");
    let ChatCompletionsEventKind::Completed { usage, .. } =
        &events.last().expect("DeepSeek Chat completion event").kind
    else {
        panic!("DeepSeek Chat stream did not complete");
    };
    assert_eq!(
        *usage,
        Some(ChatCompletionsUsage {
            input_tokens: Some(11),
            cached_input_tokens: Some(3),
            output_tokens: Some(5),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(16),
        })
    );

    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_conflict","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    )
    .expect("finish choice");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_conflict","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[],"usage":{"prompt_tokens":5,"prompt_cache_hit_tokens":3,"prompt_cache_miss_tokens":2,"completion_tokens":1,"total_tokens":6,"prompt_tokens_details":{"cached_tokens":2}}}"#,
        ),
        Err(ChatCompletionsError::InvalidTransition)
    );
    assert_eq!(decoder.push(b""), Err(ChatCompletionsError::Poisoned));
}

#[test]
fn chat_completions_normalization_preserves_canonical_facts_and_redacts_incomplete_text() {
    let normalized = normalize_chat_completions_events(
        &decode(TEXT_AND_FUNCTION_CALL).expect("complete Chat Completions stream"),
    )
    .expect("normalize Chat Completions facts");
    assert_eq!(normalized.len(), 4);
    assert!(matches!(
        &normalized[0],
        ProviderEvent::TextDelta(delta) if delta == "Hello "
    ));
    assert!(matches!(
        &normalized[2],
        ProviderEvent::FunctionCall(call)
            if call.call_id() == "call_fixture_1"
                && call.tool() == "weather"
                && call.arguments_json() == "{\"city\":\"香港\",\"unit\":\"c\"}"
    ));
    let ProviderEvent::Completed(usage) = &normalized[3] else {
        panic!("normalized stream did not end in usage");
    };
    assert_eq!(usage.input_tokens(), Some(11));
    assert_eq!(usage.cached_input_tokens(), Some(3));
    assert_eq!(usage.output_tokens(), Some(7));
    assert_eq!(usage.reasoning_output_tokens(), Some(2));
    assert_eq!(usage.total_tokens(), Some(18));
    assert_eq!(usage.service_tier(), Some("default"));

    let error = normalize_chat_completions_events(
        &decode(INCOMPLETE).expect("bounded incomplete Chat Completions stream"),
    )
    .expect_err("length-limited Chat completion must not normalize as success");
    assert_eq!(error.to_string(), "provider unavailable");
    assert!(!format!("{error:?}").contains("partial"));
}

#[test]
fn chat_completions_protocol_errors_poison_the_decoder() {
    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
    )
    .expect("first chunk");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_2","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"content":"mixed"},"finish_reason":null}]}"#,
        ),
        Err(ChatCompletionsError::InvalidTransition)
    );
    assert_eq!(decoder.push(b""), Err(ChatCompletionsError::Poisoned));

    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":null},{"index":1,"delta":{},"finish_reason":null}]}"#,
        ),
        Err(ChatCompletionsError::UnsupportedChoice)
    );
}

#[test]
fn chat_completions_rejects_post_terminal_and_unterminated_streams() {
    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_done","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    )
    .expect("finished choice");
    push_data(&mut decoder, "[DONE]").expect("terminal");
    assert_eq!(
        push_data(&mut decoder, "[DONE]"),
        Err(ChatCompletionsError::EventAfterTerminal)
    );

    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_open","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"content":"open"},"finish_reason":null}]}"#,
    )
    .expect("open stream");
    assert_eq!(
        decoder.finish(),
        Err(ChatCompletionsError::IncompleteStream)
    );
}

#[test]
fn chat_completions_enforces_output_and_argument_bounds() {
    let mut decoder = ChatCompletionsSseDecoder::new(4).expect("decoder");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"content":"12345"},"finish_reason":null}]}"#,
        ),
        Err(ChatCompletionsError::OutputLimitExceeded)
    );

    let oversized_arguments = "x".repeat(64 * 1024 + 1);
    let event = format!(
        "{{\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"model-a\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{{\"name\":\"tool\",\"arguments\":\"{oversized_arguments}\"}}}}]}},\"finish_reason\":null}}]}}"
    );
    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    assert_eq!(
        push_data(&mut decoder, &event),
        Err(ChatCompletionsError::ArgumentLimitExceeded)
    );
}

#[test]
fn chat_completions_debug_output_redacts_provider_text_and_function_arguments() {
    let events = decode(TEXT_AND_FUNCTION_CALL).expect("valid response");
    let debug = format!("{events:?}");
    assert!(!debug.contains("Hello"));
    assert!(!debug.contains("city"));
    assert!(!debug.contains("香港"));
    assert!(debug.contains("delta_bytes"));
    assert!(debug.contains("arguments_bytes"));
}

#[test]
fn chat_completions_rejects_usage_reordering_and_service_tier_changes() {
    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"service_tier":"default"}"#,
    )
    .expect("finish choice");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[],"service_tier":"default","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
    )
    .expect("usage");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[],"service_tier":"default","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        ),
        Err(ChatCompletionsError::InvalidTransition)
    );

    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(
        &mut decoder,
        r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}],"service_tier":"default"}"#,
    )
    .expect("first tier");
    assert_eq!(
        push_data(
            &mut decoder,
            r#"{"id":"chatcmpl_1","object":"chat.completion.chunk","created":1,"model":"model-a","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}],"service_tier":"priority"}"#,
        ),
        Err(ChatCompletionsError::InvalidTransition)
    );
}

#[test]
fn chat_completions_rejects_excessive_argument_nesting() {
    let arguments = format!("{}0{}", "{\"a\":".repeat(65), "}".repeat(65));
    let tool_delta = serde_json::json!({
        "id": "chatcmpl_1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "model-a",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "tool", "arguments": arguments},
                }],
            },
            "finish_reason": null,
        }],
    });
    let finish = serde_json::json!({
        "id": "chatcmpl_1",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "model-a",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
    });
    let mut decoder = ChatCompletionsSseDecoder::new(1024).expect("decoder");
    push_data(&mut decoder, &tool_delta.to_string()).expect("bounded Tool delta");
    assert_eq!(
        push_data(&mut decoder, &finish.to_string()),
        Err(ChatCompletionsError::ArgumentNestingExceeded)
    );
    assert!(!format!("{decoder:?}").contains("\"a\""));
}
