use greentyper_core::context::{
    ContextAdmissionDecision, ContextPressure, ContextPressureAccuracy, ContextPressureError,
    ContextPressureInput, ContextPressurePolicy, ContextPressureState,
    ContextPressureUnknownReason,
};

#[test]
fn context_pressure_projects_exact_soft_and_hard_boundaries() {
    let policy = ContextPressurePolicy::default();

    let normal = ContextPressure::project(
        ContextPressureInput::known(1_000, 549, 100, ContextPressureAccuracy::Exact),
        policy,
    )
    .expect("normal pressure");
    assert_eq!(normal.projected_tokens(), Some(649));
    assert_eq!(normal.occupancy_percent(), Some(64));
    assert_eq!(normal.state(), ContextPressureState::Normal);
    assert_eq!(normal.admission(), ContextAdmissionDecision::Allow);

    let soft = ContextPressure::project(
        ContextPressureInput::known(1_000, 550, 100, ContextPressureAccuracy::Exact),
        policy,
    )
    .expect("soft pressure");
    assert_eq!(soft.occupancy_percent(), Some(65));
    assert_eq!(soft.state(), ContextPressureState::Soft);
    assert_eq!(soft.admission(), ContextAdmissionDecision::Reduce);

    let hard = ContextPressure::project(
        ContextPressureInput::known(1_000, 800, 100, ContextPressureAccuracy::Exact),
        policy,
    )
    .expect("hard pressure");
    assert_eq!(hard.occupancy_percent(), Some(90));
    assert_eq!(hard.state(), ContextPressureState::Hard);
    assert_eq!(hard.admission(), ContextAdmissionDecision::Stop);
}

#[test]
fn context_pressure_preserves_estimated_and_unknown_facts() {
    let estimated = ContextPressure::project(
        ContextPressureInput::known(200_000, 100_000, 30_000, ContextPressureAccuracy::Estimated),
        ContextPressurePolicy::default(),
    )
    .expect("estimated pressure");
    assert_eq!(
        estimated.accuracy(),
        Some(ContextPressureAccuracy::Estimated)
    );
    assert_eq!(estimated.occupancy_percent(), Some(65));
    assert_eq!(estimated.state(), ContextPressureState::Soft);

    let unknown = ContextPressure::project(
        ContextPressureInput::new(
            None,
            Some(100_000),
            Some(30_000),
            Some(ContextPressureAccuracy::Exact),
        ),
        ContextPressurePolicy::default(),
    )
    .expect("unknown pressure is a valid projection");
    assert_eq!(unknown.occupancy_percent(), None);
    assert_eq!(unknown.accuracy(), None);
    assert_eq!(unknown.state(), ContextPressureState::Unknown);
    assert_eq!(unknown.admission(), ContextAdmissionDecision::Unknown);
    assert_eq!(
        unknown.unknown_reason(),
        Some(ContextPressureUnknownReason::MissingContextLimit)
    );
}

#[test]
fn context_pressure_rejects_invalid_policy_limit_and_arithmetic() {
    assert_eq!(
        ContextPressurePolicy::new(90, 65),
        Err(ContextPressureError::InvalidThresholds)
    );
    assert_eq!(
        ContextPressure::project(
            ContextPressureInput::known(0, 0, 0, ContextPressureAccuracy::Exact),
            ContextPressurePolicy::default(),
        ),
        Err(ContextPressureError::InvalidContextLimit)
    );
    assert_eq!(
        ContextPressure::project(
            ContextPressureInput::known(u64::MAX, u64::MAX, 1, ContextPressureAccuracy::Exact,),
            ContextPressurePolicy::default(),
        ),
        Err(ContextPressureError::ArithmeticOverflow)
    );
}

#[test]
fn context_pressure_caps_display_and_honors_custom_policy_boundaries() {
    let over_limit = ContextPressure::project(
        ContextPressureInput::known(1_000, 1_000, 1, ContextPressureAccuracy::Exact),
        ContextPressurePolicy::default(),
    )
    .expect("over-limit pressure");
    assert_eq!(over_limit.occupancy_percent(), Some(100));
    assert_eq!(over_limit.state(), ContextPressureState::Hard);

    let policy = ContextPressurePolicy::new(1, 100).expect("valid custom thresholds");
    let soft = ContextPressure::project(
        ContextPressureInput::known(100, 1, 0, ContextPressureAccuracy::Exact),
        policy,
    )
    .expect("custom soft boundary");
    assert_eq!(soft.state(), ContextPressureState::Soft);
    let hard = ContextPressure::project(
        ContextPressureInput::known(100, 100, 0, ContextPressureAccuracy::Exact),
        policy,
    )
    .expect("custom hard boundary");
    assert_eq!(hard.state(), ContextPressureState::Hard);
}

#[test]
fn context_pressure_freezes_every_unknown_reason_and_json_marker() {
    let cases = [
        (
            ContextPressureInput::new(None, Some(1), Some(1), Some(ContextPressureAccuracy::Exact)),
            ContextPressureUnknownReason::MissingContextLimit,
        ),
        (
            ContextPressureInput::new(
                Some(10),
                None,
                Some(1),
                Some(ContextPressureAccuracy::Exact),
            ),
            ContextPressureUnknownReason::MissingUsedTokens,
        ),
        (
            ContextPressureInput::new(
                Some(10),
                Some(1),
                None,
                Some(ContextPressureAccuracy::Exact),
            ),
            ContextPressureUnknownReason::MissingOutputReserve,
        ),
        (
            ContextPressureInput::new(Some(10), Some(1), Some(1), None),
            ContextPressureUnknownReason::MissingAccuracy,
        ),
    ];
    for (input, reason) in cases {
        let snapshot = ContextPressure::project(input, ContextPressurePolicy::default())
            .expect("unknown projection");
        assert_eq!(snapshot.unknown_reason(), Some(reason));
        assert_eq!(snapshot.accuracy(), None);
    }

    let estimated = ContextPressure::project(
        ContextPressureInput::known(100, 55, 10, ContextPressureAccuracy::Estimated),
        ContextPressurePolicy::default(),
    )
    .expect("estimated projection");
    assert_eq!(
        serde_json::to_value(estimated).expect("serialize Context Pressure"),
        serde_json::json!({
            "context_limit_tokens": 100,
            "used_tokens": 55,
            "output_reserve_tokens": 10,
            "projected_tokens": 65,
            "occupancy_percent": 65,
            "accuracy": "estimated",
            "state": "soft",
            "admission": "reduce",
            "unknown_reason": null,
            "soft_threshold_percent": 65,
            "hard_threshold_percent": 90
        })
    );
}
