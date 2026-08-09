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
fn every_phase_zero_wire_schema_starts_at_version_one() {
    assert_eq!(SchemaKind::AcceptanceEvidence.current().get(), 1);
    assert_eq!(SchemaKind::BenchmarkEvidence.current().get(), 1);
    assert_eq!(SchemaKind::DeterministicFixture.current().get(), 1);
}
