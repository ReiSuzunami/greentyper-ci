use greentyper_core::config::{
    ConfigEpoch, ConfigError, ConfigLayer, ConfigLayers, ConfigSource, ContextMode,
    MAX_OUTPUT_TOKENS, ReasoningEffort, ServiceTier,
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
fn request_policy_enums_are_closed_and_canonical() {
    for value in [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ] {
        assert_eq!(ReasoningEffort::parse(value.as_str()), Some(value));
    }
    assert_eq!(ReasoningEffort::parse("turbo"), None);

    for value in [
        ServiceTier::Auto,
        ServiceTier::Default,
        ServiceTier::Flex,
        ServiceTier::Scale,
        ServiceTier::Priority,
        ServiceTier::Fast,
    ] {
        assert_eq!(ServiceTier::parse(value.as_str()), Some(value));
    }
    assert_eq!(ServiceTier::parse("free"), None);

    for value in [ContextMode::Canonical, ContextMode::ProviderNative] {
        assert_eq!(ContextMode::parse(value.as_str()), Some(value));
    }
    assert_eq!(ContextMode::parse("automatic"), None);
}

#[test]
fn config_precedence_and_provenance_are_deterministic() {
    let layers = ConfigLayers {
        built_in: ConfigLayer::built_in(),
        user: ConfigLayer {
            provider_profile: Some("user-profile".to_owned()),
            max_output_bytes: Some(1_024),
            max_output_tokens: Some(1_000),
            reasoning_effort: Some(ReasoningEffort::Low),
            ..ConfigLayer::default()
        },
        project: ConfigLayer {
            provider_model: Some("project-model".to_owned()),
            max_output_bytes: Some(2_048),
            max_output_tokens: Some(2_000),
            service_tier: Some(ServiceTier::Flex),
            ..ConfigLayer::default()
        },
        cli: ConfigLayer {
            max_output_bytes: Some(4_096),
            max_output_tokens: Some(3_000),
            reasoning_effort: Some(ReasoningEffort::High),
            service_tier: Some(ServiceTier::Priority),
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
    assert_eq!(
        resolved.reasoning_effort().map(|value| *value.value()),
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        resolved.reasoning_effort().map(|value| value.source()),
        Some(ConfigSource::Cli)
    );
    assert_eq!(
        resolved.service_tier().map(|value| *value.value()),
        Some(ServiceTier::Priority)
    );
    assert_eq!(
        resolved.service_tier().map(|value| value.source()),
        Some(ConfigSource::Cli)
    );
}

#[test]
fn frozen_epoch_is_immutable_when_layers_change() {
    let mut layers = ConfigLayers::default();
    layers.cli.max_output_tokens = Some(4_096);
    layers.cli.reasoning_effort = Some(ReasoningEffort::Medium);
    layers.cli.service_tier = Some(ServiceTier::Default);
    let id = ConfigEpochId::new(1).expect("nonzero epoch");
    let frozen = ConfigEpoch::freeze(id, &layers).expect("valid epoch");

    layers.cli.provider_model = Some("changed-after-freeze".to_owned());
    layers.cli.max_output_tokens = Some(8_192);
    layers.cli.reasoning_effort = Some(ReasoningEffort::XHigh);
    layers.cli.service_tier = Some(ServiceTier::Fast);
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
    assert_eq!(
        frozen
            .resolved()
            .reasoning_effort()
            .map(|value| *value.value()),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        frozen.resolved().service_tier().map(|value| *value.value()),
        Some(ServiceTier::Default)
    );
    assert_ne!(frozen.fingerprint(), newer.fingerprint());
}

#[test]
fn context_mode_is_frozen_into_config_identity() {
    let baseline = ConfigEpoch::freeze(
        ConfigEpochId::new(1).expect("nonzero epoch"),
        &ConfigLayers::default(),
    )
    .expect("freeze default context mode");
    assert_eq!(
        *baseline.resolved().context_mode().value(),
        ContextMode::Canonical
    );
    assert_eq!(
        baseline.resolved().context_mode().source(),
        ConfigSource::BuiltIn
    );

    let mut native_layers = ConfigLayers::default();
    native_layers.cli.context_mode = Some(ContextMode::ProviderNative);
    let native = ConfigEpoch::freeze(
        ConfigEpochId::new(2).expect("nonzero epoch"),
        &native_layers,
    )
    .expect("freeze provider-native context mode");
    assert_eq!(
        *native.resolved().context_mode().value(),
        ContextMode::ProviderNative
    );
    assert_eq!(native.resolved().context_mode().source(), ConfigSource::Cli);
    assert_ne!(baseline.fingerprint(), native.fingerprint());
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
