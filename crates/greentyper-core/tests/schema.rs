use greentyper_core::schema::{SchemaError, SchemaKind, SchemaVersion};

#[test]
fn schema_versions_are_positive_and_explicitly_supported() {
    assert_eq!(SchemaVersion::new(1).expect("version one").get(), 1);
    assert_eq!(SchemaVersion::new(0), Err(SchemaError::ZeroVersion));

    let kind = SchemaKind::AcceptanceEvidence;
    assert_eq!(kind.require_current(1), Ok(kind.current()));
    assert_eq!(
        kind.require_current(2),
        Err(SchemaError::Unsupported {
            kind,
            supported: kind.current(),
            actual: SchemaVersion::new(2).expect("version two"),
        })
    );
}

#[test]
fn every_persisted_schema_has_an_explicit_current_version() {
    assert_eq!(SchemaKind::AcceptanceEvidence.current().get(), 1);
    assert_eq!(SchemaKind::BenchmarkEvidence.current().get(), 2);
    assert_eq!(SchemaKind::ConfigFile.current().get(), 1);
    assert_eq!(SchemaKind::ConfigEpoch.current().get(), 1);
    assert_eq!(SchemaKind::DeterministicFixture.current().get(), 1);
    assert_eq!(SchemaKind::LedgerFormat.current().get(), 1);
    assert_eq!(SchemaKind::RuntimeEvent.current().get(), 1);
    assert_eq!(SchemaKind::TeamEvent.current().get(), 2);
    assert_eq!(SchemaKind::ToolEvent.current().get(), 1);
}

#[test]
fn tool_event_schema_requires_current_and_rejects_zero_or_future_versions() {
    assert_eq!(
        SchemaKind::ToolEvent.require_current(0),
        Err(SchemaError::ZeroVersion)
    );
    assert_eq!(
        SchemaKind::ToolEvent.require_current(1),
        Ok(SchemaKind::ToolEvent.current())
    );
    assert!(matches!(
        SchemaKind::ToolEvent.require_current(2),
        Err(SchemaError::Unsupported {
            kind: SchemaKind::ToolEvent,
            ..
        })
    ));
}

#[test]
fn benchmark_schema_one_is_historical_and_not_reinterpreted_as_two() {
    let kind = SchemaKind::BenchmarkEvidence;
    assert_eq!(
        kind.require_current(1),
        Err(SchemaError::Unsupported {
            kind,
            supported: SchemaVersion::new(2).expect("version two"),
            actual: SchemaVersion::new(1).expect("version one"),
        })
    );
}

#[test]
fn team_event_schema_requires_current_and_rejects_zero_or_future_versions() {
    assert_eq!(
        SchemaKind::TeamEvent.require_current(0),
        Err(SchemaError::ZeroVersion)
    );
    assert!(matches!(
        SchemaKind::TeamEvent.require_current(1),
        Err(SchemaError::Unsupported {
            kind: SchemaKind::TeamEvent,
            ..
        })
    ));
    assert!(matches!(
        SchemaKind::TeamEvent.require_current(3),
        Err(SchemaError::Unsupported {
            kind: SchemaKind::TeamEvent,
            ..
        })
    ));
}
