use greentyper_core::context::{
    ContextAdmissionDecision, ContextPressure, ContextPressureAccuracy, ContextPressureError,
    ContextPressureInput, ContextPressurePolicy, ContextPressureState,
    ContextPressureUnknownReason, ContextReductionPolicy, ContextView, ContextViewError,
    ContextViewRole, MAX_CONTEXT_VIEW_BYTES, ReducedContextView,
};
use greentyper_core::ledger::LedgerHead;
use greentyper_core::model::{CanonicalItem, ItemId, ItemRole, TurnId};

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

#[test]
fn context_view_projects_ordered_canonical_items_from_an_exact_event_range() {
    let items = vec![
        CanonicalItem::new(
            ItemId::new(1).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::User,
            "inspect the runtime",
        )
        .expect("user Item"),
        CanonicalItem::new(
            ItemId::new(2).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::Assistant,
            "runtime is ready",
        )
        .expect("Assistant Item"),
        CanonicalItem::new(
            ItemId::new(3).expect("Item"),
            TurnId::new(2).expect("Turn"),
            ItemRole::User,
            "continue",
        )
        .expect("user Item"),
    ];
    let view = ContextView::from_items(
        LedgerHead {
            transaction: 7,
            sequence: 19,
        },
        &items,
    )
    .expect("Context View");

    assert_eq!(view.source().first_sequence(), Some(1));
    assert_eq!(view.source().last_sequence(), Some(19));
    assert_eq!(view.source().transaction(), 7);
    assert_eq!(view.raw_bytes(), 43);
    assert_eq!(view.estimated_tokens(), 11);
    assert_eq!(view.items().len(), 3);
    assert_eq!(view.items()[0].item(), 1);
    assert_eq!(view.items()[0].turn(), 1);
    assert_eq!(view.items()[0].role(), ContextViewRole::User);
    assert_eq!(view.items()[0].text(), "inspect the runtime");
    assert_eq!(view.items()[1].role(), ContextViewRole::Assistant);
    assert_eq!(view.items()[2].text(), "continue");

    let empty = ContextView::from_items(LedgerHead::default(), &[]).expect("empty Context View");
    assert_eq!(empty.source().first_sequence(), None);
    assert_eq!(empty.source().last_sequence(), None);
    assert!(empty.items().is_empty());
}

#[test]
fn context_view_rejects_non_monotonic_canonical_item_ids() {
    let item = |id| {
        CanonicalItem::new(
            ItemId::new(id).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::User,
            format!("item {id}"),
        )
        .expect("canonical Item")
    };
    let head = LedgerHead {
        transaction: 1,
        sequence: 2,
    };

    assert_eq!(
        ContextView::from_items(head, &[item(2), item(1)]),
        Err(ContextViewError::InvalidStoredView)
    );
    assert_eq!(
        ContextView::from_items(head, &[item(1), item(1)]),
        Err(ContextViewError::InvalidStoredView)
    );
}

#[test]
fn context_view_rejects_cumulative_text_beyond_its_projection_boundary() {
    let oversized = "x".repeat(MAX_CONTEXT_VIEW_BYTES + 1);
    let items = [CanonicalItem::new(
        ItemId::new(1).expect("Item"),
        TurnId::new(1).expect("Turn"),
        ItemRole::User,
        oversized,
    )
    .expect("Item fits the canonical one-megabyte boundary")];

    assert_eq!(
        ContextView::from_items(
            LedgerHead {
                transaction: 1,
                sequence: 1,
            },
            &items,
        ),
        Err(ContextViewError::ViewTooLarge)
    );
}

