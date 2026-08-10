use greentyper_core::pricing::{
    CostEstimateOutcome, CostEstimateUnknownReason, PriceSchedule, PriceScheduleBook,
    PriceScheduleDefinition, PriceScheduleSource, PricingError, TokenRates,
};
use greentyper_core::provider::{ProviderDialect, UsageAccuracy, UsageRecord};
use greentyper_core::usage::UsageTimestamp;

fn timestamp(value: i64) -> UsageTimestamp {
    UsageTimestamp::from_unix_millis(value).unwrap()
}

fn schedule(version: &str, rates: TokenRates) -> PriceSchedule {
    PriceSchedule::new(PriceScheduleDefinition {
        id: "synthetic-openai-sol".to_owned(),
        version: version.to_owned(),
        currency: "USD".to_owned(),
        provider_profile: "openai-main".to_owned(),
        model: "gpt-5.6-sol".to_owned(),
        dialect: Some(ProviderDialect::Responses),
        service_tier: Some("standard".to_owned()),
        minimum_context_tokens: 0,
        maximum_context_tokens: None,
        effective_from: timestamp(1_000),
        effective_until: None,
        source: PriceScheduleSource::Manual,
        source_ref: "synthetic-manual-rate-card".to_owned(),
        rates,
    })
    .unwrap()
}

fn complete_usage() -> UsageRecord {
    UsageRecord::new(
        Some(2_000),
        Some(500),
        Some(250),
        Some(800),
        Some(300),
        Some(2_800),
        Some("standard".to_owned()),
    )
    .unwrap()
}

#[test]
fn price_schedule_estimates_each_normalized_token_class_without_floating_point() {
    let book = PriceScheduleBook::new(vec![schedule(
        "2026-08-10.1",
        TokenRates::new(1_000_000, 500_000, 750_000, 2_000_000, 3_000_000),
    )])
    .unwrap();

    let CostEstimateOutcome::Known(estimate) = book.estimate_attempt(
        "openai-main",
        "gpt-5.6-sol",
        Some(ProviderDialect::Responses),
        timestamp(2_000),
        &complete_usage(),
    ) else {
        panic!("complete usage and one matching schedule must produce an estimate");
    };

    assert_eq!(estimate.currency(), "USD");
    assert_eq!(estimate.amount_pico_units(), 3_587_500_000);
    assert_eq!(estimate.scale_decimal_places(), 12);
    assert_eq!(estimate.usage_accuracy(), UsageAccuracy::Exact);
    assert_eq!(estimate.schedule().version(), "2026-08-10.1");
    assert_eq!(
        estimate.breakdown().uncached_input_pico_units(),
        1_250_000_000
    );
    assert_eq!(estimate.breakdown().cached_input_pico_units(), 250_000_000);
    assert_eq!(estimate.breakdown().cache_write_pico_units(), 187_500_000);
    assert_eq!(
        estimate.breakdown().visible_output_pico_units(),
        1_000_000_000
    );
    assert_eq!(
        estimate.breakdown().reasoning_output_pico_units(),
        900_000_000
    );

    let CostEstimateOutcome::Known(estimated) = book.estimate_attempt(
        "openai-main",
        "gpt-5.6-sol",
        Some(ProviderDialect::Responses),
        timestamp(2_000),
        &complete_usage().with_accuracy(UsageAccuracy::Estimated),
    ) else {
        panic!("complete estimated usage must preserve its accuracy");
    };
    assert_eq!(estimated.usage_accuracy(), UsageAccuracy::Estimated);
}

