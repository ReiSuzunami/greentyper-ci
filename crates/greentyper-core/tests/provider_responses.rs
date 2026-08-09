use greentyper_core::provider::ProviderEvent;
use greentyper_core::provider::responses::{
    ResponsesError, ResponsesEvent, ResponsesEventKind, ResponsesSseDecoder, ResponsesUsage,
    normalize_responses_events,
};

const TEXT_AND_FUNCTION_CALL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/text-and-function-call.sse"
));
const FAILED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/failed.sse"
));
const INCOMPLETE: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/incomplete.sse"
));
const ERROR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/provider/responses/v1/error.sse"
));

fn decode(stream: &[u8]) -> Result<Vec<ResponsesEvent>, ResponsesError> {
    let mut decoder = ResponsesSseDecoder::new(512 * 1024)?;
    for chunk in stream.chunks(11) {
        decoder.push(chunk)?;
    }
    decoder.finish()
}

fn push_json(
    decoder: &mut ResponsesSseDecoder,
    event: &str,
    data: &str,
) -> Result<(), ResponsesError> {
    decoder.push(format!("event: {event}\ndata: {data}\n\n").as_bytes())
}

#[test]
fn responses_sse_assembles_text_and_one_canonical_function_call() {
    let mut decoder = ResponsesSseDecoder::new(512 * 1024).expect("decoder limits");
    for chunk in TEXT_AND_FUNCTION_CALL.chunks(17) {
        decoder.push(chunk).expect("Responses SSE fragment");
    }

    assert_eq!(
        decoder.finish().expect("complete Responses stream"),
        vec![
            ResponsesEvent::new(
                1,
                ResponsesEventKind::Created {
                    response_id: "resp_fixture_1".into(),
                },
            ),
            ResponsesEvent::new(
                5,
                ResponsesEventKind::TextDelta {
                    item_id: "msg_fixture_1".into(),
                    output_index: 0,
                    content_index: 0,
                    delta: "Hello ".into(),
                },
            ),
            ResponsesEvent::new(
                6,
                ResponsesEventKind::TextDelta {
                    item_id: "msg_fixture_1".into(),
                    output_index: 0,
                    content_index: 0,
                    delta: "\u{4e2d}".into(),
                },
            ),
            ResponsesEvent::new(
                14,
                ResponsesEventKind::FunctionCall {
                    item_id: "fc_fixture_1".into(),
                    output_index: 1,
                    call_id: "call_fixture_1".into(),
                    name: "weather".into(),
                    arguments_json: "{\"city\":\"\u{9999}\u{6e2f}\",\"unit\":\"c\"}".into(),
                },
            ),
            ResponsesEvent::new(
                15,
                ResponsesEventKind::Completed {
                    response_id: "resp_fixture_1".into(),
                    usage: Some(ResponsesUsage {
                        input_tokens: Some(11),
                        cached_input_tokens: Some(3),
                        cache_write_input_tokens: Some(1),
                        output_tokens: Some(7),
                        reasoning_output_tokens: Some(2),
                        total_tokens: Some(18),
                    }),
                    service_tier: Some("default".into()),
                },
            ),
        ]
    );
}

#[test]
fn responses_normalization_preserves_canonical_facts_and_redacts_terminal_failures() {
    let normalized = normalize_responses_events(
        &decode(TEXT_AND_FUNCTION_CALL).expect("complete Responses stream"),
    )
    .expect("normalize Responses facts");
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
    assert_eq!(usage.cache_write_input_tokens(), Some(1));
    assert_eq!(usage.reasoning_output_tokens(), Some(2));
    assert_eq!(usage.service_tier(), Some("default"));

    let error = normalize_responses_events(&decode(FAILED).expect("failed Responses stream"))
        .expect_err("failed Responses stream must not normalize as output");
    assert_eq!(error.to_string(), "provider unavailable");
    assert!(!error.to_string().contains("synthetic fixture failure"));
}

