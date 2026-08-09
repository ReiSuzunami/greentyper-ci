//! Candidate-neutral benchmark orchestration and evidence.

use super::*;
use std::collections::BTreeMap;

#[cfg(feature = "bench-storage")]
mod storage;

const HARNESS_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/harness/v1/sha256-loop.json"
));
const CARGO_LOCK: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Command {
    List,
    Run(Options),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Options {
    comparison: String,
    implementation: String,
    workload: String,
    candidate_id: String,
    source_revision: String,
    output: PathBuf,
    runs: u32,
    warmup_runs: u32,
    expect_baseline: Option<String>,
    machine_identifiers: MachineIdentifierPolicy,
}

pub(super) fn parse(arguments: &[String]) -> AppResult<Command> {
    if arguments == ["list"] {
        return Ok(Command::List);
    }

    let mut comparison = None;
    let mut implementation = None;
    let mut workload = None;
    let mut candidate_id = None;
    let mut source_revision = None;
    let mut output = None;
    let mut runs = DEFAULT_RUNS;
    let mut warmup_runs = DEFAULT_WARMUP_RUNS;
    let mut expect_baseline = None;
    let mut machine_identifiers = None;

    parse_options(arguments, |name, value| match name {
        "--comparison" => set_once(&mut comparison, name, value),
        "--implementation" => set_once(&mut implementation, name, value),
        "--workload" => set_once(&mut workload, name, value),
        "--candidate-id" => set_once(&mut candidate_id, name, value),
        "--source-revision" => set_once(&mut source_revision, name, value),
        "--output" => set_once(&mut output, name, value),
        "--runs" => {
            runs = parse_run_count(name, value)?;
            Ok(())
        }
        "--warmup-runs" => {
            warmup_runs = parse_run_count(name, value)?;
            Ok(())
        }
        "--expect-baseline" => set_once(&mut expect_baseline, name, value),
        "--machine-identifiers" => set_once(&mut machine_identifiers, name, value),
        _ => Err(cli_error(format!("unknown benchmark option {name}"))),
    })?;

    let comparison = required(comparison, "--comparison")?;
    let implementation = required(implementation, "--implementation")?;
    let workload = required(workload, "--workload")?;
    let candidate_id = required(candidate_id, "--candidate-id")?;
    let source_revision = required(source_revision, "--source-revision")?;
    let output = PathBuf::from(required(output, "--output")?);
    validate_benchmark_label("comparison", &comparison)?;
    validate_benchmark_label("implementation", &implementation)?;
    validate_benchmark_label("workload", &workload)?;
    validate_candidate_id(&candidate_id)?;
    validate_source_revision(&source_revision)?;

    Ok(Command::Run(Options {
        comparison,
        implementation,
        workload,
        candidate_id,
        source_revision,
        output,
        runs,
        warmup_runs,
        expect_baseline,
        machine_identifiers: parse_machine_identifier_policy(machine_identifiers)?,
    }))
}

fn validate_benchmark_label(name: &str, value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(cli_error(format!(
            "benchmark {name} must use 1-64 lowercase ASCII letters, digits or '-'"
        )));
    }
    Ok(())
}

pub(super) fn run(command: Command) -> AppResult<()> {
    match command {
        Command::List => {
            println!("{}", serde_json::to_string_pretty(&benchmark_catalog())?);
            Ok(())
        }
        Command::Run(options) => run_benchmark(options),
    }
}

fn benchmark_catalog() -> serde_json::Value {
    let mut comparisons = vec![serde_json::json!({
        "id": "harness",
        "version": 1,
        "implementations": ["sha256-loop"],
        "workloads": [{"id": "sha256-loop", "version": 1}],
        "purpose": "benchmark pipeline integrity only"
    })];
    comparisons.extend(candidate_catalog_entries());
    serde_json::json!({"comparisons": comparisons})
}

