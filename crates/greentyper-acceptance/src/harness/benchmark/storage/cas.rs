use super::*;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::thread;
use std::time::Duration;

const SUPERVISOR_FILE: &str = "cas-supervisor";
const START_FILE: &str = "cas-start";
const LOCK_FILE: &str = "cas.lock";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(35);
const CHILD_SELF_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_MARKER_BYTES: u64 = 256;
const MAX_CONTENDERS: u32 = 32;
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CasOutcome {
    Winner,
    Loser,
}

impl CasOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Winner => "winner",
            Self::Loser => "loser",
        }
    }
}

pub(super) fn run_child(options: StorageCasChildOptions) -> AppResult<()> {
    let engine = match options.implementation.as_str() {
        "sqlite-wal" => StorageEngine::SqliteWal,
        "append-log" => StorageEngine::AppendLog,
        value => return Err(cli_error(format!("unknown storage engine {value}"))),
    };
    let fixture: StorageFixture = serde_json::from_str(CAS_FIXTURE_JSON)?;
    validate_fixture(&fixture, StorageWorkload::CasOneWinner)?;
    let contenders = fixture
        .cas_contenders
        .ok_or_else(|| cli_error("CAS fixture has no contender count"))?;
    if options.contender >= contenders {
        return Err(cli_error("storage CAS contender is outside the fixture"));
    }
    validate_child_directory(&options.run_dir, &options.supervisor_token)?;

    let events = generate_events(&fixture)?;
    let candidate = cas_event(&events)?;
    let expected_head = candidate
        .sequence
        .checked_sub(1)
        .ok_or_else(|| cli_error("CAS candidate sequence starts at zero"))?;
    signal_ready(
        &options.run_dir,
        &options.supervisor_token,
        options.contender,
    )?;
    wait_for_start(&options.run_dir, &options.supervisor_token)?;

    let outcome = match engine {
        StorageEngine::SqliteWal => run_sqlite_cas(&options.run_dir, expected_head, &candidate)?,
        StorageEngine::AppendLog => {
            run_append_log_cas(&options.run_dir, expected_head, &candidate)?
        }
    };
    write_atomic_marker(
        &options.run_dir,
        &outcome_file(options.contender),
        &outcome_marker(
            &options.supervisor_token,
            std::process::id(),
            options.contender,
            outcome,
        ),
    )
}

pub(super) fn require_no_active_children(run_dir: &Path) -> AppResult<()> {
    if run_dir.join(SUPERVISOR_FILE).try_exists()? {
        return Err(cli_error(format!(
            "storage CAS artifacts preserved because child supervision is unresolved: {}",
            run_dir.display()
        )));
    }
    Ok(())
}

pub(super) fn run_workload(
    engine: StorageEngine,
    run_dir: &Path,
    events: &[EventRecord],
    contenders: u32,
) -> AppResult<BenchmarkObservation> {
    if !(2..=MAX_CONTENDERS).contains(&contenders) {
        return Err(cli_error(format!(
            "CAS workload requires 2-{MAX_CONTENDERS} contenders"
        )));
    }
    let candidate = cas_event(events)?;

    let prepare_started = Instant::now();
    prepare_store(engine, run_dir, events)?;
    let supervisor_token = generate_supervisor_token(run_dir)?;
    create_coordination_files(run_dir, &supervisor_token)?;
    let prepare_ns = elapsed_ns(prepare_started)?;

    let cas_started = Instant::now();
    let mut children = spawn_children(engine, run_dir, &supervisor_token, contenders)?;
    children.wait_until_ready(run_dir, &supervisor_token)?;
    write_atomic_marker(run_dir, START_FILE, &start_marker(&supervisor_token))?;
    children.wait_until_complete()?;

    let mut winners = 0_u32;
    let mut losers = 0_u32;
    for child in &children.children {
        match read_outcome(run_dir, &supervisor_token, child)? {
            CasOutcome::Winner => {
                winners = winners
                    .checked_add(1)
                    .ok_or_else(|| cli_error("CAS winner count overflow"))?;
            }
            CasOutcome::Loser => {
                losers = losers
                    .checked_add(1)
                    .ok_or_else(|| cli_error("CAS loser count overflow"))?;
            }
        }
    }
    let cas_ns = elapsed_ns(cas_started)?;

    let replay_started = Instant::now();
    let replayed = match engine {
        StorageEngine::SqliteWal => replay_sqlite(&run_dir.join("ledger.sqlite3"))?,
        StorageEngine::AppendLog => {
            let replay = replay_log(&run_dir.join("ledger.gtlog"))?;
            if replay.incomplete_tail {
                return Err(cli_error("CAS append log has an incomplete tail"));
            }
            replay.events
        }
    };
    let replay_ns = elapsed_ns(replay_started)?;

    let mut expected = events.to_vec();
    expected.push(candidate);
    if winners != 1 || losers != contenders - 1 || replayed != expected {
        return Err(cli_error(
            "storage CAS did not produce exactly one canonical winner",
        ));
    }
    remove_coordination_files(run_dir, contenders)?;
    let storage_bytes = directory_size(run_dir)?;

    Ok(BenchmarkObservation {
        operation_units: u64::from(contenders),
        output_digest: canonical_digest(&replayed)?,
        timings_ns: BTreeMap::from([
            ("cas".into(), cas_ns),
            ("prepare".into(), prepare_ns),
            ("replay".into(), replay_ns),
        ]),
        gauges: BTreeMap::from([
            ("cas_losers".into(), u64::from(losers)),
            ("cas_winners".into(), u64::from(winners)),
            ("child_processes".into(), u64::from(contenders)),
            ("final_storage_bytes".into(), storage_bytes),
        ]),
    })
}