#[test]
fn context_reduction_offloads_old_items_and_keeps_a_bounded_recent_raw_tail() {
    let items = vec![
        CanonicalItem::new(
            ItemId::new(1).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::User,
            "archived context body",
        )
        .expect("Item"),
        CanonicalItem::new(
            ItemId::new(2).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::Assistant,
            "recent answer",
        )
        .expect("Item"),
        CanonicalItem::new(
            ItemId::new(3).expect("Item"),
            TurnId::new(2).expect("Turn"),
            ItemRole::User,
            "new request",
        )
        .expect("Item"),
    ];
    let view = ContextView::from_items(
        LedgerHead {
            transaction: 4,
            sequence: 12,
        },
        &items,
    )
    .expect("Context View");
    let reduced = view
        .reduce(ContextReductionPolicy::new(24, 2).expect("policy"))
        .expect("reduced Context View");

    assert_eq!(reduced.source(), view.source());
    assert_eq!(reduced.raw_bytes(), 24);
    assert_eq!(reduced.recent_items().len(), 2);
    assert_eq!(reduced.recent_items()[0].text(), "recent answer");
    assert_eq!(reduced.recent_items()[1].text(), "new request");
    assert_eq!(reduced.artifacts().len(), 1);
    assert_eq!(reduced.artifacts()[0].item(), 1);
    assert_eq!(reduced.artifacts()[0].turn(), 1);
    assert_eq!(reduced.artifacts()[0].role(), ContextViewRole::User);
    assert_eq!(reduced.artifacts()[0].byte_len(), 21);
    assert_eq!(
        reduced.artifacts()[0].digest_hex(),
        "57d3c0cbbd188483a5d0aab6e1179bb474a515e2b04d5c1c05b3d39bb2c277a0"
    );
    assert_eq!(
        view.resolve_artifact(&reduced.artifacts()[0])
            .expect("resolve authoritative Item"),
        "archived context body"
    );
    let json = serde_json::to_string(&reduced).expect("serialize reduction");
    assert!(!json.contains("archived context body"));
    assert!(json.contains("recent answer"));
}

#[test]
fn reduced_context_materializes_only_recent_and_post_checkpoint_history_for_a_request() {
    let item = |id, turn, role, text| {
        CanonicalItem::new(
            ItemId::new(id).expect("Item"),
            TurnId::new(turn).expect("Turn"),
            role,
            text,
        )
        .expect("canonical Item")
    };
    let checkpoint_items = vec![
        item(1, 1, ItemRole::User, "old user"),
        item(2, 1, ItemRole::Assistant, "old answer"),
        item(3, 2, ItemRole::User, "recent user"),
    ];
    let reduced = ReducedContextView::from_items(
        LedgerHead {
            transaction: 4,
            sequence: 12,
        },
        &checkpoint_items,
        ContextReductionPolicy::new(11, 1).expect("policy"),
    )
    .expect("reduced Context View");
    let mut authoritative = checkpoint_items;
    authoritative.push(item(4, 2, ItemRole::Assistant, "delta answer"));

    let request = reduced
        .materialize_request(&authoritative)
        .expect("request Context View");

    assert_eq!(request.source(), reduced.source());
    assert_eq!(request.archived_items(), 2);
    assert_eq!(request.items().len(), 2);
    assert_eq!(request.items()[0].item(), 3);
    assert_eq!(request.items()[0].role(), ContextViewRole::User);
    assert_eq!(request.items()[0].text(), "recent user");
    assert_eq!(request.items()[1].item(), 4);
    assert_eq!(request.items()[1].role(), ContextViewRole::Assistant);
    assert_eq!(request.items()[1].text(), "delta answer");
    assert_eq!(request.raw_bytes(), 23);
    assert_eq!(request.estimated_tokens(), 6);
    let json = serde_json::to_string(&request).expect("serialize request Context View");
    assert!(!json.contains("old user"));
    assert!(!json.contains("old answer"));
    assert!(json.contains("recent user"));
    assert!(json.contains("delta answer"));
}