fn candidate_catalog_entries() -> Vec<serde_json::Value> {
    #[cfg(feature = "bench-storage")]
    {
        vec![storage::catalog_entry()]
    }
    #[cfg(not(feature = "bench-storage"))]
    {
        Vec::new()
    }
}

fn run_benchmark(options: Options) -> AppResult<()> {
    let mut target = target_for(&options)?;
    let descriptor = target.descriptor();

    let cpu_guard = cpu_guard_report();
    require_cpu_guard(&cpu_guard, options.expect_baseline.as_deref())?;
    let executable_sha256 = sha256_file(&std::env::current_exe()?)?;

    for _ in 0..options.warmup_runs {
        let (_, observation) = execute_once(target.as_mut())?;
        black_box(observation);
    }

    let mut samples = Vec::with_capacity(options.runs as usize);
    for run in 1..=options.runs {
        let (operation_elapsed_ns, observation) = execute_once(target.as_mut())?;
        black_box(&observation);
        samples.push(BenchmarkSample {
            run,
            operation_elapsed_ns,
            operation_units: observation.operation_units,
            output_digest: observation.output_digest,
            timings_ns: observation.timings_ns,
            gauges: observation.gauges,
        });
    }

    let summary_samples: Vec<RawSample> = samples
        .iter()
        .map(|sample| RawSample {
            run: sample.run,
            elapsed_ns: sample.operation_elapsed_ns,
        })
        .collect();
    let timing_summaries = summarize_timing_samples(&samples)?;
    let gauge_summaries = summarize_gauge_samples(&samples)?;
    let generated_at_unix_ms =
        u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
            .map_err(|_| cli_error("system time exceeds benchmark evidence representation"))?;

    let evidence = BenchmarkEvidence {
        schema_version: SchemaKind::BenchmarkEvidence.current().get(),
        generated_at_unix_ms,
        candidate_id: options.candidate_id,
        source_revision: options.source_revision,
        executable_sha256,
        configuration_sha256: sha256_bytes(descriptor.fixture_bytes),
        comparison: ComparisonIdentity {
            id: descriptor.comparison_id.into(),
            version: descriptor.comparison_version,
        },
        implementation: ImplementationIdentity {
            name: descriptor.implementation.into(),
            revision: descriptor.implementation_revision.into(),
            dependency_fingerprint: dependency_fingerprint(descriptor.dependencies),
        },
        workload: WorkloadIdentity {
            id: descriptor.workload_id.into(),
            version: descriptor.workload_version,
            fixture_sha256: sha256_bytes(descriptor.fixture_bytes),
            input_shape: descriptor.input_shape.into(),
            unit: descriptor.unit.into(),
            boundary: descriptor.boundary.into(),
            process_mode: descriptor.process_mode.into(),
        },
        compiled_cpu_baseline: cpu_guard.compiled_baseline.clone(),
        cpu_guard,
        machine: machine_fingerprint(options.machine_identifiers),
        warmup_runs: options.warmup_runs,
        samples,
        summary: summarize(&summary_samples)?,
        timing_summaries,
        gauge_summaries,
    };

    write_new_json(&options.output, &evidence)?;
    println!("{}", options.output.display());
    Ok(())
}

fn execute_once(target: &mut dyn BenchmarkTarget) -> AppResult<(u64, BenchmarkObservation)> {
    target.prepare_run()?;
    let started = Instant::now();
    let operation = target.run_once();
    let elapsed = u64::try_from(started.elapsed().as_nanos())
        .map_err(|_| cli_error("benchmark duration exceeds u64 nanoseconds"));
    let cleanup = target.cleanup_run();

    match (operation, elapsed, cleanup) {
        (Ok(observation), Ok(elapsed), Ok(())) => Ok((elapsed, observation)),
        (Err(error), _, _) => Err(error),
        (_, Err(error), _) => Err(error),
        (_, _, Err(error)) => Err(error),
    }
}

