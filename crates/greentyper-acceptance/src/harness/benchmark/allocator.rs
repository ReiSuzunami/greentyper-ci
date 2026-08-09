//! Process-global allocator candidate workload.

use super::*;

const FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/allocator/v1/allocation-pressure.json"
));

const ROUNDS: u32 = 24;
const LIVE_ALLOCATIONS: u32 = 192;
const MINIMUM_ALLOCATION_BYTES: u32 = 64;
const ALLOCATION_SPAN_BYTES: u32 = 32_768;
const GROW_EVERY: u32 = 3;
const GROWTH_BYTES: u32 = 257;

pub(super) fn catalog_entry() -> Option<serde_json::Value> {
    let implementation = compiled_implementation().ok()?;
    Some(serde_json::json!({
        "id": "allocator",
        "version": 1,
        "implementations": [implementation.name],
        "workloads": [{"id": "allocation-pressure", "version": 1}],
        "purpose": "candidate build isolation and deterministic allocation correctness"
    }))
}

pub(super) fn target(
    requested_implementation: &str,
    workload: &str,
) -> AppResult<Box<dyn BenchmarkTarget>> {
    let implementation = compiled_implementation()?;
    if requested_implementation != implementation.name {
        return Err(cli_error(format!(
            "allocator runner contains {}, not {requested_implementation}",
            implementation.name
        )));
    }
    if workload != "allocation-pressure" {
        return Err(cli_error(format!("unknown allocator workload {workload}")));
    }
    let fixture: AllocationFixture = serde_json::from_str(FIXTURE_JSON)?;
    validate_fixture(&fixture)?;
    Ok(Box::new(AllocatorTarget {
        fixture,
        implementation,
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompiledImplementation {
    name: &'static str,
    dependencies: &'static str,
}

fn compiled_implementation() -> AppResult<CompiledImplementation> {
    if enabled_candidate_count() != 1 {
        return Err(cli_error(
            "allocator evidence requires exactly one allocator candidate feature",
        ));
    }
    if cfg!(feature = "bench-allocator-snmalloc") {
        return Ok(CompiledImplementation {
            name: "snmalloc",
            dependencies: "snmalloc-rs=0.7.4;features=default(build_cmake,usewait-on-address);native-cpu=off;windows-msvc-cxxflags=/wd4864",
        });
    }
    if cfg!(feature = "bench-allocator-mimalloc") {
        return Ok(CompiledImplementation {
            name: "mimalloc",
            dependencies: "mimalloc=0.1.52;default-features=false;override=off",
        });
    }
    Ok(CompiledImplementation {
        name: "system",
        dependencies: "rust-std-system-allocator;candidate-dependencies=none",
    })
}

fn enabled_candidate_count() -> u8 {
    u8::from(cfg!(feature = "bench-allocator-system"))
        + u8::from(cfg!(feature = "bench-allocator-mimalloc"))
        + u8::from(cfg!(feature = "bench-allocator-snmalloc"))
}

#[derive(Clone, Debug, Deserialize)]
struct AllocationFixture {
    schema_version: u16,
    comparison_id: String,
    workload_id: String,
    workload_version: u16,
    rounds: u32,
    live_allocations: u32,
    minimum_allocation_bytes: u32,
    allocation_span_bytes: u32,
    grow_every: u32,
    growth_bytes: u32,
    expected_digest: String,
}

fn validate_fixture(fixture: &AllocationFixture) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    if fixture.comparison_id != "allocator"
        || fixture.workload_id != "allocation-pressure"
        || fixture.workload_version != 1
        || fixture.rounds != ROUNDS
        || fixture.live_allocations != LIVE_ALLOCATIONS
        || fixture.minimum_allocation_bytes != MINIMUM_ALLOCATION_BYTES
        || fixture.allocation_span_bytes != ALLOCATION_SPAN_BYTES
        || fixture.grow_every != GROW_EVERY
        || fixture.growth_bytes != GROWTH_BYTES
        || fixture.expected_digest.len() != 64
        || !fixture
            .expected_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cli_error("allocator benchmark fixture is invalid"));
    }
    Ok(())
}

struct AllocatorTarget {
    fixture: AllocationFixture,
    implementation: CompiledImplementation,
}

impl BenchmarkTarget for AllocatorTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "allocator",
            comparison_version: 1,
            implementation: self.implementation.name,
            implementation_revision: "1",
            dependencies: self.implementation.dependencies,
            workload_id: "allocation-pressure",
            workload_version: self.fixture.workload_version,
            input_shape: "24 rounds of 192 live heap blocks spanning 64-32831 bytes with deterministic growth",
            unit: "payload bytes allocated and mutated",
            boundary: "allocate, grow, mutate, sample, release, and verify a deterministic heap workload",
            process_mode: "separate-process-global-allocator",
            fixture_bytes: FIXTURE_JSON.as_bytes(),
        }
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        let observation = execute_workload(&self.fixture)?;
        if observation.output_digest != self.fixture.expected_digest {
            return Err(cli_error(
                "allocator benchmark produced an incorrect digest",
            ));
        }
        Ok(observation)
    }
}

