use greentyper_core::provider::sse::{SseError, SseEvent, SseLimits, SseParser};

#[test]
fn bounded_sse_framing_survives_fragmented_utf8_and_coalesced_events() {
    let mut parser = SseParser::new(SseLimits::new(256, 64).expect("limits"));

    parser
        .push(b": heartbeat\r\nevent: delta\r\ndata: \xe4")
        .expect("first fragment");
    assert!(
        parser
            .push_until_first_event(
                b"\xb8\xaddata\r\ndata: line two\r\n\r\nevent: done\ndata: [DONE]\n\n"
            )
            .expect("second fragment")
    );

    assert_eq!(
        parser.events(),
        &[SseEvent::new("delta", "\u{4e2d}data\nline two")]
    );
    assert_eq!(
        parser.finish().expect("complete stream"),
        vec![
            SseEvent::new("delta", "\u{4e2d}data\nline two"),
            SseEvent::new("done", "[DONE]"),
        ]
    );
}

#[test]
fn empty_event_name_defaults_to_message_and_malformed_streams_fail_closed() {
    let limits = SseLimits::new(32, 16).expect("limits");
    let mut parser = SseParser::new(limits);
    parser
        .push(b"event:\ndata: value\n\n")
        .expect("bounded event");
    assert_eq!(
        parser.finish().expect("complete stream"),
        vec![SseEvent::new("message", "value")]
    );

    assert!(SseLimits::new(0, 1).is_err());
    assert!(SseLimits::new(8, 9).is_err());

    let mut parser = SseParser::new(limits);
    assert!(parser.push(b"data: \xff\n\n").is_err());
    assert_eq!(parser.push(b"data: ignored\n\n"), Err(SseError::Poisoned));

    let mut parser = SseParser::new(limits);
    parser.push(b"data: pending").expect("bounded prefix");
    assert!(parser.finish().is_err());
}

#[test]
fn lone_cr_frames_events_and_crlf_does_not_consume_the_line_budget() {
    let limits = SseLimits::new(64, 11).expect("limits");
    let mut parser = SseParser::new(limits);
    parser.push(b"data: 12345\r\r").expect("CR-only SSE");
    assert_eq!(parser.events(), &[SseEvent::new("message", "12345")]);
    assert_eq!(
        parser.finish().expect("complete CR-only stream"),
        vec![SseEvent::new("message", "12345")]
    );

    let mut parser = SseParser::new(limits);
    parser.push(b"data: 12345\r").expect("split CRLF prefix");
    parser.push(b"\n\r\n").expect("split CRLF suffix");
    assert_eq!(
        parser.finish().expect("complete CRLF stream"),
        vec![SseEvent::new("message", "12345")]
    );
}

#[test]
fn debug_output_is_redacted_and_data_line_count_is_bounded() {
    let event = SseEvent::new("private-event-name", "confidential-provider-payload");
    let event_debug = format!("{event:?}");
    assert!(!event_debug.contains("private-event-name"));
    assert!(!event_debug.contains("confidential-provider-payload"));
    assert!(event_debug.contains("event_bytes"));
    assert!(event_debug.contains("data_bytes"));

    let limits = SseLimits::new(16 * 1024, 32).expect("limits");
    let mut parser = SseParser::new(limits);
    parser
        .push(b"event: private\ndata: confidential\n")
        .expect("partial private event");
    let parser_debug = format!("{parser:?}");
    assert!(!parser_debug.contains("private"));
    assert!(!parser_debug.contains("confidential"));

    let mut parser = SseParser::new(limits);
    let excessive_lines = "data:\n".repeat(1025);
    assert_eq!(
        parser.push(excessive_lines.as_bytes()),
        Err(SseError::DataLineLimitExceeded)
    );
    assert_eq!(parser.push(b""), Err(SseError::Poisoned));
}