fn summarize_timing_samples(
    samples: &[BenchmarkSample],
) -> AppResult<BTreeMap<String, SampleSummary>> {
    let Some(first) = samples.first() else {
        return Err(cli_error("cannot summarize empty benchmark timings"));
    };
    if samples
        .iter()
        .any(|sample| !sample.timings_ns.keys().eq(first.timings_ns.keys()))
    {
        return Err(cli_error("benchmark timing sets changed between samples"));
    }
    let mut summaries = BTreeMap::new();
    for key in first.timings_ns.keys() {
        let values: Vec<RawSample> = samples
            .iter()
            .map(|sample| RawSample {
                run: sample.run,
                elapsed_ns: sample.timings_ns[key],
            })
            .collect();
        summaries.insert(key.clone(), summarize(&values)?);
    }
    Ok(summaries)
}

fn summarize_gauge_samples(
    samples: &[BenchmarkSample],
) -> AppResult<BTreeMap<String, GaugeSummary>> {
    let Some(first) = samples.first() else {
        return Err(cli_error("cannot summarize empty benchmark gauges"));
    };
    if samples
        .iter()
        .any(|sample| !sample.gauges.keys().eq(first.gauges.keys()))
    {
        return Err(cli_error("benchmark gauge sets changed between samples"));
    }
    let mut summaries = BTreeMap::new();
    for key in first.gauges.keys() {
        let values: Vec<u64> = samples.iter().map(|sample| sample.gauges[key]).collect();
        summaries.insert(key.clone(), summarize_gauges(&values)?);
    }
    Ok(summaries)
}

fn summarize_gauges(values: &[u64]) -> AppResult<GaugeSummary> {
    if values.is_empty() {
        return Err(cli_error("cannot summarize an empty gauge set"));
    }
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    let mean = ordered.iter().map(|value| *value as f64).sum::<f64>() / ordered.len() as f64;
    let variance = ordered
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / ordered.len() as f64;
    Ok(GaugeSummary {
        count: ordered.len(),
        minimum: ordered[0],
        maximum: ordered[ordered.len() - 1],
        p50: nearest_rank(&ordered, 50),
        p95: nearest_rank(&ordered, 95),
        mean,
        standard_deviation: variance.sqrt(),
    })
}

