use super::*;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

const READY_FILE: &str = "crash-ready";
const READY_TEMP_FILE: &str = "crash-ready.pending";
const SUPERVISOR_FILE: &str = "crash-supervisor";
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_SELF_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_MARKER_BYTES: u64 = 256;
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const CASES: [CrashCase; 6] = [
    CrashCase::BeforeWrite,
    CrashCase::During(1),
    CrashCase::During(2),
    CrashCase::During(3),
    CrashCase::During(4),
    CrashCase::AfterSync,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashCase {
    BeforeWrite,
    During(u32),
    AfterSync,
}

impl CrashCase {
    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "before-write" => Ok(Self::BeforeWrite),
            "during-1" => Ok(Self::During(1)),
            "during-2" => Ok(Self::During(2)),
            "during-3" => Ok(Self::During(3)),
            "during-4" => Ok(Self::During(4)),
            "after-sync" => Ok(Self::AfterSync),
            _ => Err(cli_error(format!("unknown storage crash case {value}"))),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::BeforeWrite => "before-write",
            Self::During(1) => "during-1",
            Self::During(2) => "during-2",
            Self::During(3) => "during-3",
            Self::During(4) => "during-4",
            Self::During(_) => "during-invalid",
            Self::AfterSync => "after-sync",
        }
    }
}

pub(super) fn run_child(options: StorageCrashChildOptions) -> AppResult<()> {
    let engine = match options.implementation.as_str() {
        "sqlite-wal" => StorageEngine::SqliteWal,
        "append-log" => StorageEngine::AppendLog,
        value => return Err(cli_error(format!("unknown storage engine {value}"))),
    };
    let case = CrashCase::parse(&options.case_name)?;
    validate_child_directory(&options.run_dir, case, &options.supervisor_token)?;
    let fixture: StorageFixture = serde_json::from_str(CRASH_FIXTURE_JSON)?;
    validate_fixture(&fixture, StorageWorkload::CrossProcessCrashReplay)?;
    let events = generate_events(&fixture)?;
    let (base, candidate) = crash_event_groups(&events)?;
    match engine {
        StorageEngine::SqliteWal => run_sqlite_child(
            &options.run_dir,
            case,
            base,
            candidate,
            &options.supervisor_token,
        ),
        StorageEngine::AppendLog => run_append_log_child(
            &options.run_dir,
            case,
            base,
            candidate,
            &options.supervisor_token,
        ),
    }
}

