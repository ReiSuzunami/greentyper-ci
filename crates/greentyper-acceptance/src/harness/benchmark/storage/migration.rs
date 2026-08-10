use super::*;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::process::{Child, Command as ProcessCommand, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

const READY_FILE: &str = "migration-ready";
const READY_TEMP_FILE: &str = "migration-ready.pending";
const SUPERVISOR_FILE: &str = "migration-supervisor";
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_SELF_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_MARKER_BYTES: u64 = 256;
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const CASES: [MigrationCase; 3] = [
    MigrationCase::EarlyUnpublished,
    MigrationCase::CompleteUnpublished,
    MigrationCase::PublishedV2,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationCase {
    EarlyUnpublished,
    CompleteUnpublished,
    PublishedV2,
}

impl MigrationCase {
    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "early-unpublished" => Ok(Self::EarlyUnpublished),
            "complete-unpublished" => Ok(Self::CompleteUnpublished),
            "published-v2" => Ok(Self::PublishedV2),
            _ => Err(cli_error(format!("unknown storage migration case {value}"))),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::EarlyUnpublished => "early-unpublished",
            Self::CompleteUnpublished => "complete-unpublished",
            Self::PublishedV2 => "published-v2",
        }
    }

    const fn expected_schema_version(self) -> u64 {
        match self {
            Self::EarlyUnpublished | Self::CompleteUnpublished => 1,
            Self::PublishedV2 => 2,
        }
    }
}

pub(super) fn run_child(options: StorageMigrationChildOptions) -> AppResult<()> {
    let engine = match options.implementation.as_str() {
        "sqlite-wal" => StorageEngine::SqliteWal,
        "append-log" => StorageEngine::AppendLog,
        value => return Err(cli_error(format!("unknown storage engine {value}"))),
    };
    let case = MigrationCase::parse(&options.case_name)?;
    let supervisor_token = validate_child_directory(&options.run_dir, case)?;
    let fixture: StorageFixture = serde_json::from_str(MIGRATION_FIXTURE_JSON)?;
    validate_fixture(&fixture, StorageWorkload::InterruptedMigration)?;
    let events = generate_events(&fixture)?;
    match engine {
        StorageEngine::SqliteWal => {
            run_sqlite_child(&options.run_dir, case, &events, &supervisor_token)
        }
        StorageEngine::AppendLog => {
            run_append_log_child(&options.run_dir, case, &events, &supervisor_token)
        }
    }
}