fn dependency_fingerprint(declared_features: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(declared_features.as_bytes());
    hasher.update(b"\n--- Cargo.lock ---\n");
    hasher.update(CARGO_LOCK.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

trait BenchmarkTarget {
    fn descriptor(&self) -> BenchmarkDescriptor;
    fn prepare_run(&mut self) -> AppResult<()> {
        Ok(())
    }
    fn run_once(&mut self) -> AppResult<BenchmarkObservation>;
    fn cleanup_run(&mut self) -> AppResult<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct BenchmarkDescriptor {
    comparison_id: &'static str,
    comparison_version: u16,
    implementation: &'static str,
    implementation_revision: &'static str,
    dependencies: &'static str,
    workload_id: &'static str,
    workload_version: u16,
    input_shape: &'static str,
    unit: &'static str,
    boundary: &'static str,
    process_mode: &'static str,
    fixture_bytes: &'static [u8],
}

#[derive(Debug)]
struct BenchmarkObservation {
    operation_units: u64,
    output_digest: String,
    timings_ns: BTreeMap<String, u64>,
    gauges: BTreeMap<String, u64>,
}

fn target_for(options: &Options) -> AppResult<Box<dyn BenchmarkTarget>> {
    match (
        options.comparison.as_str(),
        options.implementation.as_str(),
        options.workload.as_str(),
    ) {
        ("harness", "sha256-loop", "sha256-loop") => {
            let fixture: HarnessFixture = serde_json::from_str(HARNESS_FIXTURE_JSON)?;
            validate_fixture(&fixture)?;
            Ok(Box::new(Sha256LoopTarget { fixture }))
        }
        #[cfg(feature = "bench-storage")]
        ("storage", implementation, workload) => storage::target(implementation, workload),
        (comparison, implementation, workload) => Err(cli_error(format!(
            "benchmark target {comparison}/{implementation}/{workload} is not compiled into this runner"
        ))),
    }
}

#[derive(Clone, Debug, Deserialize)]
struct HarnessFixture {
    schema_version: u16,
    comparison_id: String,
    workload_id: String,
    workload_version: u16,
    payload: String,
    iterations: u32,
    expected_digest: String,
}

fn validate_fixture(fixture: &HarnessFixture) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    if fixture.comparison_id != "harness"
        || fixture.workload_id != "sha256-loop"
        || fixture.workload_version != 1
        || fixture.payload.is_empty()
        || fixture.payload.len() > 64 * 1024
        || fixture.iterations != 256
        || fixture.expected_digest.len() != 64
        || !fixture
            .expected_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(cli_error("benchmark harness fixture is invalid"));
    }
    Ok(())
}

struct Sha256LoopTarget {
    fixture: HarnessFixture,
}

impl BenchmarkTarget for Sha256LoopTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "harness",
            comparison_version: 1,
            implementation: "sha256-loop",
            implementation_revision: "1",
            dependencies: "sha2=0.11.0;features=default",
            workload_id: "sha256-loop",
            workload_version: self.fixture.workload_version,
            input_shape: "fixed UTF-8 payload repeated 256 times",
            unit: "bytes hashed",
            boundary: "hash input, encode digest, and verify expected digest",
            process_mode: "in-process",
            fixture_bytes: HARNESS_FIXTURE_JSON.as_bytes(),
        }
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        let mut hasher = Sha256::new();
        for _ in 0..self.fixture.iterations {
            hasher.update(black_box(self.fixture.payload.as_bytes()));
        }
        let output_digest: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if output_digest != self.fixture.expected_digest {
            return Err(cli_error("benchmark target produced an incorrect digest"));
        }
        let operation_units = u64::try_from(self.fixture.payload.len())?
            .checked_mul(u64::from(self.fixture.iterations))
            .ok_or_else(|| cli_error("benchmark operation size overflow"))?;
        Ok(BenchmarkObservation {
            operation_units,
            output_digest,
            timings_ns: BTreeMap::new(),
            gauges: BTreeMap::new(),
        })
    }
}

#[derive(Serialize)]
struct BenchmarkEvidence {
    schema_version: u16,
    generated_at_unix_ms: u64,
    candidate_id: String,
    source_revision: String,
    executable_sha256: String,
    configuration_sha256: String,
    comparison: ComparisonIdentity,
    implementation: ImplementationIdentity,
    workload: WorkloadIdentity,
    compiled_cpu_baseline: String,
    cpu_guard: CpuGuardReport,
    machine: MachineFingerprint,
    warmup_runs: u32,
    samples: Vec<BenchmarkSample>,
    summary: SampleSummary,
    timing_summaries: BTreeMap<String, SampleSummary>,
    gauge_summaries: BTreeMap<String, GaugeSummary>,
}

#[derive(Serialize)]
struct GaugeSummary {
    count: usize,
    minimum: u64,
    maximum: u64,
    p50: u64,
    p95: u64,
    mean: f64,
    standard_deviation: f64,
}

#[derive(Serialize)]
struct ComparisonIdentity {
    id: String,
    version: u16,
}

#[derive(Serialize)]
struct ImplementationIdentity {
    name: String,
    revision: String,
    dependency_fingerprint: String,
}

#[derive(Serialize)]
struct WorkloadIdentity {
    id: String,
    version: u16,
    fixture_sha256: String,
    input_shape: String,
    unit: String,
    boundary: String,
    process_mode: String,
}