pub(super) fn require_no_active_children(run_dir: &Path) -> AppResult<()> {
    for entry in fs::read_dir(run_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join(SUPERVISOR_FILE).try_exists()? {
            return Err(cli_error(format!(
                "storage crash artifacts preserved because child supervision is unresolved: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

pub(super) fn run_workload(
    engine: StorageEngine,
    run_dir: &Path,
    events: &[EventRecord],
    expected_cases: u32,
) -> AppResult<BenchmarkObservation> {
    if usize::try_from(expected_cases)? != CASES.len() {
        return Err(cli_error(
            "cross-process crash fixture case count is invalid",
        ));
    }
    let (base, candidate) = crash_event_groups(events)?;
    let mut result_hasher = Sha256::new();
    let mut spawn_and_kill_ns = 0_u64;
    let mut recovery_and_reconcile_ns = 0_u64;
    let mut known_not_repeated = 0_u64;
    let mut ambiguous_blocked = 0_u64;

    for case in CASES {
        let case_dir = run_dir.join(case.name());
        create_private_directory(&case_dir)?;
        let crash_started = Instant::now();
        spawn_and_kill(engine, case, &case_dir)?;
        spawn_and_kill_ns = spawn_and_kill_ns
            .checked_add(elapsed_ns(crash_started)?)
            .ok_or_else(|| cli_error("crash workload timing overflow"))?;

        let recovery_started = Instant::now();
        let outcome = recover_and_reconcile(engine, case, &case_dir, base, candidate)?;
        recovery_and_reconcile_ns = recovery_and_reconcile_ns
            .checked_add(elapsed_ns(recovery_started)?)
            .ok_or_else(|| cli_error("crash recovery timing overflow"))?;
        match outcome.classification {
            RecoveryClassification::KnownNotRepeated => known_not_repeated += 1,
            RecoveryClassification::AmbiguousBlocked => ambiguous_blocked += 1,
        }
        result_hasher.update(case.name().as_bytes());
        result_hasher.update(outcome.classification.label().as_bytes());
        result_hasher.update(outcome.event_digest.as_bytes());
    }

    if known_not_repeated != 2 || ambiguous_blocked != 4 {
        return Err(cli_error(
            "cross-process crash outcomes do not match the frozen recovery policy",
        ));
    }
    Ok(BenchmarkObservation {
        operation_units: u64::from(expected_cases),
        output_digest: result_hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        timings_ns: BTreeMap::from([
            ("recovery_and_reconcile".into(), recovery_and_reconcile_ns),
            ("spawn_and_kill".into(), spawn_and_kill_ns),
        ]),
        gauges: BTreeMap::from([
            ("ambiguous_blocked".into(), ambiguous_blocked),
            ("child_processes_killed".into(), u64::from(expected_cases)),
            ("known_not_repeated".into(), known_not_repeated),
        ]),
    })
}

struct RecoveryOutcome {
    classification: RecoveryClassification,
    event_digest: String,
}

#[derive(Clone, Copy)]
enum RecoveryClassification {
    KnownNotRepeated,
    AmbiguousBlocked,
}

impl RecoveryClassification {
    const fn label(self) -> &'static str {
        match self {
            Self::KnownNotRepeated => "known-not-repeated",
            Self::AmbiguousBlocked => "ambiguous-blocked",
        }
    }
}

fn crash_event_groups(events: &[EventRecord]) -> AppResult<(&[EventRecord], &[EventRecord])> {
    let transactions = transaction_slices(events)?;
    let candidate = transactions
        .last()
        .copied()
        .ok_or_else(|| cli_error("crash workload has no candidate transaction"))?;
    let base_length = events
        .len()
        .checked_sub(candidate.len())
        .ok_or_else(|| cli_error("crash workload event split underflow"))?;
    let base = &events[..base_length];
    if base.is_empty() || candidate.len() != 4 || candidate[0].index != 0 {
        return Err(cli_error("crash workload transaction shape is invalid"));
    }
    Ok((base, candidate))
}

fn run_sqlite_child(
    run_dir: &Path,
    case: CrashCase,
    base: &[EventRecord],
    candidate: &[EventRecord],
    supervisor_token: &str,
) -> AppResult<()> {
    let path = run_dir.join("ledger.sqlite3");
    let mut connection = create_sqlite_store(&path)?;
    append_sqlite_events(&mut connection, base)?;
    match case {
        CrashCase::BeforeWrite => signal_ready_and_wait(run_dir, case, supervisor_token),
        CrashCase::During(progress) => {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            {
                let mut statement = transaction.prepare(
                    "INSERT INTO events (sequence, transaction_id, event_index, payload)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for event in candidate.iter().take(usize::try_from(progress)?) {
                    statement.execute(params![
                        i64::try_from(event.sequence)?,
                        i64::try_from(event.transaction)?,
                        i64::from(event.index),
                        &event.payload,
                    ])?;
                }
            }
            if usize::try_from(progress)? == candidate.len() {
                let first = candidate
                    .first()
                    .ok_or_else(|| cli_error("crash candidate is empty"))?;
                let last = candidate
                    .last()
                    .ok_or_else(|| cli_error("crash candidate is empty"))?;
                let changed = transaction.execute(
                    "UPDATE ledger_state SET head_sequence = ?1
                     WHERE singleton = 1 AND head_sequence = ?2",
                    params![
                        i64::try_from(last.sequence)?,
                        i64::try_from(first.sequence - 1)?
                    ],
                )?;
                if changed != 1 {
                    return Err(cli_error("crash child SQLite head update failed"));
                }
            }
            std::hint::black_box(&transaction);
            signal_ready_and_wait(run_dir, case, supervisor_token)
        }
        CrashCase::AfterSync => {
            append_sqlite_events(&mut connection, candidate)?;
            signal_ready_and_wait(run_dir, case, supervisor_token)
        }
    }
}

fn run_append_log_child(
    run_dir: &Path,
    case: CrashCase,
    base: &[EventRecord],
    candidate: &[EventRecord],
    supervisor_token: &str,
) -> AppResult<()> {
    let path = run_dir.join("ledger.gtlog");
    let mut file = create_append_log(&path)?;
    append_log_events(&mut file, base)?;
    match case {
        CrashCase::BeforeWrite => signal_ready_and_wait(run_dir, case, supervisor_token),
        CrashCase::During(progress) => {
            let frame = encode_transaction(candidate)?;
            let cut = append_log_cut_offset(&frame, progress)?;
            file.write_all(&frame[..cut])?;
            file.flush()?;
            signal_ready_and_wait(run_dir, case, supervisor_token)
        }
        CrashCase::AfterSync => {
            write_transaction(&mut file, candidate)?;
            file.flush()?;
            file.sync_data()?;
            signal_ready_and_wait(run_dir, case, supervisor_token)
        }
    }
}

fn append_log_cut_offset(frame: &[u8], progress: u32) -> AppResult<usize> {
    let framing = TRANSACTION_MAGIC.len() + size_of::<u32>();
    let cut = match progress {
        1 => 2,
        2 => TRANSACTION_MAGIC.len() + 2,
        3 => framing + (frame.len() - framing) / 2,
        4 => frame
            .len()
            .checked_sub(2)
            .ok_or_else(|| cli_error("append-log crash frame is too short"))?,
        _ => return Err(cli_error("append-log crash progress is invalid")),
    };
    if cut == 0 || cut >= frame.len() {
        return Err(cli_error("append-log crash cut is outside the frame"));
    }
    Ok(cut)
}

fn signal_ready_and_wait(run_dir: &Path, case: CrashCase, supervisor_token: &str) -> AppResult<()> {
    let marker_path = run_dir.join(READY_FILE);
    let temporary_marker_path = run_dir.join(READY_TEMP_FILE);
    let mut marker = create_private_file(&temporary_marker_path)?;
    marker.write_all(ready_marker(supervisor_token, std::process::id(), case).as_bytes())?;
    marker.flush()?;
    marker.sync_all()?;
    drop(marker);
    fs::rename(temporary_marker_path, marker_path)?;
    sync_directory(run_dir)?;
    let started = Instant::now();
    while started.elapsed() < CHILD_SELF_TIMEOUT {
        thread::sleep(POLL_INTERVAL);
    }
    Err(cli_error("storage crash child supervisor timed out"))
}

fn validate_child_directory(
    run_dir: &Path,
    case: CrashCase,
    supervisor_token: &str,
) -> AppResult<()> {
    let metadata = fs::symlink_metadata(run_dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(cli_error(
            "storage crash child run directory must be a real directory",
        ));
    }
    let canonical_run_dir = fs::canonicalize(run_dir)?;
    if canonical_run_dir != run_dir {
        return Err(cli_error(
            "storage crash child run directory must already be canonical",
        ));
    }
    if canonical_run_dir.file_name().and_then(|name| name.to_str()) != Some(case.name()) {
        return Err(cli_error(
            "storage crash child directory does not match its crash case",
        ));
    }
    let benchmark_dir = canonical_run_dir
        .parent()
        .ok_or_else(|| cli_error("storage crash child directory has no benchmark parent"))?;
    let benchmark_metadata = fs::symlink_metadata(benchmark_dir)?;
    if !benchmark_metadata.file_type().is_dir() || benchmark_metadata.file_type().is_symlink() {
        return Err(cli_error(
            "storage crash benchmark parent must be a real directory",
        ));
    }
    let benchmark_name = benchmark_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cli_error("storage crash benchmark directory name is invalid"))?;
    if !benchmark_name.starts_with("greentyper-storage-bench-") {
        return Err(cli_error(
            "storage crash child directory is outside the benchmark namespace",
        ));
    }
    let canonical_temp = fs::canonicalize(std::env::temp_dir())?;
    if benchmark_dir.parent() != Some(canonical_temp.as_path()) {
        return Err(cli_error(
            "storage crash child directory is outside the system temporary directory",
        ));
    }

    let entries: Vec<_> = fs::read_dir(&canonical_run_dir)?.collect::<Result<_, _>>()?;
    if entries.len() != 1 || entries[0].file_name() != SUPERVISOR_FILE {
        return Err(cli_error(
            "storage crash child directory was not freshly supervised",
        ));
    }
    let supervisor_path = canonical_run_dir.join(SUPERVISOR_FILE);
    let supervisor_metadata = fs::symlink_metadata(&supervisor_path)?;
    if !supervisor_metadata.file_type().is_file()
        || supervisor_metadata.file_type().is_symlink()
        || supervisor_metadata.len() != 64
    {
        return Err(cli_error("storage crash supervisor file is invalid"));
    }
    if fs::read_to_string(supervisor_path)? != supervisor_token {
        return Err(cli_error("storage crash supervisor token does not match"));
    }
    Ok(())
}

fn generate_supervisor_token(run_dir: &Path, case: CrashCase) -> AppResult<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut entropy = RandomState::new().build_hasher();
    entropy.write_u128(timestamp);
    entropy.write_u32(std::process::id());
    entropy.write_u64(sequence);
    entropy.write(case.name().as_bytes());
    entropy.write(run_dir.to_string_lossy().as_bytes());

    let mut digest = Sha256::new();
    digest.update(timestamp.to_le_bytes());
    digest.update(std::process::id().to_le_bytes());
    digest.update(sequence.to_le_bytes());
    digest.update(entropy.finish().to_le_bytes());
    digest.update(case.name().as_bytes());
    digest.update(run_dir.to_string_lossy().as_bytes());
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn create_private_file(path: &Path) -> AppResult<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn ready_marker(supervisor_token: &str, child_id: u32, case: CrashCase) -> String {
    format!(
        "greentyper-storage-crash-v1\n{supervisor_token}\n{child_id}\n{}\n",
        case.name()
    )
}

fn validate_ready_marker(
    marker_path: &Path,
    supervisor_token: &str,
    child_id: u32,
    case: CrashCase,
) -> AppResult<()> {
    let metadata = fs::symlink_metadata(marker_path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MARKER_BYTES
    {
        return Err(cli_error("storage crash ready marker is invalid"));
    }
    if fs::read_to_string(marker_path)? != ready_marker(supervisor_token, child_id, case) {
        return Err(cli_error("storage crash ready marker did not authenticate"));
    }
    Ok(())
}

fn spawn_and_kill(engine: StorageEngine, case: CrashCase, run_dir: &Path) -> AppResult<()> {
    let supervisor_token = generate_supervisor_token(run_dir, case)?;
    let supervisor_path = run_dir.join(SUPERVISOR_FILE);
    let mut supervisor = create_private_file(&supervisor_path)?;
    supervisor.write_all(supervisor_token.as_bytes())?;
    supervisor.flush()?;
    supervisor.sync_all()?;
    drop(supervisor);

    let executable = std::env::current_exe()?;
    let child = ProcessCommand::new(executable)
        .arg("bench")
        .arg("__storage-crash-child")
        .arg("--implementation")
        .arg(engine.implementation())
        .arg("--case")
        .arg(case.name())
        .arg("--run-dir")
        .arg(run_dir)
        .arg("--supervisor-token")
        .arg(&supervisor_token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            fs::remove_file(&supervisor_path)?;
            return Err(error.into());
        }
    };
    let mut child = CrashChildGuard::new(child);
    let child_id = child.id();
    let marker_path = run_dir.join(READY_FILE);
    let started = Instant::now();
    loop {
        if marker_path.try_exists()? {
            validate_ready_marker(&marker_path, &supervisor_token, child_id, case)?;
            let status = child.terminate_and_wait()?;
            if status.success() {
                return Err(cli_error("crash child exited successfully after kill"));
            }
            fs::remove_file(supervisor_path)?;
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            fs::remove_file(supervisor_path)?;
            return Err(cli_error(format!(
                "crash child exited before its marker ({status})"
            )));
        }
        if started.elapsed() >= CHILD_TIMEOUT {
            child.terminate_and_wait()?;
            fs::remove_file(supervisor_path)?;
            return Err(cli_error("crash child timed out before its marker"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

struct CrashChildGuard {
    child: Option<Child>,
}

impl CrashChildGuard {
    const fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let child = self
            .child
            .as_mut()
            .expect("crash child guard lost its process");
        let status = child.try_wait()?;
        if status.is_some() {
            self.child.take();
        }
        Ok(status)
    }

    fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        let child = self
            .child
            .as_mut()
            .expect("crash child guard lost its process");
        if let Err(kill_error) = child.kill() {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok(status);
            }
            return Err(kill_error);
        }
        let status = child.wait()?;
        self.child.take();
        Ok(status)
    }
}

impl Drop for CrashChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn recover_and_reconcile(
    engine: StorageEngine,
    case: CrashCase,
    run_dir: &Path,
    base: &[EventRecord],
    candidate: &[EventRecord],
) -> AppResult<RecoveryOutcome> {
    match engine {
        StorageEngine::SqliteWal => recover_sqlite(case, run_dir, base, candidate),
        StorageEngine::AppendLog => recover_append_log(case, run_dir, base, candidate),
    }
}

fn recover_sqlite(
    case: CrashCase,
    run_dir: &Path,
    base: &[EventRecord],
    candidate: &[EventRecord],
) -> AppResult<RecoveryOutcome> {
    let path = run_dir.join("ledger.sqlite3");
    let recovered = replay_sqlite(&path)?;
    match case {
        CrashCase::BeforeWrite => {
            require_events(&recovered, base, "SQLite before-write recovery")?;
            if !append_sqlite_if_head(&path, head(base), candidate)? {
                return Err(cli_error(
                    "SQLite before-write retry lost its expected head",
                ));
            }
            let final_events = replay_sqlite(&path)?;
            let expected = combined_events(base, candidate);
            require_events(&final_events, &expected, "SQLite before-write retry")?;
            known_outcome(&final_events)
        }
        CrashCase::During(_) => {
            require_events(&recovered, base, "SQLite uncommitted recovery")?;
            ambiguous_outcome(&recovered)
        }
        CrashCase::AfterSync => {
            let expected = combined_events(base, candidate);
            require_events(&recovered, &expected, "SQLite after-sync recovery")?;
            if append_sqlite_if_head(&path, head(base), candidate)? {
                return Err(cli_error("SQLite repeated an already durable transaction"));
            }
            require_events(&replay_sqlite(&path)?, &expected, "SQLite after-sync retry")?;
            known_outcome(&expected)
        }
    }
}

fn recover_append_log(
    case: CrashCase,
    run_dir: &Path,
    base: &[EventRecord],
    candidate: &[EventRecord],
) -> AppResult<RecoveryOutcome> {
    let path = run_dir.join("ledger.gtlog");
    let recovered = replay_log(&path)?;
    match case {
        CrashCase::BeforeWrite => {
            if recovered.incomplete_tail {
                return Err(cli_error("append-log before-write has an incomplete tail"));
            }
            require_events(&recovered.events, base, "append-log before-write recovery")?;
            if !append_log_if_head(&path, head(base), candidate)? {
                return Err(cli_error(
                    "append-log before-write retry lost its expected head",
                ));
            }
            let final_events = replay_log(&path)?;
            if final_events.incomplete_tail {
                return Err(cli_error("append-log retry left an incomplete tail"));
            }
            let expected = combined_events(base, candidate);
            require_events(
                &final_events.events,
                &expected,
                "append-log before-write retry",
            )?;
            known_outcome(&final_events.events)
        }
        CrashCase::During(_) => {
            if !recovered.incomplete_tail {
                return Err(cli_error("append-log partial write was not detected"));
            }
            require_events(&recovered.events, base, "append-log partial recovery")?;
            ambiguous_outcome(&recovered.events)
        }
        CrashCase::AfterSync => {
            if recovered.incomplete_tail {
                return Err(cli_error("append-log after-sync has an incomplete tail"));
            }
            let expected = combined_events(base, candidate);
            require_events(
                &recovered.events,
                &expected,
                "append-log after-sync recovery",
            )?;
            if append_log_if_head(&path, head(base), candidate)? {
                return Err(cli_error(
                    "append-log repeated an already durable transaction",
                ));
            }
            require_events(
                &replay_log(&path)?.events,
                &expected,
                "append-log after-sync retry",
            )?;
            known_outcome(&expected)
        }
    }
}

fn append_sqlite_if_head(
    path: &Path,
    expected_head: u64,
    candidate: &[EventRecord],
) -> AppResult<bool> {
    let mut connection = Connection::open(path)?;
    configure_sqlite_durability(&connection)?;
    let stored_head: i64 = connection.query_row(
        "SELECT head_sequence FROM ledger_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if u64::try_from(stored_head)? != expected_head {
        return Ok(false);
    }
    append_sqlite_events(&mut connection, candidate)?;
    Ok(true)
}

fn append_log_if_head(
    path: &Path,
    expected_head: u64,
    candidate: &[EventRecord],
) -> AppResult<bool> {
    let replayed = replay_log(path)?;
    if replayed.incomplete_tail {
        return Err(cli_error("cannot append after an incomplete log tail"));
    }
    if head(&replayed.events) != expected_head {
        return Ok(false);
    }
    let mut file = OpenOptions::new().append(true).open(path)?;
    append_log_events(&mut file, candidate)?;
    Ok(true)
}

fn require_events(actual: &[EventRecord], expected: &[EventRecord], label: &str) -> AppResult<()> {
    if actual != expected {
        return Err(cli_error(format!(
            "{label} differs from the canonical prefix"
        )));
    }
    Ok(())
}

fn combined_events(base: &[EventRecord], candidate: &[EventRecord]) -> Vec<EventRecord> {
    base.iter().chain(candidate).cloned().collect()
}

fn head(events: &[EventRecord]) -> u64 {
    events.last().map_or(0, |event| event.sequence)
}

fn known_outcome(events: &[EventRecord]) -> AppResult<RecoveryOutcome> {
    Ok(RecoveryOutcome {
        classification: RecoveryClassification::KnownNotRepeated,
        event_digest: canonical_digest(events)?,
    })
}

fn ambiguous_outcome(events: &[EventRecord]) -> AppResult<RecoveryOutcome> {
    Ok(RecoveryOutcome {
        classification: RecoveryClassification::AmbiguousBlocked,
        event_digest: canonical_digest(events)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_log_crash_offsets_cover_framing_body_and_checksum() {
        let fixture: StorageFixture = serde_json::from_str(CRASH_FIXTURE_JSON).expect("fixture");
        let events = generate_events(&fixture).expect("events");
        let (_, candidate) = crash_event_groups(&events).expect("groups");
        let frame = encode_transaction(candidate).expect("frame");
        let offsets: Vec<usize> = (1..=4)
            .map(|progress| append_log_cut_offset(&frame, progress).expect("offset"))
            .collect();
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(offsets[0] < TRANSACTION_MAGIC.len());
        assert!(offsets[3] < frame.len());
    }

    #[test]
    fn crash_case_names_are_stable_and_parseable() {
        for case in CASES {
            assert_eq!(CrashCase::parse(case.name()).expect("case"), case);
        }
        assert!(CrashCase::parse("unknown").is_err());
    }

    #[test]
    fn child_directory_requires_a_fresh_matching_supervisor() {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let benchmark_dir = std::env::temp_dir().join(format!(
            "greentyper-storage-bench-validation-{}-{timestamp}",
            std::process::id()
        ));
        create_private_directory(&benchmark_dir).expect("benchmark directory");
        let benchmark_dir = fs::canonicalize(benchmark_dir).expect("canonical benchmark path");
        let case_dir = benchmark_dir.join(CrashCase::BeforeWrite.name());
        create_private_directory(&case_dir).expect("case directory");
        let case_dir = fs::canonicalize(case_dir).expect("canonical case path");
        let token = "a".repeat(64);
        let mut supervisor =
            create_private_file(&case_dir.join(SUPERVISOR_FILE)).expect("supervisor");
        supervisor.write_all(token.as_bytes()).expect("token");
        supervisor.sync_all().expect("sync token");
        drop(supervisor);

        validate_child_directory(&case_dir, CrashCase::BeforeWrite, &token)
            .expect("fresh supervised directory");
        assert!(
            validate_child_directory(&case_dir, CrashCase::BeforeWrite, &"b".repeat(64)).is_err()
        );
        File::create(case_dir.join("unexpected")).expect("unexpected entry");
        assert!(validate_child_directory(&case_dir, CrashCase::BeforeWrite, &token).is_err());
        fs::remove_dir_all(benchmark_dir).expect("cleanup");
    }

    #[test]
    fn ready_marker_binds_token_process_and_case() {
        let marker = ready_marker(&"a".repeat(64), 42, CrashCase::During(3));
        assert!(marker.contains("\n42\nduring-3\n"));
        assert_ne!(
            marker,
            ready_marker(&"b".repeat(64), 42, CrashCase::During(3))
        );
        assert_ne!(
            marker,
            ready_marker(&"a".repeat(64), 43, CrashCase::During(3))
        );
    }
}