pub(super) fn require_no_active_children(run_dir: &Path) -> AppResult<()> {
    for entry in fs::read_dir(run_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join(SUPERVISOR_FILE).try_exists()? {
            return Err(cli_error(format!(
                "storage migration artifacts preserved because child supervision is unresolved: {}",
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
    expected_boundaries: u32,
) -> AppResult<BenchmarkObservation> {
    if usize::try_from(expected_boundaries)? != CASES.len() {
        return Err(cli_error(
            "cross-process migration fixture boundary count is invalid",
        ));
    }
    let mut spawn_and_kill_ns = 0_u64;
    let mut recovery_ns = 0_u64;
    let mut old_recoveries = 0_u64;
    let mut new_recoveries = 0_u64;
    let mut final_storage_bytes = 0_u64;
    let mut final_schema_version = 0_u64;

    for case in CASES {
        let case_dir = run_dir.join(case.name());
        create_private_directory(&case_dir)?;
        let migration_started = Instant::now();
        spawn_and_kill(engine, case, &case_dir)?;
        spawn_and_kill_ns = spawn_and_kill_ns
            .checked_add(elapsed_ns(migration_started)?)
            .ok_or_else(|| cli_error("migration spawn timing overflow"))?;

        let recovery_started = Instant::now();
        let recovered = recover(engine, case, &case_dir, events)?;
        recovery_ns = recovery_ns
            .checked_add(elapsed_ns(recovery_started)?)
            .ok_or_else(|| cli_error("migration recovery timing overflow"))?;
        final_storage_bytes = final_storage_bytes
            .checked_add(directory_size(&case_dir)?)
            .ok_or_else(|| cli_error("migration storage size overflow"))?;
        match recovered {
            1 => old_recoveries += 1,
            2 => new_recoveries += 1,
            _ => return Err(cli_error("migration recovered an unsupported schema")),
        }
        if case == MigrationCase::PublishedV2 {
            final_schema_version = recovered;
        }
    }

    if old_recoveries != 2 || new_recoveries != 1 || final_schema_version != 2 {
        return Err(cli_error(
            "storage migration did not recover as exactly two v1 states and one v2 state",
        ));
    }
    Ok(BenchmarkObservation {
        operation_units: u64::from(expected_boundaries),
        output_digest: canonical_digest(events)?,
        timings_ns: BTreeMap::from([
            ("recovery".into(), recovery_ns),
            ("spawn_and_kill".into(), spawn_and_kill_ns),
        ]),
        gauges: BTreeMap::from([
            (
                "child_processes_killed".into(),
                u64::from(expected_boundaries),
            ),
            ("final_schema_version".into(), final_schema_version),
            ("final_storage_bytes".into(), final_storage_bytes),
            ("new_generation_recoveries".into(), new_recoveries),
            ("old_generation_recoveries".into(), old_recoveries),
        ]),
    })
}

fn run_sqlite_child(
    run_dir: &Path,
    case: MigrationCase,
    events: &[EventRecord],
    supervisor_token: &str,
) -> AppResult<()> {
    let path = run_dir.join("ledger.sqlite3");
    let mut connection = create_sqlite_store(&path)?;
    append_sqlite_events(&mut connection, events)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch("ALTER TABLE events ADD COLUMN payload_size INTEGER;")?;
    if case != MigrationCase::EarlyUnpublished {
        transaction.execute_batch(
            "UPDATE events SET payload_size = length(payload);
             PRAGMA user_version = 2;",
        )?;
    }
    if case == MigrationCase::PublishedV2 {
        transaction.commit()?;
        drop(connection);
        verify_sqlite_schema(&path, 2, true, events)?;
    } else {
        std::hint::black_box(&transaction);
    }
    signal_ready_and_wait(run_dir, case, supervisor_token)
}

fn run_append_log_child(
    run_dir: &Path,
    case: MigrationCase,
    events: &[EventRecord],
    supervisor_token: &str,
) -> AppResult<()> {
    let v1_path = run_dir.join("ledger-v1.gtlog");
    let mut v1 = create_append_log(&v1_path)?;
    append_log_events(&mut v1, events)?;
    drop(v1);
    let initial = select_append_log_generation(run_dir)?;
    if initial.format_version != 1 || initial.events != events {
        return Err(cli_error("append-log v1 generation is invalid"));
    }

    match case {
        MigrationCase::EarlyUnpublished => {
            let partial_path = run_dir.join("ledger-v2.partial");
            let first_transaction = transaction_slices(events)?
                .into_iter()
                .next()
                .ok_or_else(|| cli_error("migration workload has no transaction"))?;
            let mut partial = create_append_log_with_header(&partial_path, LOG_HEADER_V2)?;
            write_transaction(&mut partial, first_transaction)?;
            partial.flush()?;
            partial.sync_all()?;
            let length = partial.metadata()?.len();
            if length <= u64::try_from(LOG_HEADER_V2.len())? {
                return Err(cli_error("append-log migration partial frame is empty"));
            }
            partial.set_len(length - 1)?;
            partial.sync_all()?;
        }
        MigrationCase::CompleteUnpublished | MigrationCase::PublishedV2 => {
            let temporary_path = run_dir.join(".ledger-v2.gtlog.tmp");
            let published_path = run_dir.join("ledger-v2.gtlog");
            let mut temporary = create_append_log_with_header(&temporary_path, LOG_HEADER_V2)?;
            append_log_events(&mut temporary, events)?;
            drop(temporary);
            let candidate = replay_log(&temporary_path)?;
            if candidate.format_version != 2
                || candidate.incomplete_tail
                || candidate.events != events
            {
                return Err(cli_error("append-log complete v2 candidate is invalid"));
            }
            if case == MigrationCase::PublishedV2 {
                fs::rename(temporary_path, published_path)?;
                sync_directory(run_dir)?;
            }
        }
    }
    signal_ready_and_wait(run_dir, case, supervisor_token)
}

fn recover(
    engine: StorageEngine,
    case: MigrationCase,
    run_dir: &Path,
    events: &[EventRecord],
) -> AppResult<u64> {
    let expected_version = case.expected_schema_version();
    match engine {
        StorageEngine::SqliteWal => {
            verify_sqlite_schema(
                &run_dir.join("ledger.sqlite3"),
                expected_version,
                expected_version == 2,
                events,
            )?;
        }
        StorageEngine::AppendLog => {
            if case == MigrationCase::EarlyUnpublished {
                let partial = replay_log(&run_dir.join("ledger-v2.partial"))?;
                if partial.format_version != 2 || !partial.incomplete_tail {
                    return Err(cli_error(
                        "append-log partial migration generation was not detected",
                    ));
                }
            }
            if case == MigrationCase::CompleteUnpublished {
                let temporary = replay_log(&run_dir.join(".ledger-v2.gtlog.tmp"))?;
                if temporary.format_version != 2
                    || temporary.incomplete_tail
                    || temporary.events != events
                {
                    return Err(cli_error(
                        "append-log unpublished migration generation is invalid",
                    ));
                }
            }
            let selected = select_append_log_generation(run_dir)?;
            if selected.format_version != expected_version || selected.events != events {
                return Err(cli_error(
                    "append-log migration selected the wrong complete generation",
                ));
            }
        }
    }
    Ok(expected_version)
}

fn signal_ready_and_wait(
    run_dir: &Path,
    case: MigrationCase,
    supervisor_token: &str,
) -> AppResult<()> {
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
    Err(cli_error("storage migration child supervisor timed out"))
}

fn spawn_and_kill(engine: StorageEngine, case: MigrationCase, run_dir: &Path) -> AppResult<()> {
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
        .arg("__storage-migration-child")
        .arg("--implementation")
        .arg(engine.implementation())
        .arg("--case")
        .arg(case.name())
        .arg("--run-dir")
        .arg(run_dir)
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
    let mut child = MigrationChildGuard::new(child);
    let child_id = child.id();
    let marker_path = run_dir.join(READY_FILE);
    let started = Instant::now();
    loop {
        if marker_path.try_exists()? {
            validate_ready_marker(&marker_path, &supervisor_token, child_id, case)?;
            let status = child.terminate_and_wait()?;
            if status.success() {
                return Err(cli_error("migration child exited successfully after kill"));
            }
            fs::remove_file(&marker_path)?;
            fs::remove_file(&supervisor_path)?;
            sync_directory(run_dir)?;
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            fs::remove_file(&supervisor_path)?;
            return Err(cli_error(format!(
                "migration child exited before its marker ({status})"
            )));
        }
        if started.elapsed() >= CHILD_TIMEOUT {
            child.terminate_and_wait()?;
            fs::remove_file(&supervisor_path)?;
            return Err(cli_error("migration child timed out before its marker"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn validate_child_directory(run_dir: &Path, case: MigrationCase) -> AppResult<String> {
    let metadata = fs::symlink_metadata(run_dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(cli_error(
            "storage migration child run directory must be a real directory",
        ));
    }
    let canonical_run_dir = fs::canonicalize(run_dir)?;
    if canonical_run_dir != run_dir {
        return Err(cli_error(
            "storage migration child run directory must already be canonical",
        ));
    }
    if canonical_run_dir.file_name().and_then(|name| name.to_str()) != Some(case.name()) {
        return Err(cli_error(
            "storage migration child directory does not match its migration case",
        ));
    }
    let benchmark_dir = canonical_run_dir
        .parent()
        .ok_or_else(|| cli_error("storage migration child directory has no benchmark parent"))?;
    let benchmark_metadata = fs::symlink_metadata(benchmark_dir)?;
    if !benchmark_metadata.file_type().is_dir() || benchmark_metadata.file_type().is_symlink() {
        return Err(cli_error(
            "storage migration benchmark parent must be a real directory",
        ));
    }
    let benchmark_name = benchmark_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cli_error("storage migration benchmark directory name is invalid"))?;
    if !benchmark_name.starts_with("greentyper-storage-bench-") {
        return Err(cli_error(
            "storage migration child directory is outside the benchmark namespace",
        ));
    }
    let canonical_temp = fs::canonicalize(std::env::temp_dir())?;
    if benchmark_dir.parent() != Some(canonical_temp.as_path()) {
        return Err(cli_error(
            "storage migration child directory is outside the system temporary directory",
        ));
    }
    let entries: Vec<_> = fs::read_dir(&canonical_run_dir)?.collect::<Result<_, _>>()?;
    if entries.len() != 1 || entries[0].file_name() != SUPERVISOR_FILE {
        return Err(cli_error(
            "storage migration child directory was not freshly supervised",
        ));
    }
    let supervisor_path = canonical_run_dir.join(SUPERVISOR_FILE);
    let supervisor_metadata = fs::symlink_metadata(&supervisor_path)?;
    if !supervisor_metadata.file_type().is_file()
        || supervisor_metadata.file_type().is_symlink()
        || supervisor_metadata.len() != 64
    {
        return Err(cli_error("storage migration supervisor file is invalid"));
    }
    let supervisor_token = fs::read_to_string(supervisor_path)?;
    if supervisor_token.len() != 64
        || !supervisor_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cli_error("storage migration supervisor token is invalid"));
    }
    Ok(supervisor_token)
}

fn generate_supervisor_token(run_dir: &Path, case: MigrationCase) -> AppResult<String> {
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

fn ready_marker(supervisor_token: &str, child_id: u32, case: MigrationCase) -> String {
    format!(
        "greentyper-storage-migration-v1\n{supervisor_token}\n{child_id}\n{}\n",
        case.name()
    )
}

fn validate_ready_marker(
    marker_path: &Path,
    supervisor_token: &str,
    child_id: u32,
    case: MigrationCase,
) -> AppResult<()> {
    let metadata = fs::symlink_metadata(marker_path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MARKER_BYTES
    {
        return Err(cli_error("storage migration ready marker is invalid"));
    }
    if fs::read_to_string(marker_path)? != ready_marker(supervisor_token, child_id, case) {
        return Err(cli_error(
            "storage migration ready marker did not authenticate",
        ));
    }
    Ok(())
}

struct MigrationChildGuard {
    child: Option<Child>,
}

impl MigrationChildGuard {
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
            .expect("migration child guard lost its process");
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
            .expect("migration child guard lost its process");
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

impl Drop for MigrationChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