#[derive(Serialize)]
struct BenchmarkSample {
    run: u32,
    operation_elapsed_ns: u64,
    operation_units: u64,
    output_digest: String,
    timings_ns: BTreeMap<String, u64>,
    gauges: BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn benchmark_list_is_a_separate_command() {
        assert_eq!(parse(&strings(&["list"])).expect("list"), Command::List);
    }

    #[test]
    fn benchmark_options_default_to_contract_sample_counts() {
        let command = parse(&strings(&[
            "--comparison",
            "harness",
            "--implementation",
            "sha256-loop",
            "--workload",
            "sha256-loop",
            "--candidate-id",
            "rc.bench",
            "--source-revision",
            "0123456789abcdef",
            "--output",
            "bench.json",
        ]))
        .expect("valid benchmark command");
        assert!(matches!(
            command,
            Command::Run(Options {
                runs: 30,
                warmup_runs: 3,
                ..
            })
        ));
    }

    #[test]
    fn harness_target_checks_the_versioned_fixture_digest() {
        let fixture: HarnessFixture =
            serde_json::from_str(HARNESS_FIXTURE_JSON).expect("valid fixture JSON");
        validate_fixture(&fixture).expect("supported fixture");
        let expected = fixture.expected_digest.clone();
        let mut target = Sha256LoopTarget { fixture };
        let observation = target.run_once().expect("correct digest");
        assert_eq!(observation.output_digest, expected);
        assert!(observation.operation_units > 0);
    }

    #[test]
    fn unavailable_technology_candidate_fails_explicitly() {
        let options = Options {
            comparison: "storage".into(),
            implementation: "unknown".into(),
            workload: "critical-append-replay".into(),
            candidate_id: "rc".into(),
            source_revision: "0123456".into(),
            output: "bench.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Full,
        };
        assert!(target_for(&options).is_err());
    }

    #[test]
    fn dependency_fingerprint_binds_lockfile_and_declared_features() {
        let default = dependency_fingerprint("sha2=0.11.0;features=default");
        let changed = dependency_fingerprint("sha2=0.11.0;features=asm");
        assert_eq!(default.len(), 64);
        assert_ne!(default, changed);
    }

    #[test]
    fn benchmark_measurements_keep_timing_and_gauge_units_separate() {
        let samples = vec![
            BenchmarkSample {
                run: 1,
                operation_elapsed_ns: 100,
                operation_units: 1,
                output_digest: "a".into(),
                timings_ns: BTreeMap::from([("append".into(), 70)]),
                gauges: BTreeMap::from([("storage_bytes".into(), 512)]),
            },
            BenchmarkSample {
                run: 2,
                operation_elapsed_ns: 120,
                operation_units: 1,
                output_digest: "a".into(),
                timings_ns: BTreeMap::from([("append".into(), 80)]),
                gauges: BTreeMap::from([("storage_bytes".into(), 768)]),
            },
        ];
        let timings = summarize_timing_samples(&samples).expect("timings");
        let gauges = summarize_gauge_samples(&samples).expect("gauges");
        assert_eq!(timings["append"].p50_ns, 70);
        assert_eq!(gauges["storage_bytes"].p50, 512);
    }

    #[test]
    fn benchmark_measurement_keys_cannot_change_between_samples() {
        let samples = vec![
            BenchmarkSample {
                run: 1,
                operation_elapsed_ns: 100,
                operation_units: 1,
                output_digest: "a".into(),
                timings_ns: BTreeMap::from([("append".into(), 70)]),
                gauges: BTreeMap::from([("storage_bytes".into(), 512)]),
            },
            BenchmarkSample {
                run: 2,
                operation_elapsed_ns: 120,
                operation_units: 1,
                output_digest: "a".into(),
                timings_ns: BTreeMap::from([("replay".into(), 80)]),
                gauges: BTreeMap::new(),
            },
        ];
        assert!(summarize_timing_samples(&samples).is_err());
        assert!(summarize_gauge_samples(&samples).is_err());
    }
}
