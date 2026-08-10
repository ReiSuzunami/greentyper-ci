use greentyper_core::provider::ProviderEvent;
use greentyper_core::provider::messages::{
    MessagesError, MessagesEvent, MessagesEventKind, MessagesSseDecoder, MessagesUsage,
    normalize_messages_events,
};

const TEXT_AND_TOOL_USE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/messages/v1/text-and-tool-use.sse"
));
const INCOMPLETE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/messages/v1/incomplete.sse"
));

fn decode(stream: &[u8]) -> Result<Vec<MessagesEvent>, MessagesError> {
    let mut decoder = MessagesSseDecoder::new(512 * 1024)?;
    for chunk in stream.chunks(11) {
        decoder.push(chunk)?;
    }
    decoder.finish()
}

fn push_event(
    decoder: &mut MessagesSseDecoder,
    event: &str,
    data: &str,
) -> Result<(), MessagesError> {
    decoder.push(format!("event: {event}\ndata: {data}\n\n").as_bytes())
}

#[test]
fn messages_sse_assembles_text_one_tool_use_and_usage() {
    assert_eq!(
        decode(TEXT_AND_TOOL_USE).expect("complete Messages stream"),
        vec![
            MessagesEvent::new(MessagesEventKind::TextDelta {
                message_id: "msg_fixture_messages_001".into(),
                block_index: 0,
                delta: "Hello ".into(),
            }),
            MessagesEvent::new(MessagesEventKind::TextDelta {
                message_id: "msg_fixture_messages_001".into(),
                block_index: 0,
                delta: "双".into(),
            }),
            MessagesEvent::new(MessagesEventKind::FunctionCall {
                message_id: "msg_fixture_messages_001".into(),
                block_index: 1,
                call_id: "toolu_fixture_001".into(),
                name: "local_echo".into(),
                arguments_json: "{\"message\":\"Hello 双\"}".into(),
            }),
            MessagesEvent::new(MessagesEventKind::Completed {
                message_id: "msg_fixture_messages_001".into(),
                usage: MessagesUsage {
                    uncached_input_tokens: Some(120),
                    cached_input_tokens: Some(30),
                    cache_write_input_tokens: Some(7),
                    output_tokens: Some(18),
                    total_tokens: Some(175),
                },
            }),
        ]
    );
}

#[test]
fn messages_normalization_preserves_canonical_facts_and_redacts_incomplete_text() {
    let normalized =
        normalize_messages_events(&decode(TEXT_AND_TOOL_USE).expect("complete Messages stream"))
            .expect("normalize Messages facts");
    assert_eq!(normalized.len(), 4);
    assert!(matches!(
        &normalized[0],
        ProviderEvent::TextDelta(delta) if delta == "Hello "
    ));
    assert!(matches!(
        &normalized[2],
        ProviderEvent::FunctionCall(call)
            if call.call_id() == "toolu_fixture_001"
                && call.tool() == "local_echo"
                && call.arguments_json() == "{\"message\":\"Hello 双\"}"
    ));
    let ProviderEvent::Completed(usage) = &normalized[3] else {
        panic!("normalized stream did not end in usage");
    };
    assert_eq!(usage.input_tokens(), Some(157));
    assert_eq!(usage.cached_input_tokens(), Some(30));
    assert_eq!(usage.cache_write_input_tokens(), Some(7));
    assert_eq!(usage.output_tokens(), Some(18));
    assert_eq!(usage.total_tokens(), Some(175));

    let error =
        normalize_messages_events(&decode(INCOMPLETE).expect("bounded incomplete Messages stream"))
            .expect_err("token-limited message must not normalize as success");
    assert_eq!(error.to_string(), "provider unavailable");
    assert!(!format!("{error:?}").contains("private partial output"));
}

#[test]
fn messages_normalizes_split_anthropic_input_accounting() {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    for (event, data) in [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_cache","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":5,"cache_read_input_tokens":100,"output_tokens":1}}}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ] {
        push_event(&mut decoder, event, data).expect("valid cached-input event");
    }
    let events = decoder.finish().expect("complete cached-input stream");
    let MessagesEventKind::Completed { usage, .. } = events[0].kind else {
        panic!("cached-input stream did not complete");
    };
    assert_eq!(usage.uncached_input_tokens, Some(2));
    assert_eq!(usage.cached_input_tokens, Some(100));
    assert_eq!(usage.cache_write_input_tokens, Some(5));
    assert_eq!(usage.total_tokens, Some(110));

    let normalized = normalize_messages_events(&events).expect("normalize cached input");
    let ProviderEvent::Completed(usage) = &normalized[0] else {
        panic!("normalized cached-input stream did not complete");
    };
    assert_eq!(usage.input_tokens(), Some(107));
    assert_eq!(usage.cached_input_tokens(), Some(100));
    assert_eq!(usage.cache_write_input_tokens(), Some(5));
    assert_eq!(usage.output_tokens(), Some(3));
    assert_eq!(usage.total_tokens(), Some(110));
}