#[test]
fn responses_terminal_variants_preserve_known_details_without_fabricating_usage() {
    assert_eq!(
        decode(FAILED).expect("failed response"),
        vec![
            ResponsesEvent::new(
                1,
                ResponsesEventKind::Created {
                    response_id: "resp_failed_1".into(),
                },
            ),
            ResponsesEvent::new(
                4,
                ResponsesEventKind::Failed {
                    response_id: "resp_failed_1".into(),
                    code: Some("server_error".into()),
                    message: Some("synthetic fixture failure".into()),
                },
            ),
        ]
    );
    assert_eq!(
        decode(INCOMPLETE).expect("incomplete response"),
        vec![
            ResponsesEvent::new(
                8,
                ResponsesEventKind::Created {
                    response_id: "resp_incomplete_1".into(),
                },
            ),
            ResponsesEvent::new(
                12,
                ResponsesEventKind::Incomplete {
                    response_id: "resp_incomplete_1".into(),
                    reason: Some("max_output_tokens".into()),
                },
            ),
        ]
    );
    assert_eq!(
        decode(ERROR).expect("error response"),
        vec![ResponsesEvent::new(
            1,
            ResponsesEventKind::Error {
                code: None,
                message: "synthetic fixture error".into(),
                param: None,
            },
        )]
    );
}

#[test]
fn responses_protocol_errors_poison_the_decoder() {
    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    decoder
        .push(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
        )
        .expect("created");
    assert_eq!(
        decoder.push(
            b"event: response.in_progress\ndata: {\"type\":\"response.in_progress\",\"sequence_number\":2,\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
        ),
        Err(ResponsesError::SequenceNotIncreasing)
    );
    assert_eq!(decoder.push(b""), Err(ResponsesError::Poisoned));

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    assert_eq!(
        decoder.push(
            b"event: response.failed\ndata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\"}}\n\n",
        ),
        Err(ResponsesError::EventTypeMismatch)
    );
}

#[test]
fn responses_debug_output_redacts_provider_text_and_function_arguments() {
    let events = decode(TEXT_AND_FUNCTION_CALL).expect("valid response");
    let debug = format!("{events:?}");
    assert!(!debug.contains("Hello"));
    assert!(!debug.contains("city"));
    assert!(!debug.contains("\u{9999}\u{6e2f}"));
    assert!(debug.contains("delta_bytes"));
    assert!(debug.contains("arguments_bytes"));
}

#[test]
fn responses_rejects_unknown_post_terminal_and_unterminated_streams() {
    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    assert_eq!(
        push_json(
            &mut decoder,
            "response.future",
            r#"{"type":"response.future","sequence_number":1}"#,
        ),
        Err(ResponsesError::UnsupportedEvent)
    );

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    let two_events = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"sequence_number\":1,\"code\":null,\"message\":\"stop\",\"param\":null}\n\n",
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":2,\"response\":{\"id\":\"resp_late\",\"status\":\"in_progress\"}}\n\n"
    );
    assert_eq!(
        decoder.push(two_events.as_bytes()),
        Err(ResponsesError::EventAfterTerminal)
    );

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":7,"response":{"id":"resp_open","status":"in_progress"}}"#,
    )
    .expect("created");
    assert_eq!(decoder.finish(), Err(ResponsesError::IncompleteStream));
}

#[test]
fn responses_enforces_output_index_text_and_argument_bounds() {
    let mut decoder = ResponsesSseDecoder::new(4).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"resp_text","status":"in_progress"}}"#,
    )
    .expect("created");
    push_json(
        &mut decoder,
        "response.output_item.added",
        r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"msg_1","type":"message","status":"in_progress","content":[]}}"#,
    )
    .expect("message");
    push_json(
        &mut decoder,
        "response.content_part.added",
        r#"{"type":"response.content_part.added","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
    )
    .expect("content part");
    assert_eq!(
        push_json(
            &mut decoder,
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"12345"}"#,
        ),
        Err(ResponsesError::OutputLimitExceeded)
    );

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"resp_index","status":"in_progress"}}"#,
    )
    .expect("created");
    assert_eq!(
        push_json(
            &mut decoder,
            "response.output_item.added",
            r#"{"type":"response.output_item.added","sequence_number":2,"output_index":1024,"item":{"id":"msg_1","type":"message","status":"in_progress","content":[]}}"#,
        ),
        Err(ResponsesError::OutputItemLimitExceeded)
    );

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"resp_args","status":"in_progress"}}"#,
    )
    .expect("created");
    push_json(
        &mut decoder,
        "response.output_item.added",
        r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"fc_1","type":"function_call","status":"in_progress","call_id":"call_1","name":"weather","arguments":""}}"#,
    )
    .expect("function call");
    push_json(
        &mut decoder,
        "response.function_call_arguments.delta",
        r#"{"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"fc_1","output_index":0,"delta":"[]"}"#,
    )
    .expect("arguments delta");
    assert_eq!(
        push_json(
            &mut decoder,
            "response.function_call_arguments.done",
            r#"{"type":"response.function_call_arguments.done","sequence_number":4,"item_id":"fc_1","output_index":0,"name":"weather","arguments":"[]"}"#,
        ),
        Err(ResponsesError::ArgumentsNotObject)
    );
}