fn execute_workload(fixture: &AllocationFixture) -> AppResult<BenchmarkObservation> {
    let mut hasher = Sha256::new();
    let mut total_payload_bytes = 0_u64;
    let mut peak_live_payload_bytes = 0_u64;
    let mut maximum_block_bytes = 0_u64;
    let mut allocation_count = 0_u64;
    let mut reallocation_count = 0_u64;
    let mut allocation_and_mutation_ns = 0_u64;
    let mut release_ns = 0_u64;

    for round in 0..fixture.rounds {
        let allocation_started = Instant::now();
        let mut live = Vec::with_capacity(usize::try_from(fixture.live_allocations)?);
        let mut live_payload_bytes = 0_u64;

        for slot in 0..fixture.live_allocations {
            let ordinal = u64::from(round)
                .checked_mul(u64::from(fixture.live_allocations))
                .and_then(|value| value.checked_add(u64::from(slot)))
                .ok_or_else(|| cli_error("allocator fixture ordinal overflow"))?;
            let mixed = ordinal
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let size = u64::from(fixture.minimum_allocation_bytes)
                .checked_add(mixed % u64::from(fixture.allocation_span_bytes))
                .ok_or_else(|| cli_error("allocator fixture size overflow"))?;
            let fill = u8::try_from((ordinal.wrapping_mul(37).wrapping_add(11)) & 0xff)?;
            let mut block = vec![fill; usize::try_from(size)?];
            allocation_count = allocation_count
                .checked_add(1)
                .ok_or_else(|| cli_error("allocator count overflow"))?;

            if ordinal % u64::from(fixture.grow_every) == 0 {
                let grown = block
                    .len()
                    .checked_add(usize::try_from(fixture.growth_bytes)?)
                    .ok_or_else(|| cli_error("allocator growth overflow"))?;
                block.resize(grown, fill ^ 0xa5);
                reallocation_count = reallocation_count
                    .checked_add(1)
                    .ok_or_else(|| cli_error("allocator reallocation count overflow"))?;
            }

            let middle = block.len() / 2;
            let last = block.len() - 1;
            block[0] ^= u8::try_from(u64::from(round) & 0xff)?;
            block[middle] ^= u8::try_from(u64::from(slot) & 0xff)?;
            block[last] ^= 0x5a;

            let block_bytes = u64::try_from(block.len())?;
            total_payload_bytes = total_payload_bytes
                .checked_add(block_bytes)
                .ok_or_else(|| cli_error("allocator payload total overflow"))?;
            live_payload_bytes = live_payload_bytes
                .checked_add(block_bytes)
                .ok_or_else(|| cli_error("allocator live payload overflow"))?;
            maximum_block_bytes = maximum_block_bytes.max(block_bytes);

            hasher.update(round.to_le_bytes());
            hasher.update(slot.to_le_bytes());
            hasher.update(block_bytes.to_le_bytes());
            hasher.update([block[0], block[middle], block[last]]);
            black_box(&block);
            live.push(block);
        }

        peak_live_payload_bytes = peak_live_payload_bytes.max(live_payload_bytes);
        allocation_and_mutation_ns = checked_elapsed_add(
            allocation_and_mutation_ns,
            allocation_started,
            "allocator allocation timing overflow",
        )?;

        let release_started = Instant::now();
        drop(live);
        release_ns = checked_elapsed_add(
            release_ns,
            release_started,
            "allocator release timing overflow",
        )?;
    }

    let digest_started = Instant::now();
    let output_digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let digest_verification_ns = u64::try_from(digest_started.elapsed().as_nanos())?;

    Ok(BenchmarkObservation {
        operation_units: total_payload_bytes,
        output_digest,
        timings_ns: BTreeMap::from([
            (
                "allocate_mutate_and_sample".into(),
                allocation_and_mutation_ns,
            ),
            ("digest_verification".into(), digest_verification_ns),
            ("release".into(), release_ns),
        ]),
        gauges: BTreeMap::from([
            ("allocation_count".into(), allocation_count),
            ("growth_count".into(), reallocation_count),
            (
                "live_allocations_per_round".into(),
                u64::from(fixture.live_allocations),
            ),
            ("maximum_block_bytes".into(), maximum_block_bytes),
            ("peak_live_payload_bytes".into(), peak_live_payload_bytes),
            ("rounds".into(), u64::from(fixture.rounds)),
            ("total_payload_bytes".into(), total_payload_bytes),
        ]),
    })
}

