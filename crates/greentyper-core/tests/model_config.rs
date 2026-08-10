use greentyper_core::config::{
    ConfigEpoch, ConfigError, ConfigLayer, ConfigLayers, ConfigSource, MAX_OUTPUT_TOKENS,
};
use greentyper_core::model::{
    CanonicalItem, ConfigEpochId, ItemId, ItemRole, MAX_ITEM_TEXT_BYTES, ThreadId, TurnId,
};

#[test]
fn canonical_ids_reserve_zero_and_items_require_text() {
    assert!(ThreadId::new(0).is_err());
    let turn = TurnId::new(1).expect("nonzero turn");
    let item = ItemId::new(1).expect("nonzero item");
    assert!(CanonicalItem::new(item, turn, ItemRole::User, "").is_err());
    assert_eq!(
        CanonicalItem::new(item, turn, ItemRole::User, "hello")
            .expect("valid item")
            .text(),
        "hello"
    );
    assert!(CanonicalItem::new(item, turn, ItemRole::User, "   ").is_err());
    assert!(
        CanonicalItem::new(
            item,
            turn,
            ItemRole::User,
            "x".repeat(MAX_ITEM_TEXT_BYTES + 1)
        )
        .is_err()
    );
}

#[test]
fn config_precedence_and_provenance_are_deterministic() {
    let layers = ConfigLayers {
        built_in: ConfigLayer::built_in(),
        user: ConfigLayer {
            provider_profile: Some("user-profile".to_owned()),
            max_output_bytes: Some(1_024),
            max_output_tokens: Some(1_000),
            ..ConfigLayer::default()
        },
        project: ConfigLayer {
            provider_model: Some("project-model".to_owned()),
            max_output_bytes: Some(2_048),
            max_output_tokens: Some(2_000),
            ..ConfigLayer::default()
        },
        cli: ConfigLayer {
            max_output_bytes: Some(4_096),
            max_output_tokens: Some(3_000),
            ..ConfigLayer::default()
        },
    };

    let resolved = layers.resolve().expect("valid layers");
    assert_eq!(resolved.provider_profile().value(), "user-profile");
    assert_eq!(resolved.provider_profile().source(), ConfigSource::User);
    assert_eq!(resolved.provider_model().value(), "project-model");
    assert_eq!(resolved.provider_model().source(), ConfigSource::Project);
    assert_eq!(*resolved.max_output_bytes().value(), 4_096);
    assert_eq!(resolved.max_output_bytes().source(), ConfigSource::Cli);
    assert_eq!(
        resolved.max_output_tokens().map(|value| *value.value()),
        Some(3_000)
    );
    assert_eq!(
        resolved.max_output_tokens().map(|value| value.source()),
        Some(ConfigSource::Cli)
    );
}

#[test]
fn frozen_epoch_is_immutable_when_layers_change() {
    let mut layers = ConfigLayers::default();
    layers.cli.max_output_tokens = Some(4_096);
    let id = ConfigEpochId::new(1).expect("nonzero epoch");
    let frozen = ConfigEpoch::freeze(id, &layers).expect("valid epoch");

    layers.cli.provider_model = Some("changed-after-freeze".to_owned());
    layers.cli.max_output_tokens = Some(8_192);
    let newer = ConfigEpoch::freeze(ConfigEpochId::new(2).expect("nonzero epoch"), &layers)
        .expect("valid newer epoch");

    assert_eq!(
        frozen.resolved().provider_model().value(),
        "deterministic-v1"
    );
    assert_eq!(
        frozen
            .resolved()
            .max_output_tokens()
            .map(|value| *value.value()),
        Some(4_096)
    );
    assert_ne!(frozen.fingerprint(), newer.fingerprint());
}

#[test]
fn config_validation_fails_closed() {
    let mut missing = ConfigLayers::default();
    missing.built_in.provider_model = None;
    assert_eq!(
        missing.resolve(),
        Err(ConfigError::MissingRequired("provider.model"))
    );

    let mut zero = ConfigLayers::default();
    zero.cli.max_output_bytes = Some(0);
    assert_eq!(zero.resolve(), Err(ConfigError::ZeroMaxOutputBytes));

    let mut zero_tokens = ConfigLayers::default();
    zero_tokens.cli.max_output_tokens = Some(0);
    assert_eq!(zero_tokens.resolve(), Err(ConfigError::ZeroMaxOutputTokens));

    let mut excessive_tokens = ConfigLayers::default();
    excessive_tokens.cli.max_output_tokens = Some(MAX_OUTPUT_TOKENS + 1);
    assert_eq!(
        excessive_tokens.resolve(),
        Err(ConfigError::MaxOutputTokensTooLarge)
    );

    let mut spaced = ConfigLayers::default();
    spaced.cli.provider_model = Some(" model ".to_owned());
    assert_eq!(
        spaced.resolve(),
        Err(ConfigError::SurroundingWhitespace("provider.model"))
    );

    let mut overridden_invalid = ConfigLayers::default();
    overridden_invalid.user.max_output_bytes = Some(0);
    overridden_invalid.cli.max_output_bytes = Some(1_024);
    assert_eq!(
        overridden_invalid.resolve(),
        Err(ConfigError::ZeroMaxOutputBytes)
    );
}