#[test]
fn reduced_context_request_omits_an_incomplete_leading_assistant_turn() {
    let item = |id, turn, role, text| {
        CanonicalItem::new(
            ItemId::new(id).expect("Item"),
            TurnId::new(turn).expect("Turn"),
            role,
            text,
        )
        .expect("canonical Item")
    };
    let checkpoint_items = vec![
        item(1, 1, ItemRole::User, "archived user"),
        item(2, 1, ItemRole::Assistant, "split assistant"),
    ];
    let reduced = ReducedContextView::from_items(
        LedgerHead {
            transaction: 2,
            sequence: 4,
        },
        &checkpoint_items,
        ContextReductionPolicy::new(32, 1).expect("policy"),
    )
    .expect("reduced Context View");
    let mut authoritative = checkpoint_items;
    authoritative.extend([
        item(3, 2, ItemRole::User, "complete user"),
        item(4, 2, ItemRole::Assistant, "complete assistant"),
    ]);

    let request = reduced
        .materialize_request(&authoritative)
        .expect("request Context View");

    assert_eq!(request.archived_items(), 2);
    assert_eq!(request.items().len(), 2);
    assert_eq!(request.items()[0].role(), ContextViewRole::User);
    assert_eq!(request.items()[0].text(), "complete user");
    assert_eq!(request.items()[1].role(), ContextViewRole::Assistant);
    assert_eq!(request.items()[1].text(), "complete assistant");
}

#[test]
fn reduced_context_request_rejects_missing_or_changed_authoritative_history() {
    let original = CanonicalItem::new(
        ItemId::new(1).expect("Item"),
        TurnId::new(1).expect("Turn"),
        ItemRole::User,
        "authoritative",
    )
    .expect("canonical Item");
    let reduced = ReducedContextView::from_items(
        LedgerHead {
            transaction: 1,
            sequence: 1,
        },
        std::slice::from_ref(&original),
        ContextReductionPolicy::new(1, 1).expect("policy"),
    )
    .expect("reduced Context View");
    let changed = CanonicalItem::new(
        ItemId::new(1).expect("Item"),
        TurnId::new(1).expect("Turn"),
        ItemRole::User,
        "changed",
    )
    .expect("canonical Item");

    assert_eq!(
        reduced.materialize_request(&[]),
        Err(ContextViewError::ArtifactMismatch)
    );
    assert_eq!(
        reduced.materialize_request(&[changed]),
        Err(ContextViewError::ArtifactMismatch)
    );
}

#[test]
fn context_reduction_rejects_invalid_limits_and_foreign_artifact_references() {
    assert_eq!(
        ContextReductionPolicy::new(MAX_CONTEXT_VIEW_BYTES + 1, 1),
        Err(ContextViewError::InvalidReductionPolicy)
    );

    let source = ContextView::from_items(
        LedgerHead {
            transaction: 1,
            sequence: 1,
        },
        &[CanonicalItem::new(
            ItemId::new(1).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::User,
            "authoritative",
        )
        .expect("Item")],
    )
    .expect("source View");
    let foreign = ContextView::from_items(
        LedgerHead {
            transaction: 1,
            sequence: 1,
        },
        &[CanonicalItem::new(
            ItemId::new(1).expect("Item"),
            TurnId::new(1).expect("Turn"),
            ItemRole::User,
            "different",
        )
        .expect("Item")],
    )
    .expect("foreign View")
    .reduce(ContextReductionPolicy::new(1, 1).expect("policy"))
    .expect("foreign reduction");

    assert_eq!(
        source.resolve_artifact(&foreign.artifacts()[0]),
        Err(ContextViewError::ArtifactMismatch)
    );
}

#[test]
fn context_reduction_offloads_history_larger_than_the_raw_view_boundary() {
    let oversized = "x".repeat(MAX_CONTEXT_VIEW_BYTES + 1);
    let items = [CanonicalItem::new(
        ItemId::new(1).expect("Item"),
        TurnId::new(1).expect("Turn"),
        ItemRole::User,
        oversized,
    )
    .expect("Item fits canonical storage")];

    let reduced = ReducedContextView::from_items(
        LedgerHead {
            transaction: 1,
            sequence: 1,
        },
        &items,
        ContextReductionPolicy::new(64, 1).expect("policy"),
    )
    .expect("oversized history is offloaded without retaining raw text");

    assert_eq!(reduced.artifacts().len(), 1);
    assert!(reduced.recent_items().is_empty());
    assert_eq!(reduced.raw_bytes(), 0);
    assert_eq!(
        reduced.artifacts()[0].byte_len(),
        MAX_CONTEXT_VIEW_BYTES as u64 + 1
    );
}