#[test]
fn responses_partial_usage_stays_unknown_instead_of_becoming_zero() {
    let stream = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp_usage\",\"status\":\"in_progress\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":9,\"response\":{\"id\":\"resp_usage\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"future_token_class\":13}}}\n\n"
    );
    assert_eq!(
        decode(stream.as_bytes()).expect("partial usage"),
        vec![
            ResponsesEvent::new(
                1,
                ResponsesEventKind::Created {
                    response_id: "resp_usage".into(),
                },
            ),
            ResponsesEvent::new(
                9,
                ResponsesEventKind::Completed {
                    response_id: "resp_usage".into(),
                    usage: Some(ResponsesUsage {
                        input_tokens: Some(5),
                        ..ResponsesUsage::default()
                    }),
                    service_tier: None,
                },
            ),
        ]
    );
}

#[test]
fn responses_accepts_sequence_gaps_and_enforces_the_event_limit() {
    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":2,"response":{"id":"resp_events","status":"in_progress"}}"#,
    )
    .expect("created with a nonzero starting sequence");

    for sequence_number in 4..=4098 {
        push_json(
            &mut decoder,
            "response.in_progress",
            &format!(
                "{{\"type\":\"response.in_progress\",\"sequence_number\":{sequence_number},\"response\":{{\"id\":\"resp_events\",\"status\":\"in_progress\"}}}}"
            ),
        )
        .expect("event within the configured count limit");
    }

    assert_eq!(
        push_json(
            &mut decoder,
            "response.in_progress",
            r#"{"type":"response.in_progress","sequence_number":4099,"response":{"id":"resp_events","status":"in_progress"}}"#,
        ),
        Err(ResponsesError::EventLimitExceeded)
    );
    assert_eq!(decoder.push(b""), Err(ResponsesError::Poisoned));
}

#[test]
fn responses_rejects_excessive_argument_nesting_and_redacts_partial_state() {
    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"private-response","status":"in_progress"}}"#,
    )
    .expect("created");
    push_json(
        &mut decoder,
        "response.output_item.added",
        r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"private-function","type":"function_call","status":"in_progress","call_id":"private-call","name":"private_tool","arguments":""}}"#,
    )
    .expect("function call");
    push_json(
        &mut decoder,
        "response.function_call_arguments.delta",
        r#"{"type":"response.function_call_arguments.delta","sequence_number":3,"item_id":"private-function","output_index":0,"delta":"{\"secret\":\"confidential-argument\"}"}"#,
    )
    .expect("arguments delta");

    let debug = format!("{decoder:?}");
    assert!(!debug.contains("private-response"));
    assert!(!debug.contains("private-function"));
    assert!(!debug.contains("private-call"));
    assert!(!debug.contains("private_tool"));
    assert!(!debug.contains("confidential-argument"));
    assert!(debug.contains("item_count"));

    let mut decoder = ResponsesSseDecoder::new(1024).expect("decoder");
    push_json(
        &mut decoder,
        "response.created",
        r#"{"type":"response.created","sequence_number":1,"response":{"id":"resp_depth","status":"in_progress"}}"#,
    )
    .expect("created");
    push_json(
        &mut decoder,
        "response.output_item.added",
        r#"{"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":{"id":"fc_depth","type":"function_call","status":"in_progress","call_id":"call_depth","name":"depth","arguments":""}}"#,
    )
    .expect("function call");
    let arguments = format!("{}0{}", "{\"a\":".repeat(70), "}".repeat(70));
    let delta = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "sequence_number": 3,
        "item_id": "fc_depth",
        "output_index": 0,
        "delta": arguments,
    })
    .to_string();
    push_json(
        &mut decoder,
        "response.function_call_arguments.delta",
        &delta,
    )
    .expect("bounded arguments delta");
    let done = serde_json::json!({
        "type": "response.function_call_arguments.done",
        "sequence_number": 4,
        "item_id": "fc_depth",
        "output_index": 0,
        "name": "depth",
        "arguments": arguments,
    })
    .to_string();
    assert_eq!(
        push_json(&mut decoder, "response.function_call_arguments.done", &done,),
        Err(ResponsesError::ArgumentNestingExceeded)
    );
}
