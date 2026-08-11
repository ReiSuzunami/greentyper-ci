use greentyper_core::config::{ConfigEpoch, ConfigLayers};
use greentyper_core::context::{ContextReductionPolicy, ReducedContextView};
use greentyper_core::ledger::LedgerHead;
use greentyper_core::model::{
    CanonicalItem, ConfigEpochId, ItemId, ItemRole, ProviderEpochId, ThreadId, TurnId,
};
use greentyper_core::provider::{
    DeterministicProvider, ProviderEpoch, ProviderError, ProviderEvent, ProviderRequest,
    ProviderRuntime, ProviderToolCall, ProviderUnavailableStage,
};

const SINGLE_TURN_SUCCESS: &str =
    include_str!("../../../tests/fixtures/provider/v1/single-turn-success.txt");

fn request(input: &str) -> ProviderRequest {
    ProviderRequest {
        thread: ThreadId::new(1).expect("thread"),
        turn: TurnId::new(1).expect("turn"),
        config: ConfigEpoch::freeze(
            ConfigEpochId::new(1).expect("config epoch"),
            &ConfigLayers::default(),
        )
        .expect("config"),
        provider: ProviderEpoch::new(
            ProviderEpochId::new(1).expect("provider epoch"),
            "simulator",
            "deterministic-v1",
        )
        .expect("provider"),
        context: None,
        input: input.to_owned(),
    }
}

#[test]
fn simulator_is_deterministic_and_preserves_unicode_boundaries() {
    let mut first = DeterministicProvider::new("reply: ", 3).expect("simulator");
    let mut second = first.clone();
    let request = request("中🙂");
    let first_events = first.run(&request).expect("provider events");
    let second_events = second.run(&request).expect("provider events");
    assert_eq!(first_events, second_events);

    let output = first_events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            ProviderEvent::FunctionCall(_) | ProviderEvent::Completed(_) => None,
        })
        .collect::<String>();
    assert_eq!(output, "reply: 中🙂");
    assert!(matches!(
        first_events.last(),
        Some(ProviderEvent::Completed(_))
    ));
}

#[test]
fn simulator_configuration_is_bounded() {
    assert!(DeterministicProvider::new("prefix", 0).is_err());
    assert!(
        ProviderEpoch::new(
            ProviderEpochId::new(1).expect("provider epoch"),
            " simulator ",
            "model"
        )
        .is_err()
    );
}

#[test]
fn simulator_matches_the_versioned_success_fixture() {
    let mut simulator = DeterministicProvider::default();
    let events = simulator.run(&request("hello")).expect("provider events");
    let output = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::TextDelta(delta) => Some(delta.as_str()),
            ProviderEvent::FunctionCall(_) | ProviderEvent::Completed(_) => None,
        })
        .collect::<String>();
    assert_eq!(format!("{output}\n"), SINGLE_TURN_SUCCESS);
}

#[test]
fn provider_neutral_debug_redacts_text_and_tool_arguments() {
    let events = vec![
        ProviderEvent::TextDelta("private response text".into()),
        ProviderEvent::FunctionCall(
            ProviderToolCall::new(
                "private-call-id",
                "private-tool-name",
                r#"{"secret":"private argument"}"#,
            )
            .expect("Provider Tool call"),
        ),
    ];
    let debug = format!("{events:?}");
    for secret in [
        "private response text",
        "private-call-id",
        "private-tool-name",
        "private argument",
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("arguments_bytes"));
}

#[test]
fn provider_request_and_error_debug_redact_external_text() {
    let history = [CanonicalItem::new(
        ItemId::new(1).expect("Item"),
        TurnId::new(1).expect("Turn"),
        ItemRole::User,
        "private Context history",
    )
    .expect("canonical Item")];
    let reduced = ReducedContextView::from_items(
        LedgerHead {
            transaction: 1,
            sequence: 1,
        },
        &history,
        ContextReductionPolicy::new(64, 1).expect("policy"),
    )
    .expect("reduced Context View");
    let mut request = request("private user input");
    request.context = Some(
        reduced
            .materialize_request(&history)
            .expect("request Context View"),
    );
    let request_debug = format!("{request:?}");
    assert!(!request_debug.contains("private user input"));
    assert!(!request_debug.contains("private Context history"));
    assert!(request_debug.contains("context_raw_bytes"));
    assert!(request_debug.contains("input_bytes"));

    let error = ProviderError::unavailable("https://provider.test/?token=private-token");
    let error_debug = format!("{error:?}");
    assert!(!error_debug.contains("private-token"));
    assert!(error_debug.contains("BeforeResponse"));
    assert!(error_debug.contains("message_bytes"));

    let partial = ProviderError::unavailable_during(
        ProviderUnavailableStage::AfterFirstEvent,
        "private partial response",
    );
    assert_eq!(
        partial.unavailable_stage(),
        Some(ProviderUnavailableStage::AfterFirstEvent)
    );
    assert!(!format!("{partial:?}").contains("private partial response"));
}