#[test]
fn price_schedule_preserves_missing_usage_and_arithmetic_overflow_as_unknown() {
    let ordinary = PriceScheduleBook::new(vec![schedule(
        "2026-08-10.1",
        TokenRates::new(1, 1, 1, 1, 1),
    )])
    .unwrap();
    let missing = UsageRecord::new(
        Some(1),
        None,
        Some(0),
        Some(1),
        Some(0),
        Some(2),
        Some("standard".to_owned()),
    )
    .unwrap();
    assert_eq!(
        ordinary.estimate_attempt(
            "openai-main",
            "gpt-5.6-sol",
            Some(ProviderDialect::Responses),
            timestamp(2_000),
            &missing,
        ),
        CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingCachedInputTokens)
    );
    let missing_input = UsageRecord::new(
        None,
        Some(0),
        Some(0),
        Some(1),
        Some(0),
        None,
        Some("standard".to_owned()),
    )
    .unwrap();
    assert_eq!(
        ordinary.estimate_attempt(
            "openai-main",
            "gpt-5.6-sol",
            Some(ProviderDialect::Responses),
            timestamp(2_000),
            &missing_input,
        ),
        CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingInputTokens)
    );
    let missing_tier =
        UsageRecord::new(Some(1), Some(0), Some(0), Some(1), Some(0), Some(2), None).unwrap();
    assert_eq!(
        ordinary.estimate_attempt(
            "openai-main",
            "gpt-5.6-sol",
            Some(ProviderDialect::Responses),
            timestamp(2_000),
            &missing_tier,
        ),
        CostEstimateOutcome::Unknown(CostEstimateUnknownReason::MissingServiceTier)
    );

    let overflowing = PriceScheduleBook::new(vec![schedule(
        "2026-08-10.2",
        TokenRates::new(u64::MAX, 0, 0, 0, 0),
    )])
    .unwrap();
    let huge = UsageRecord::new(
        Some(u64::MAX),
        Some(0),
        Some(0),
        Some(0),
        Some(0),
        Some(u64::MAX),
        Some("standard".to_owned()),
    )
    .unwrap();
    assert_eq!(
        overflowing.estimate_attempt(
            "openai-main",
            "gpt-5.6-sol",
            Some(ProviderDialect::Responses),
            timestamp(2_000),
            &huge,
        ),
        CostEstimateOutcome::Unknown(CostEstimateUnknownReason::ArithmeticOverflow)
    );
}

#[test]
fn historical_estimate_keeps_the_schedule_version_used_for_calculation() {
    let first = PriceScheduleBook::new(vec![schedule(
        "2026-08-10.1",
        TokenRates::new(1, 1, 1, 1, 1),
    )])
    .unwrap();
    let second = PriceScheduleBook::new(vec![schedule(
        "2026-08-11.1",
        TokenRates::new(2, 2, 2, 2, 2),
    )])
    .unwrap();

    let CostEstimateOutcome::Known(old_estimate) = first.estimate_attempt(
        "openai-main",
        "gpt-5.6-sol",
        Some(ProviderDialect::Responses),
        timestamp(2_000),
        &complete_usage(),
    ) else {
        panic!("first schedule must estimate");
    };
    let CostEstimateOutcome::Known(new_estimate) = second.estimate_attempt(
        "openai-main",
        "gpt-5.6-sol",
        Some(ProviderDialect::Responses),
        timestamp(2_000),
        &complete_usage(),
    ) else {
        panic!("second schedule must estimate");
    };

    assert_eq!(old_estimate.schedule().version(), "2026-08-10.1");
    assert_eq!(new_estimate.schedule().version(), "2026-08-11.1");
    assert_eq!(
        new_estimate.amount_pico_units(),
        old_estimate.amount_pico_units() * 2
    );
}

#[test]
fn schedule_identity_rejects_invalid_profiles_and_fingerprints_optional_bounds() {
    let definition = PriceScheduleDefinition {
        id: "synthetic-openai-sol".to_owned(),
        version: "2026-08-10.1".to_owned(),
        currency: "USD".to_owned(),
        provider_profile: "openai-main".to_owned(),
        model: "gpt-5.6-sol".to_owned(),
        dialect: Some(ProviderDialect::Responses),
        service_tier: None,
        minimum_context_tokens: 0,
        maximum_context_tokens: None,
        effective_from: timestamp(1_000),
        effective_until: None,
        source: PriceScheduleSource::Manual,
        source_ref: "synthetic-manual-rate-card".to_owned(),
        rates: TokenRates::new(1, 1, 1, 1, 1),
    };
    let unbounded = PriceSchedule::new(definition.clone()).unwrap();
    let bounded = PriceSchedule::new(PriceScheduleDefinition {
        maximum_context_tokens: Some(u64::MAX),
        ..definition.clone()
    })
    .unwrap();
    assert_ne!(unbounded.fingerprint(), bounded.fingerprint());

    assert_eq!(
        PriceSchedule::new(PriceScheduleDefinition {
            source: PriceScheduleSource::Template,
            ..definition.clone()
        }),
        Err(PricingError::UntrustedSource)
    );
    assert_eq!(
        PriceSchedule::new(PriceScheduleDefinition {
            provider_profile: "OpenAI Main".to_owned(),
            ..definition
        }),
        Err(PricingError::InvalidProviderProfile)
    );
}