#[test]
fn messages_preserves_uncached_input_when_cache_fields_are_absent() {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    for (event, data) in [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_no_cache","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":1}}}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ] {
        push_event(&mut decoder, event, data).expect("valid uncached-input event");
    }
    let events = decoder.finish().expect("complete uncached-input stream");
    let normalized = normalize_messages_events(&events).expect("normalize uncached input");
    let ProviderEvent::Completed(usage) = &normalized[0] else {
        panic!("normalized uncached-input stream did not complete");
    };
    assert_eq!(usage.input_tokens(), Some(4));
    assert_eq!(usage.cached_input_tokens(), None);
    assert_eq!(usage.cache_write_input_tokens(), None);
    assert_eq!(usage.output_tokens(), Some(2));
    assert_eq!(usage.total_tokens(), Some(6));
}

#[test]
fn messages_marks_a_truncated_tool_use_incomplete() {
    let mut decoder = started_tool_decoder();
    for (event, data) in [
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"message\":\"partial\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ] {
        push_event(&mut decoder, event, data).expect("valid truncated Tool stream");
    }
    let events = decoder.finish().expect("bounded incomplete Tool stream");
    assert!(matches!(
        events.as_slice(),
        [
            MessagesEvent {
                kind: MessagesEventKind::FunctionCall { .. }
            },
            MessagesEvent {
                kind: MessagesEventKind::Incomplete { reason, .. }
            }
        ] if reason == "max_tokens"
    ));
    assert!(normalize_messages_events(&events).is_err());
}

#[test]
fn messages_protocol_errors_poison_the_decoder() {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    push_event(
        &mut decoder,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    )
    .expect("message start");
    assert_eq!(
        push_event(
            &mut decoder,
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ),
        Err(MessagesError::InvalidTransition)
    );
    assert_eq!(decoder.push(b""), Err(MessagesError::Poisoned));

    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    assert_eq!(
        push_event(
            &mut decoder,
            "unknown_future_event",
            r#"{"type":"unknown_future_event"}"#
        ),
        Err(MessagesError::UnsupportedEvent)
    );
}

#[test]
fn messages_rejects_post_terminal_and_unterminated_streams() {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    for (event, data) in [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_done","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":0}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ] {
        push_event(&mut decoder, event, data).expect("valid terminal sequence");
    }
    assert_eq!(
        push_event(&mut decoder, "ping", r#"{"type":"ping"}"#),
        Err(MessagesError::EventAfterTerminal)
    );

    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    push_event(
        &mut decoder,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_open","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    )
    .expect("open message");
    assert_eq!(decoder.finish(), Err(MessagesError::IncompleteStream));
}

#[test]
fn messages_enforces_output_and_argument_bounds() {
    let mut decoder = MessagesSseDecoder::new(4).expect("decoder");
    push_event(
        &mut decoder,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    )
    .expect("message start");
    push_event(
        &mut decoder,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    )
    .expect("text start");
    assert_eq!(
        push_event(
            &mut decoder,
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"12345"}}"#,
        ),
        Err(MessagesError::OutputLimitExceeded)
    );

    let oversized_arguments = "x".repeat(64 * 1024 + 1);
    let mut decoder = started_tool_decoder();
    let event = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": oversized_arguments},
    });
    assert_eq!(
        push_event(&mut decoder, "content_block_delta", &event.to_string()),
        Err(MessagesError::ArgumentLimitExceeded)
    );
}

#[test]
fn messages_debug_output_redacts_provider_text_and_tool_arguments() {
    let events = decode(TEXT_AND_TOOL_USE).expect("valid response");
    let debug = format!("{events:?}");
    assert!(!debug.contains("Hello"));
    assert!(!debug.contains("local_echo"));
    assert!(!debug.contains("\"message\""));
    assert!(!debug.contains("双"));
    assert!(debug.contains("delta_bytes"));
    assert!(debug.contains("arguments_bytes"));
}

#[test]
fn messages_rejects_block_reordering_and_usage_changes() {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    push_event(
        &mut decoder,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    )
    .expect("message start");
    assert_eq!(
        push_event(
            &mut decoder,
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        ),
        Err(MessagesError::InvalidTransition)
    );

    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    for (event, data) in [
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
        ),
    ] {
        push_event(&mut decoder, event, data).expect("valid message event");
    }
    assert_eq!(
        push_event(
            &mut decoder,
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        ),
        Err(MessagesError::InvalidTransition)
    );
}

#[test]
fn messages_rejects_excessive_argument_nesting() {
    let arguments = format!("{}0{}", "{\"a\":".repeat(65), "}".repeat(65));
    let mut decoder = started_tool_decoder();
    let delta = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": arguments},
    });
    push_event(&mut decoder, "content_block_delta", &delta.to_string())
        .expect("bounded Tool delta");
    assert_eq!(
        push_event(
            &mut decoder,
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        Err(MessagesError::ArgumentNestingExceeded)
    );
    assert!(!format!("{decoder:?}").contains("\"a\""));
}

fn started_tool_decoder() -> MessagesSseDecoder {
    let mut decoder = MessagesSseDecoder::new(1024).expect("decoder");
    push_event(
        &mut decoder,
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","content":[],"model":"model-a","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#,
    )
    .expect("message start");
    push_event(
        &mut decoder,
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"local_echo","input":{}}}"#,
    )
    .expect("Tool start");
    decoder
}
