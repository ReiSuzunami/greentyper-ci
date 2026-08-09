use greentyper_core::config::{ConfigEpoch, ConfigLayers};
use greentyper_core::model::{ConfigEpochId, ProviderEpochId, ThreadId, TurnId};
use greentyper_core::provider::{
    DeterministicProvider, ProviderEpoch, ProviderEvent, ProviderRequest, ProviderRuntime,
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
            ProviderEvent::Completed(_) => None,
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
            ProviderEvent::Completed(_) => None,
        })
        .collect::<String>();
    assert_eq!(format!("{output}\n"), SINGLE_TURN_SUCCESS);
}
