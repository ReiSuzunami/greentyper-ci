//! Versioned Target Machine measurement and evidence harness.

use greentyper_core::agent_team::{
    AgentStatus, Capability, CapabilitySnapshot, CommandOutcome, CompletionCapsule,
    MessageRecipient, ResourceBudget, TaskScope, TaskSpec, TaskStatus, TeamCommand, TeamRuntime,
};
use greentyper_core::schema::SchemaKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/acceptance/v1/agent-team-smoke.json"
));
const DEFAULT_RUNS: u32 = 30;
const DEFAULT_WARMUP_RUNS: u32 = 3;
const MAX_RUNS: u32 = 10_000;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn run(arguments: impl IntoIterator<Item = String>) -> ExitCode {
    match run_cli(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("greentyper-acceptance: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(arguments: impl IntoIterator<Item = String>) -> AppResult<()> {
    match parse_command(arguments)? {
        AcceptanceCommand::VerifyCpu { expect_baseline } => {
            let report = cpu_guard_report();
            require_cpu_guard(&report, expect_baseline.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        AcceptanceCommand::Run(options) => run_acceptance(options)?,
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AcceptanceCommand {
    VerifyCpu { expect_baseline: Option<String> },
    Run(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunOptions {
    candidate_id: String,
    source_revision: String,
    output: PathBuf,
    runs: u32,
    warmup_runs: u32,
    expect_baseline: Option<String>,
    machine_identifiers: MachineIdentifierPolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MachineIdentifierPolicy {
    #[default]
    Full,
    Redacted,
}

fn parse_command(arguments: impl IntoIterator<Item = String>) -> AppResult<AcceptanceCommand> {
    let mut arguments = arguments.into_iter();
    let command = arguments.next().ok_or_else(|| cli_error(usage()))?;
    let remaining: Vec<String> = arguments.collect();

    match command.as_str() {
        "verify-cpu" => {
            let mut expect_baseline = None;
            parse_options(&remaining, |name, value| match name {
                "--expect-baseline" => set_once(&mut expect_baseline, name, value),
                _ => Err(cli_error(format!("unknown option {name}"))),
            })?;
            Ok(AcceptanceCommand::VerifyCpu { expect_baseline })
        }
        "run" => {
            let mut candidate_id = None;
            let mut source_revision = None;
            let mut output = None;
            let mut runs = DEFAULT_RUNS;
            let mut warmup_runs = DEFAULT_WARMUP_RUNS;
            let mut expect_baseline = None;
            let mut machine_identifiers = None;

            parse_options(&remaining, |name, value| match name {
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
                _ => Err(cli_error(format!("unknown option {name}"))),
            })?;

            let candidate_id = required(candidate_id, "--candidate-id")?;
            let source_revision = required(source_revision, "--source-revision")?;
            let output = PathBuf::from(required(output, "--output")?);
            validate_candidate_id(&candidate_id)?;
            validate_source_revision(&source_revision)?;
            let machine_identifiers = parse_machine_identifier_policy(machine_identifiers)?;

            Ok(AcceptanceCommand::Run(RunOptions {
                candidate_id,
                source_revision,
                output,
                runs,
                warmup_runs,
                expect_baseline,
                machine_identifiers,
            }))
        }
        "help" | "--help" | "-h" => Err(cli_error(usage())),
        _ => Err(cli_error(format!("unknown command {command}\n{}", usage()))),
    }
}

fn parse_options(
    arguments: &[String],
    mut apply: impl FnMut(&str, String) -> AppResult<()>,
) -> AppResult<()> {
    let mut cursor = 0;
    while cursor < arguments.len() {
        let name = &arguments[cursor];
        if !name.starts_with("--") {
            return Err(cli_error(format!("expected an option, found {name}")));
        }
        let value = arguments
            .get(cursor + 1)
            .ok_or_else(|| cli_error(format!("{name} requires a value")))?;
        if value.starts_with("--") {
            return Err(cli_error(format!("{name} requires a value")));
        }
        apply(name, value.clone())?;
        cursor += 2;
    }
    Ok(())
}

fn set_once(target: &mut Option<String>, name: &str, value: String) -> AppResult<()> {
    if target.replace(value).is_some() {
        return Err(cli_error(format!("{name} may only be specified once")));
    }
    Ok(())
}

fn required(value: Option<String>, name: &str) -> AppResult<String> {
    value.ok_or_else(|| cli_error(format!("missing required option {name}")))
}

fn parse_run_count(name: &str, value: String) -> AppResult<u32> {
    let value = value
        .parse::<u32>()
        .map_err(|_| cli_error(format!("{name} must be an integer")))?;
    if value == 0 || value > MAX_RUNS {
        return Err(cli_error(format!(
            "{name} must be between 1 and {MAX_RUNS}"
        )));
    }
    Ok(value)
}

fn validate_candidate_id(value: &str) -> AppResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(cli_error(
            "candidate ID must use 1-128 ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

fn validate_source_revision(value: &str) -> AppResult<()> {
    if !(7..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(cli_error(
            "source revision must be a 7-64 character hexadecimal object ID",
        ));
    }
    Ok(())
}

fn parse_machine_identifier_policy(value: Option<String>) -> AppResult<MachineIdentifierPolicy> {
    match value.as_deref() {
        None | Some("full") => Ok(MachineIdentifierPolicy::Full),
        Some("redacted") => Ok(MachineIdentifierPolicy::Redacted),
        Some(value) => Err(cli_error(format!(
            "--machine-identifiers must be 'full' or 'redacted', found {value}"
        ))),
    }
}

fn usage() -> String {
    "usage:\n  greentyper-acceptance verify-cpu [--expect-baseline NAME]\n  greentyper-acceptance run --candidate-id ID --source-revision HEX --output FILE [--runs N] [--warmup-runs N] [--expect-baseline NAME] [--machine-identifiers full|redacted]".into()
}

fn run_acceptance(options: RunOptions) -> AppResult<()> {
    let fixture: AgentTeamFixture = serde_json::from_str(FIXTURE_JSON)?;
    validate_fixture(&fixture)?;

    let cpu_guard = cpu_guard_report();
    require_cpu_guard(&cpu_guard, options.expect_baseline.as_deref())?;
    let executable = std::env::current_exe()?;
    let executable_sha256 = sha256_file(&executable)?;

    for _ in 0..options.warmup_runs {
        black_box(execute_fixture(&fixture)?);
    }

    let mut samples = Vec::with_capacity(options.runs as usize);
    for run in 1..=options.runs {
        let started = Instant::now();
        let observation = execute_fixture(&fixture)?;
        let elapsed_ns = u64::try_from(started.elapsed().as_nanos())
            .map_err(|_| cli_error("sample duration exceeds u64 nanoseconds"))?;
        black_box(observation);
        samples.push(RawSample { run, elapsed_ns });
    }

    let generated_at_unix_ms =
        u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
            .map_err(|_| cli_error("system time exceeds evidence representation"))?;

    let evidence = AcceptanceEvidence {
        schema_version: SchemaKind::AcceptanceEvidence.current().get(),
        generated_at_unix_ms,
        candidate_id: options.candidate_id,
        source_revision: options.source_revision,
        executable_sha256,
        configuration_sha256: sha256_bytes(FIXTURE_JSON.as_bytes()),
        workload_id: fixture.workload_id.clone(),
        workload_version: fixture.workload_version,
        compiled_cpu_baseline: cpu_guard.compiled_baseline.clone(),
        cpu_guard,
        machine: machine_fingerprint(options.machine_identifiers),
        warmup_runs: options.warmup_runs,
        samples: samples.clone(),
        summary: summarize(&samples)?,
    };

    write_new_json(&options.output, &evidence)?;
    println!("{}", options.output.display());
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
struct AgentTeamFixture {
    schema_version: u16,
    workload_id: String,
    workload_version: u16,
    max_active_agents: usize,
    root_title: String,
    scope_labels: Vec<String>,
    message: String,
    completion_outcome: String,
    expected_revision: u64,
    expected_tasks: usize,
    expected_agents: usize,
    expected_messages: usize,
}

fn validate_fixture(fixture: &AgentTeamFixture) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    if fixture.workload_id.trim().is_empty()
        || fixture.workload_version == 0
        || fixture.max_active_agents == 0
        || fixture.root_title.trim().is_empty()
        || fixture.scope_labels.is_empty()
        || fixture.message.trim().is_empty()
        || fixture.completion_outcome.trim().is_empty()
    {
        return Err(cli_error("acceptance fixture contains an invalid field"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FixtureObservation {
    revision: u64,
    tasks: usize,
    agents: usize,
    messages: usize,
}

fn execute_fixture(fixture: &AgentTeamFixture) -> AppResult<FixtureObservation> {
    let mut runtime = TeamRuntime::new(fixture.max_active_agents)?;
    let admission = runtime.dispatch(TeamCommand::AdmitRoot {
        task: TaskSpec::new(
            fixture.root_title.clone(),
            TaskScope::from_labels(fixture.scope_labels.iter().cloned()),
        ),
        budget: ResourceBudget::new(1_000, 1),
        capabilities: CapabilitySnapshot::from_capabilities([Capability::WorkspaceRead]),
    })?;
    let session = match admission.outcome {
        CommandOutcome::RootAdmitted { session, .. } => session,
        _ => {
            return Err(cli_error(
                "fixture admission returned an unexpected outcome",
            ));
        }
    };

    runtime.dispatch(TeamCommand::SendMessage {
        from: session,
        recipient: MessageRecipient::Team,
        body: fixture.message.clone(),
    })?;
    runtime.dispatch(TeamCommand::Complete {
        agent: session,
        capsule: CompletionCapsule::new(fixture.completion_outcome.clone()),
    })?;

    let snapshot = runtime.snapshot();
    if snapshot.tasks.len() != 1
        || snapshot.agents.len() != 1
        || snapshot.tasks[0].status != TaskStatus::Succeeded
        || snapshot.agents[0].status != AgentStatus::Succeeded
    {
        return Err(cli_error("fixture produced an invalid terminal projection"));
    }
    let observation = FixtureObservation {
        revision: snapshot.revision.get(),
        tasks: snapshot.tasks.len(),
        agents: snapshot.agents.len(),
        messages: snapshot.messages.len(),
    };
    let expected = FixtureObservation {
        revision: fixture.expected_revision,
        tasks: fixture.expected_tasks,
        agents: fixture.expected_agents,
        messages: fixture.expected_messages,
    };
    if observation != expected {
        return Err(cli_error(format!(
            "fixture projection changed: expected {expected:?}, found {observation:?}"
        )));
    }
    Ok(observation)
}

#[derive(Clone, Debug, Serialize)]
struct AcceptanceEvidence {
    schema_version: u16,
    generated_at_unix_ms: u64,
    candidate_id: String,
    source_revision: String,
    executable_sha256: String,
    configuration_sha256: String,
    workload_id: String,
    workload_version: u16,
    compiled_cpu_baseline: String,
    cpu_guard: CpuGuardReport,
    machine: MachineFingerprint,
    warmup_runs: u32,
    samples: Vec<RawSample>,
    summary: SampleSummary,
}

#[derive(Clone, Debug, Serialize)]
struct RawSample {
    run: u32,
    elapsed_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SampleSummary {
    count: usize,
    minimum_ns: u64,
    maximum_ns: u64,
    p50_ns: u64,
    p95_ns: u64,
    mean_ns: f64,
    standard_deviation_ns: f64,
}

fn summarize(samples: &[RawSample]) -> AppResult<SampleSummary> {
    if samples.is_empty() {
        return Err(cli_error("cannot summarize an empty sample set"));
    }
    let mut values: Vec<u64> = samples.iter().map(|sample| sample.elapsed_ns).collect();
    values.sort_unstable();
    let mean = values.iter().map(|value| *value as f64).sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;

    Ok(SampleSummary {
        count: values.len(),
        minimum_ns: values[0],
        maximum_ns: values[values.len() - 1],
        p50_ns: nearest_rank(&values, 50),
        p95_ns: nearest_rank(&values, 95),
        mean_ns: mean,
        standard_deviation_ns: variance.sqrt(),
    })
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CpuGuardReport {
    compiled_baseline: String,
    supported: bool,
    required_features: Vec<String>,
    missing_features: Vec<String>,
}

fn cpu_guard_report() -> CpuGuardReport {
    let compiled_baseline = compiled_cpu_baseline();
    let (required_features, missing_features) = required_and_missing_features(compiled_baseline);
    CpuGuardReport {
        compiled_baseline: compiled_baseline.into(),
        supported: missing_features.is_empty(),
        required_features,
        missing_features,
    }
}

fn compiled_cpu_baseline() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if cfg!(all(
            target_feature = "avx",
            target_feature = "avx2",
            target_feature = "bmi1",
            target_feature = "bmi2",
            target_feature = "f16c",
            target_feature = "fma",
            target_feature = "lzcnt",
            target_feature = "movbe",
            target_feature = "popcnt",
            target_feature = "sse3",
            target_feature = "sse4.1",
            target_feature = "sse4.2",
            target_feature = "ssse3"
        )) {
            return "x86-64-v3";
        }
        return "x86-64";
    }
    #[cfg(target_arch = "aarch64")]
    {
        "aarch64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        std::env::consts::ARCH
    }
}

#[cfg(target_arch = "x86_64")]
fn required_and_missing_features(baseline: &str) -> (Vec<String>, Vec<String>) {
    if baseline != "x86-64-v3" {
        return (Vec::new(), Vec::new());
    }
    let required = [
        "avx", "avx2", "bmi1", "bmi2", "f16c", "fma", "lzcnt", "movbe", "popcnt", "sse3", "sse4.1",
        "sse4.2", "ssse3",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let mut missing = Vec::new();
    if !std::arch::is_x86_feature_detected!("avx") {
        missing.push("avx".into());
    }
    if !std::arch::is_x86_feature_detected!("avx2") {
        missing.push("avx2".into());
    }
    if !std::arch::is_x86_feature_detected!("bmi1") {
        missing.push("bmi1".into());
    }
    if !std::arch::is_x86_feature_detected!("bmi2") {
        missing.push("bmi2".into());
    }
    if !std::arch::is_x86_feature_detected!("f16c") {
        missing.push("f16c".into());
    }
    if !std::arch::is_x86_feature_detected!("fma") {
        missing.push("fma".into());
    }
    if !std::arch::is_x86_feature_detected!("lzcnt") {
        missing.push("lzcnt".into());
    }
    if !std::arch::is_x86_feature_detected!("movbe") {
        missing.push("movbe".into());
    }
    if !std::arch::is_x86_feature_detected!("popcnt") {
        missing.push("popcnt".into());
    }
    if !std::arch::is_x86_feature_detected!("sse3") {
        missing.push("sse3".into());
    }
    if !std::arch::is_x86_feature_detected!("sse4.1") {
        missing.push("sse4.1".into());
    }
    if !std::arch::is_x86_feature_detected!("sse4.2") {
        missing.push("sse4.2".into());
    }
    if !std::arch::is_x86_feature_detected!("ssse3") {
        missing.push("ssse3".into());
    }
    (required, missing)
}

#[cfg(not(target_arch = "x86_64"))]
fn required_and_missing_features(_baseline: &str) -> (Vec<String>, Vec<String>) {
    (Vec::new(), Vec::new())
}

fn require_cpu_guard(report: &CpuGuardReport, expected: Option<&str>) -> AppResult<()> {
    if let Some(expected) = expected
        && report.compiled_baseline != expected
    {
        return Err(cli_error(format!(
            "compiled CPU baseline is {}, expected {expected}",
            report.compiled_baseline
        )));
    }
    if !report.supported {
        return Err(cli_error(format!(
            "host is missing compiled CPU features: {}",
            report.missing_features.join(", ")
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct MachineFingerprint {
    os: String,
    architecture: String,
    logical_processors: usize,
    computer_name: Option<String>,
    os_version: Option<String>,
    processor: Option<String>,
    total_physical_memory_bytes: Option<u64>,
}

fn machine_fingerprint(identifier_policy: MachineIdentifierPolicy) -> MachineFingerprint {
    let fingerprint = MachineFingerprint {
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        logical_processors: std::thread::available_parallelism().map_or(1, usize::from),
        computer_name: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok(),
        os_version: command_line(if cfg!(windows) { "cmd.exe" } else { "uname" }),
        processor: std::env::var("PROCESSOR_IDENTIFIER").ok(),
        total_physical_memory_bytes: None,
    };

    #[cfg(windows)]
    {
        let mut fingerprint = fingerprint;
        if let Some(windows) = windows_fingerprint() {
            fingerprint.computer_name = windows.computer_name;
            fingerprint.os_version = windows.os_version;
            fingerprint.processor = windows.processor;
            fingerprint.total_physical_memory_bytes = windows.total_physical_memory_bytes;
        }
        redact_machine_identifiers(fingerprint, identifier_policy)
    }
    #[cfg(not(windows))]
    {
        redact_machine_identifiers(fingerprint, identifier_policy)
    }
}

fn redact_machine_identifiers(
    mut fingerprint: MachineFingerprint,
    policy: MachineIdentifierPolicy,
) -> MachineFingerprint {
    if policy == MachineIdentifierPolicy::Redacted {
        fingerprint.computer_name = None;
        fingerprint.processor = None;
    }
    fingerprint
}

fn command_line(program: &str) -> Option<String> {
    let mut command = Command::new(program);
    if cfg!(windows) {
        command.args(["/d", "/c", "ver"]);
    } else {
        command.args(["-s", "-r"]);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(windows)]
#[derive(Deserialize)]
struct WindowsFingerprint {
    computer_name: Option<String>,
    os_version: Option<String>,
    processor: Option<String>,
    total_physical_memory_bytes: Option<u64>,
}

#[cfg(windows)]
fn windows_fingerprint() -> Option<WindowsFingerprint> {
    const SCRIPT: &str = "$os=Get-CimInstance Win32_OperatingSystem;$cs=Get-CimInstance Win32_ComputerSystem;$cpu=Get-CimInstance Win32_Processor|Select-Object -First 1;[pscustomobject]@{computer_name=$env:COMPUTERNAME;os_version=($os.Caption+' '+$os.Version+' build '+$os.BuildNumber);processor=$cpu.Name;total_physical_memory_bytes=[uint64]$cs.TotalPhysicalMemory}|ConvertTo-Json -Compress";
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn sha256_file(path: &Path) -> AppResult<String> {
    Ok(sha256_bytes(&fs::read(path)?))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_new_json(path: &Path, value: &impl Serialize) -> AppResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| cli_error("evidence output must name a file"))?
        .to_string_lossy();
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));

    let result = (|| -> AppResult<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer_pretty(&mut writer, value)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.sync_all()?;
        fs::hard_link(&temporary, path)?;
        let _ = fs::remove_file(&temporary);
        #[cfg(unix)]
        OpenOptions::new().read(true).open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug)]
struct CliError(String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CliError {}

fn cli_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(CliError(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn embedded_fixture_reaches_its_versioned_terminal_projection() {
        let fixture: AgentTeamFixture =
            serde_json::from_str(FIXTURE_JSON).expect("valid embedded fixture");
        validate_fixture(&fixture).expect("supported fixture");
        assert_eq!(
            execute_fixture(&fixture).expect("fixture runs"),
            FixtureObservation {
                revision: 10,
                tasks: 1,
                agents: 1,
                messages: 1,
            }
        );
    }

    #[test]
    fn unsupported_fixture_schema_fails_closed() {
        let incompatible =
            FIXTURE_JSON.replacen("\"schema_version\": 1", "\"schema_version\": 2", 1);
        let fixture: AgentTeamFixture =
            serde_json::from_str(&incompatible).expect("syntactically valid fixture");
        assert!(validate_fixture(&fixture).is_err());
    }

    #[test]
    fn summary_uses_nearest_rank_percentiles_and_keeps_raw_count() {
        let samples: Vec<RawSample> = [1_u64, 2, 3, 4, 100]
            .into_iter()
            .enumerate()
            .map(|(index, elapsed_ns)| RawSample {
                run: (index + 1) as u32,
                elapsed_ns,
            })
            .collect();
        let summary = summarize(&samples).expect("non-empty samples");
        assert_eq!(summary.count, 5);
        assert_eq!(summary.minimum_ns, 1);
        assert_eq!(summary.maximum_ns, 100);
        assert_eq!(summary.p50_ns, 3);
        assert_eq!(summary.p95_ns, 100);
    }

    #[test]
    fn run_cli_defaults_to_thirty_measured_runs() {
        let command = parse_command(strings(&[
            "run",
            "--candidate-id",
            "rc.test",
            "--source-revision",
            "0123456789abcdef",
            "--output",
            "evidence.json",
        ]))
        .expect("valid command");
        assert_eq!(
            command,
            AcceptanceCommand::Run(RunOptions {
                candidate_id: "rc.test".into(),
                source_revision: "0123456789abcdef".into(),
                output: PathBuf::from("evidence.json"),
                runs: 30,
                warmup_runs: 3,
                expect_baseline: None,
                machine_identifiers: MachineIdentifierPolicy::Full,
            })
        );
    }

    #[test]
    fn zero_runs_and_unbound_evidence_are_rejected() {
        assert!(
            parse_command(strings(&[
                "run",
                "--candidate-id",
                "rc",
                "--source-revision",
                "0123456",
                "--output",
                "evidence.json",
                "--runs",
                "0",
            ]))
            .is_err()
        );
        assert!(parse_command(strings(&["run", "--runs", "1"])).is_err());
    }

    #[test]
    fn cpu_guard_reports_the_compiled_baseline() {
        let report = cpu_guard_report();
        assert!(!report.compiled_baseline.is_empty());
        assert_eq!(report.supported, report.missing_features.is_empty());
    }

    #[test]
    fn redaction_removes_public_machine_identifiers() {
        let fingerprint = MachineFingerprint {
            os: "windows".into(),
            architecture: "x86_64".into(),
            logical_processors: 4,
            computer_name: Some("private-host".into()),
            os_version: Some("build".into()),
            processor: Some("private-cpu".into()),
            total_physical_memory_bytes: Some(8),
        };
        let redacted = redact_machine_identifiers(fingerprint, MachineIdentifierPolicy::Redacted);
        assert_eq!(redacted.computer_name, None);
        assert_eq!(redacted.processor, None);
        assert_eq!(redacted.os_version.as_deref(), Some("build"));
    }

    #[test]
    fn evidence_commit_is_complete_and_never_replaces_existing_output() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "greentyper-evidence-test-{}-{unique}.json",
            std::process::id()
        ));
        let value = serde_json::json!({"schema_version": 1, "complete": true});

        write_new_json(&path, &value).expect("first evidence commit succeeds");
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("read evidence"))
                .expect("complete JSON");
        assert_eq!(persisted, value);
        assert!(write_new_json(&path, &value).is_err());

        fs::remove_file(path).expect("remove isolated test evidence");
    }
}