fn prepare_store(engine: StorageEngine, run_dir: &Path, events: &[EventRecord]) -> AppResult<()> {
    match engine {
        StorageEngine::SqliteWal => {
            let mut connection = create_sqlite_store(&run_dir.join("ledger.sqlite3"))?;
            append_sqlite_events(&mut connection, events)?;
        }
        StorageEngine::AppendLog => {
            let mut file = create_append_log(&run_dir.join("ledger.gtlog"))?;
            append_log_events(&mut file, events)?;
        }
    }
    Ok(())
}

fn run_sqlite_cas(
    run_dir: &Path,
    expected_head: u64,
    candidate: &EventRecord,
) -> AppResult<CasOutcome> {
    let mut connection = Connection::open(run_dir.join("ledger.sqlite3"))?;
    connection.busy_timeout(CHILD_SELF_TIMEOUT)?;
    configure_sqlite_durability(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = transaction.execute(
        "UPDATE ledger_state SET head_sequence = ?1
         WHERE singleton = 1 AND head_sequence = ?2",
        params![
            i64::try_from(candidate.sequence)?,
            i64::try_from(expected_head)?
        ],
    )?;
    let outcome = match changed {
        0 => CasOutcome::Loser,
        1 => {
            transaction.execute(
                "INSERT INTO events
                 (sequence, transaction_id, event_index, payload)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    i64::try_from(candidate.sequence)?,
                    i64::try_from(candidate.transaction)?,
                    i64::from(candidate.index),
                    &candidate.payload,
                ],
            )?;
            CasOutcome::Winner
        }
        _ => return Err(cli_error("SQLite CAS updated more than one head row")),
    };
    transaction.commit()?;
    Ok(outcome)
}

fn run_append_log_cas(
    run_dir: &Path,
    expected_head: u64,
    candidate: &EventRecord,
) -> AppResult<CasOutcome> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(run_dir.join(LOCK_FILE))?;
    lock.lock()?;
    let _lock = LockedCasFile(lock);

    let replay = replay_log(&run_dir.join("ledger.gtlog"))?;
    if replay.incomplete_tail {
        return Err(cli_error("CAS append log has an incomplete tail"));
    }
    let current_head = replay.events.last().map_or(0, |event| event.sequence);
    if current_head == candidate.sequence {
        return Ok(CasOutcome::Loser);
    }
    if current_head != expected_head {
        return Err(cli_error("append-log CAS observed an unexpected head"));
    }

    let mut file = OpenOptions::new()
        .append(true)
        .open(run_dir.join("ledger.gtlog"))?;
    write_transaction(&mut file, std::slice::from_ref(candidate))?;
    file.flush()?;
    file.sync_data()?;
    Ok(CasOutcome::Winner)
}

struct LockedCasFile(File);

impl Drop for LockedCasFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

struct CasChild {
    contender: u32,
    process_id: u32,
    child: Child,
    finished: bool,
}

struct CasChildren {
    children: Vec<CasChild>,
}