fn checked_elapsed_add(current: u64, started: Instant, message: &str) -> AppResult<u64> {
    current
        .checked_add(u64::try_from(started.elapsed().as_nanos())?)
        .ok_or_else(|| cli_error(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_fixture_shape_is_frozen() {
        let fixture: AllocationFixture = serde_json::from_str(FIXTURE_JSON).expect("fixture");
        validate_fixture(&fixture).expect("frozen fixture");

        let mut changed = fixture.clone();
        changed.live_allocations += 1;
        assert!(validate_fixture(&changed).is_err());
        changed = fixture.clone();
        changed.growth_bytes = 0;
        assert!(validate_fixture(&changed).is_err());
    }

    #[test]
    fn allocation_pressure_is_deterministic_and_bounded() {
        let fixture: AllocationFixture = serde_json::from_str(FIXTURE_JSON).expect("fixture");
        validate_fixture(&fixture).expect("valid fixture");
        let observation = execute_workload(&fixture).expect("workload");

        assert_eq!(observation.output_digest, fixture.expected_digest);
        assert_eq!(observation.gauges["allocation_count"], 4_608);
        assert_eq!(observation.gauges["growth_count"], 1_536);
        assert_eq!(observation.gauges["rounds"], 24);
        assert_eq!(observation.gauges["live_allocations_per_round"], 192);
        assert_eq!(observation.gauges["maximum_block_bytes"], 33_078);
        assert_eq!(observation.gauges["peak_live_payload_bytes"], 3_617_440);
        assert_eq!(observation.gauges["total_payload_bytes"], 76_742_400);
        assert_eq!(
            observation.operation_units,
            observation.gauges["total_payload_bytes"]
        );
        assert_eq!(
            observation
                .timings_ns
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "allocate_mutate_and_sample",
                "digest_verification",
                "release"
            ]
        );
    }

    #[cfg(any(
        all(
            feature = "bench-allocator-system",
            not(feature = "bench-allocator-mimalloc"),
            not(feature = "bench-allocator-snmalloc")
        ),
        all(
            not(feature = "bench-allocator-system"),
            feature = "bench-allocator-mimalloc",
            not(feature = "bench-allocator-snmalloc")
        ),
        all(
            not(feature = "bench-allocator-system"),
            not(feature = "bench-allocator-mimalloc"),
            feature = "bench-allocator-snmalloc"
        )
    ))]
    #[test]
    fn allocator_runner_only_advertises_its_compiled_implementation() {
        let implementation = compiled_implementation().expect("one implementation");
        let catalog = catalog_entry().expect("catalog entry");
        assert_eq!(
            catalog["implementations"],
            serde_json::json!([implementation.name])
        );
        let compiled_target = target(implementation.name, "allocation-pressure").expect("target");
        assert_eq!(
            compiled_target.descriptor().implementation,
            implementation.name
        );
        assert!(target("different", "allocation-pressure").is_err());
        assert!(benchmark_catalog().to_string().contains("allocator"));
    }

    #[cfg(any(
        all(
            feature = "bench-allocator-system",
            feature = "bench-allocator-mimalloc"
        ),
        all(
            feature = "bench-allocator-system",
            feature = "bench-allocator-snmalloc"
        ),
        all(
            feature = "bench-allocator-mimalloc",
            feature = "bench-allocator-snmalloc"
        )
    ))]
    #[test]
    fn combined_allocator_features_cannot_emit_candidate_evidence() {
        assert!(enabled_candidate_count() > 1);
        assert!(compiled_implementation().is_err());
        assert!(catalog_entry().is_none());
        assert!(target("snmalloc", "allocation-pressure").is_err());
        assert!(!benchmark_catalog().to_string().contains("allocator"));
    }
}