impl CasChildren {
    fn wait_until_ready(&mut self, run_dir: &Path, supervisor_token: &str) -> AppResult<()> {
        let started = Instant::now();
        loop {
            let mut all_ready = true;
            for child in &mut self.children {
                let ready_path = run_dir.join(ready_file(child.contender));
                if ready_path.try_exists()? {
                    validate_marker(
                        &ready_path,
                        &ready_marker(supervisor_token, child.process_id, child.contender),
                    )?;
                    continue;
                }
                all_ready = false;
                if let Some(status) = child.child.try_wait()? {
                    child.finished = true;
                    return Err(cli_error(format!(
                        "CAS contender {} exited before its ready marker ({status})",
                        child.contender
                    )));
                }
            }
            if all_ready {
                return Ok(());
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(cli_error(
                    "CAS contenders timed out before the start barrier",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn wait_until_complete(&mut self) -> AppResult<()> {
        let started = Instant::now();
        loop {
            let mut all_finished = true;
            for child in &mut self.children {
                if child.finished {
                    continue;
                }
                all_finished = false;
                if let Some(status) = child.child.try_wait()? {
                    child.finished = true;
                    if !status.success() {
                        return Err(cli_error(format!(
                            "CAS contender {} failed ({status})",
                            child.contender
                        )));
                    }
                }
            }
            if self.children.iter().all(|child| child.finished) {
                return Ok(());
            }
            if all_finished || started.elapsed() >= COMPLETION_TIMEOUT {
                return Err(cli_error(
                    "CAS contenders timed out after the start barrier",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for CasChildren {
    fn drop(&mut self) {
        for child in &mut self.children {
            if !child.finished {
                let _ = child.child.kill();
                let _ = child.child.wait();
                child.finished = true;
            }
        }
    }
}

fn spawn_children(
    engine: StorageEngine,
    run_dir: &Path,
    supervisor_token: &str,
    contenders: u32,
) -> AppResult<CasChildren> {
    let executable = std::env::current_exe()?;
    let mut children = CasChildren {
        children: Vec::with_capacity(usize::try_from(contenders)?),
    };
    for contender in 0..contenders {
        let child = ProcessCommand::new(&executable)
            .arg("bench")
            .arg("__storage-cas-child")
            .arg("--implementation")
            .arg(engine.implementation())
            .arg("--contender")
            .arg(contender.to_string())
            .arg("--run-dir")
            .arg(run_dir)
            .arg("--supervisor-token")
            .arg(supervisor_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        children.children.push(CasChild {
            contender,
            process_id: child.id(),
            child,
            finished: false,
        });
    }
    Ok(children)
}

fn create_coordination_files(run_dir: &Path, supervisor_token: &str) -> AppResult<()> {
    let mut supervisor = create_private_file(&run_dir.join(SUPERVISOR_FILE))?;
    supervisor.write_all(supervisor_token.as_bytes())?;
    supervisor.flush()?;
    supervisor.sync_all()?;
    drop(supervisor);

    let lock = create_private_file(&run_dir.join(LOCK_FILE))?;
    lock.sync_all()?;
    drop(lock);
    sync_directory(run_dir)
}

fn remove_coordination_files(run_dir: &Path, contenders: u32) -> AppResult<()> {
    for contender in 0..contenders {
        fs::remove_file(run_dir.join(ready_file(contender)))?;
        fs::remove_file(run_dir.join(outcome_file(contender)))?;
    }
    fs::remove_file(run_dir.join(START_FILE))?;
    fs::remove_file(run_dir.join(LOCK_FILE))?;
    fs::remove_file(run_dir.join(SUPERVISOR_FILE))?;
    sync_directory(run_dir)
}

fn validate_child_directory(run_dir: &Path, supervisor_token: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(run_dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(cli_error(
            "storage CAS child run directory must be a real directory",
        ));
    }
    let canonical_run_dir = fs::canonicalize(run_dir)?;
    if canonical_run_dir != run_dir {
        return Err(cli_error(
            "storage CAS child run directory must already be canonical",
        ));
    }
    let benchmark_name = canonical_run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cli_error("storage CAS benchmark directory name is invalid"))?;
    if !benchmark_name.starts_with("greentyper-storage-bench-") {
        return Err(cli_error(
            "storage CAS child directory is outside the benchmark namespace",
        ));
    }
    let canonical_temp = fs::canonicalize(std::env::temp_dir())?;
    if canonical_run_dir.parent() != Some(canonical_temp.as_path()) {
        return Err(cli_error(
            "storage CAS child directory is outside the system temporary directory",
        ));
    }
    validate_supervisor_file(&canonical_run_dir, supervisor_token)?;
    validate_lock_file(&canonical_run_dir)
}

fn validate_supervisor_file(run_dir: &Path, supervisor_token: &str) -> AppResult<()> {
    let path = run_dir.join(SUPERVISOR_FILE);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() != 64
    {
        return Err(cli_error("storage CAS supervisor file is invalid"));
    }
    if fs::read_to_string(path)? != supervisor_token {
        return Err(cli_error("storage CAS supervisor token does not match"));
    }
    Ok(())
}

fn validate_lock_file(run_dir: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(run_dir.join(LOCK_FILE))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() != 0 {
        return Err(cli_error("storage CAS lock file is invalid"));
    }
    Ok(())
}

fn signal_ready(run_dir: &Path, supervisor_token: &str, contender: u32) -> AppResult<()> {
    write_atomic_marker(
        run_dir,
        &ready_file(contender),
        &ready_marker(supervisor_token, std::process::id(), contender),
    )
}

fn wait_for_start(run_dir: &Path, supervisor_token: &str) -> AppResult<()> {
    let path = run_dir.join(START_FILE);
    let started = Instant::now();
    loop {
        if path.try_exists()? {
            return validate_marker(&path, &start_marker(supervisor_token));
        }
        if started.elapsed() >= CHILD_SELF_TIMEOUT {
            return Err(cli_error("storage CAS child start barrier timed out"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn read_outcome(run_dir: &Path, supervisor_token: &str, child: &CasChild) -> AppResult<CasOutcome> {
    let path = run_dir.join(outcome_file(child.contender));
    for outcome in [CasOutcome::Winner, CasOutcome::Loser] {
        if validate_marker(
            &path,
            &outcome_marker(supervisor_token, child.process_id, child.contender, outcome),
        )
        .is_ok()
        {
            return Ok(outcome);
        }
    }
    Err(cli_error(format!(
        "storage CAS outcome for contender {} is invalid",
        child.contender
    )))
}

fn write_atomic_marker(run_dir: &Path, name: &str, contents: &str) -> AppResult<()> {
    let pending = run_dir.join(format!("{name}.pending"));
    let final_path = run_dir.join(name);
    let mut file = create_private_file(&pending)?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(pending, final_path)?;
    sync_directory(run_dir)
}

fn validate_marker(path: &Path, expected: &str) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MARKER_BYTES
    {
        return Err(cli_error("storage CAS marker is invalid"));
    }
    if fs::read_to_string(path)? != expected {
        return Err(cli_error("storage CAS marker did not authenticate"));
    }
    Ok(())
}

fn generate_supervisor_token(run_dir: &Path) -> AppResult<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut entropy = RandomState::new().build_hasher();
    entropy.write_u128(timestamp);
    entropy.write_u32(std::process::id());
    entropy.write_u64(sequence);
    entropy.write(run_dir.to_string_lossy().as_bytes());

    let mut digest = Sha256::new();
    digest.update(timestamp.to_le_bytes());
    digest.update(std::process::id().to_le_bytes());
    digest.update(sequence.to_le_bytes());
    digest.update(entropy.finish().to_le_bytes());
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

fn ready_file(contender: u32) -> String {
    format!("cas-ready-{contender}")
}

fn outcome_file(contender: u32) -> String {
    format!("cas-outcome-{contender}")
}

fn ready_marker(supervisor_token: &str, process_id: u32, contender: u32) -> String {
    format!("greentyper-storage-cas-ready-v1\n{supervisor_token}\n{process_id}\n{contender}\n")
}

fn outcome_marker(
    supervisor_token: &str,
    process_id: u32,
    contender: u32,
    outcome: CasOutcome,
) -> String {
    format!(
        "greentyper-storage-cas-outcome-v1\n{supervisor_token}\n{process_id}\n{contender}\n{}\n",
        outcome.label()
    )
}

fn start_marker(supervisor_token: &str) -> String {
    format!("greentyper-storage-cas-start-v1\n{supervisor_token}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_names_and_contents_bind_process_and_contender() {
        let token = "a".repeat(64);
        assert_eq!(ready_file(7), "cas-ready-7");
        assert_eq!(outcome_file(7), "cas-outcome-7");
        assert!(ready_marker(&token, 42, 7).contains("\n42\n7\n"));
        assert!(outcome_marker(&token, 42, 7, CasOutcome::Winner).ends_with("\nwinner\n"));
        assert_ne!(
            outcome_marker(&token, 42, 7, CasOutcome::Winner),
            outcome_marker(&token, 42, 7, CasOutcome::Loser)
        );
    }
}
