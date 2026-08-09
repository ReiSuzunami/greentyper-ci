use super::*;
use rusqlite::{Connection, MAIN_DB, TransactionBehavior, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod crash;

const STORAGE_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/critical-append-replay.json"
));
const STREAMING_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/bounded-streaming-replay.json"
));
const CAS_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/cas-one-winner.json"
));
const BACKUP_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/backup-restore.json"
));
const MIGRATION_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/interrupted-migration.json"
));
const CRASH_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/cross-process-crash-replay.json"
));
const LOG_HEADER: &[u8; 8] = b"GTLG\x01\0\0\0";
const LOG_HEADER_V2: &[u8; 8] = b"GTLG\x02\0\0\0";
const TRANSACTION_MAGIC: &[u8; 4] = b"GTXN";
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_EVENTS_PER_TRANSACTION: u32 = 4_096;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) fn catalog_entry() -> serde_json::Value {
    serde_json::json!({
        "id": "storage",
        "version": 1,
        "implementations": ["sqlite-wal", "append-log"],
        "workloads": [
            {"id": "critical-append-replay", "version": 1},
            {"id": "bounded-streaming-replay", "version": 1},
            {"id": "cas-one-winner", "version": 1},
            {"id": "backup-restore", "version": 1},
            {"id": "interrupted-migration", "version": 1},
            {"id": "cross-process-crash-replay", "version": 1}
        ],
        "purpose": "candidate evidence; not a storage selection"
    })
}

pub(super) fn target(implementation: &str, workload: &str) -> AppResult<Box<dyn BenchmarkTarget>> {
    let (workload, fixture_bytes) = StorageWorkload::resolve(workload)?;
    let fixture: StorageFixture = serde_json::from_slice(fixture_bytes)?;
    validate_fixture(&fixture, workload)?;
    let engine = match implementation {
        "sqlite-wal" => StorageEngine::SqliteWal,
        "append-log" => StorageEngine::AppendLog,
        _ => {
            return Err(cli_error(format!(
                "benchmark implementation storage/{implementation} is not compiled into this runner"
            )));
        }
    };
    let events = generate_events(&fixture)?;
    Ok(Box::new(StorageTarget {
        engine,
        workload,
        fixture,
        fixture_bytes,
        events,
        run_dir: None,
    }))
}

pub(super) fn run_crash_child(options: StorageCrashChildOptions) -> AppResult<()> {
    crash::run_child(options)
}

#[derive(Clone, Debug, Deserialize)]
struct StorageFixture {
    schema_version: u16,
    comparison_id: String,
    workload_id: String,
    workload_version: u16,
    transactions: u32,
    events_per_transaction: u32,
    payload_bytes: u32,
    #[serde(default)]
    max_batch_events: Option<u32>,
    #[serde(default)]
    cas_contenders: Option<u32>,
    #[serde(default)]
    backup_restore_cycles: Option<u32>,
    #[serde(default)]
    migration_interruptions: Option<u32>,
    #[serde(default)]
    crash_cases: Option<u32>,
}

fn validate_fixture(fixture: &StorageFixture, workload: StorageWorkload) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    let shape_is_valid = match workload {
        StorageWorkload::CriticalAppendReplay => {
            fixture.workload_id == "critical-append-replay"
                && fixture.transactions == 16
                && fixture.events_per_transaction == 4
                && fixture.payload_bytes == 256
                && fixture.max_batch_events.is_none()
                && fixture.cas_contenders.is_none()
                && fixture.backup_restore_cycles.is_none()
                && fixture.migration_interruptions.is_none()
                && fixture.crash_cases.is_none()
        }
        StorageWorkload::BoundedStreamingReplay => {
            fixture.workload_id == "bounded-streaming-replay"
                && fixture.transactions == 8
                && fixture.events_per_transaction == 32
                && fixture.payload_bytes == 128
                && fixture.max_batch_events == Some(32)
                && fixture.cas_contenders.is_none()
                && fixture.backup_restore_cycles.is_none()
                && fixture.migration_interruptions.is_none()
                && fixture.crash_cases.is_none()
        }
        StorageWorkload::CasOneWinner => {
            fixture.workload_id == "cas-one-winner"
                && fixture.transactions == 16
                && fixture.events_per_transaction == 4
                && fixture.payload_bytes == 256
                && fixture.max_batch_events.is_none()
                && fixture.cas_contenders == Some(8)
                && fixture.backup_restore_cycles.is_none()
                && fixture.migration_interruptions.is_none()
                && fixture.crash_cases.is_none()
        }
        StorageWorkload::BackupRestore => {
            fixture.workload_id == "backup-restore"
                && fixture.transactions == 16
                && fixture.events_per_transaction == 4
                && fixture.payload_bytes == 256
                && fixture.max_batch_events.is_none()
                && fixture.cas_contenders.is_none()
                && fixture.backup_restore_cycles == Some(1)
                && fixture.migration_interruptions.is_none()
                && fixture.crash_cases.is_none()
        }
        StorageWorkload::InterruptedMigration => {
            fixture.workload_id == "interrupted-migration"
                && fixture.transactions == 16
                && fixture.events_per_transaction == 4
                && fixture.payload_bytes == 256
                && fixture.max_batch_events.is_none()
                && fixture.cas_contenders.is_none()
                && fixture.backup_restore_cycles.is_none()
                && fixture.migration_interruptions == Some(3)
                && fixture.crash_cases.is_none()
        }
        StorageWorkload::CrossProcessCrashReplay => {
            fixture.workload_id == "cross-process-crash-replay"
                && fixture.transactions == 4
                && fixture.events_per_transaction == 4
                && fixture.payload_bytes == 128
                && fixture.max_batch_events.is_none()
                && fixture.cas_contenders.is_none()
                && fixture.backup_restore_cycles.is_none()
                && fixture.migration_interruptions.is_none()
                && fixture.crash_cases == Some(6)
        }
    };
    if fixture.comparison_id != "storage" || fixture.workload_version != 1 || !shape_is_valid {
        return Err(cli_error("storage benchmark fixture is invalid"));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventRecord {
    sequence: u64,
    transaction: u64,
    index: u32,
    payload: Vec<u8>,
}

fn generate_events(fixture: &StorageFixture) -> AppResult<Vec<EventRecord>> {
    let event_count = fixture
        .transactions
        .checked_mul(fixture.events_per_transaction)
        .ok_or_else(|| cli_error("storage fixture event count overflow"))?;
    let mut events = Vec::with_capacity(usize::try_from(event_count)?);
    for transaction_index in 0..fixture.transactions {
        let transaction = u64::from(transaction_index) + 1;
        for index in 0..fixture.events_per_transaction {
            let sequence = u64::try_from(events.len())?
                .checked_add(1)
                .ok_or_else(|| cli_error("storage event sequence overflow"))?;
            let seed = (sequence as u8)
                .wrapping_mul(31)
                .wrapping_add(transaction as u8);
            let payload = (0..fixture.payload_bytes)
                .map(|offset| seed.wrapping_add(offset as u8))
                .collect();
            events.push(EventRecord {
                sequence,
                transaction,
                index,
                payload,
            });
        }
    }
    Ok(events)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageEngine {
    SqliteWal,
    AppendLog,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageWorkload {
    CriticalAppendReplay,
    BoundedStreamingReplay,
    CasOneWinner,
    BackupRestore,
    InterruptedMigration,
    CrossProcessCrashReplay,
}

impl StorageWorkload {
    fn resolve(workload: &str) -> AppResult<(Self, &'static [u8])> {
        match workload {
            "critical-append-replay" => {
                Ok((Self::CriticalAppendReplay, STORAGE_FIXTURE_JSON.as_bytes()))
            }
            "bounded-streaming-replay" => Ok((
                Self::BoundedStreamingReplay,
                STREAMING_FIXTURE_JSON.as_bytes(),
            )),
            "cas-one-winner" => Ok((Self::CasOneWinner, CAS_FIXTURE_JSON.as_bytes())),
            "backup-restore" => Ok((Self::BackupRestore, BACKUP_FIXTURE_JSON.as_bytes())),
            "interrupted-migration" => Ok((
                Self::InterruptedMigration,
                MIGRATION_FIXTURE_JSON.as_bytes(),
            )),
            "cross-process-crash-replay" => {
                Ok((Self::CrossProcessCrashReplay, CRASH_FIXTURE_JSON.as_bytes()))
            }
            _ => Err(cli_error(format!(
                "benchmark workload storage/{workload} is not compiled into this runner"
            ))),
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::CriticalAppendReplay => "critical-append-replay",
            Self::BoundedStreamingReplay => "bounded-streaming-replay",
            Self::CasOneWinner => "cas-one-winner",
            Self::BackupRestore => "backup-restore",
            Self::InterruptedMigration => "interrupted-migration",
            Self::CrossProcessCrashReplay => "cross-process-crash-replay",
        }
    }

    const fn input_shape(self) -> &'static str {
        match self {
            Self::CriticalAppendReplay => "16 sync transactions x 4 events x 256 payload bytes",
            Self::BoundedStreamingReplay => "8 max-event batches x 32 events x 128 payload bytes",
            Self::CasOneWinner => {
                "64-event Ledger followed by 8 CAS contenders at one expected head"
            }
            Self::BackupRestore => "64-event Ledger backed up once and restored into a fresh store",
            Self::InterruptedMigration => {
                "64-event v1 Ledger with 3 interruption boundaries before v2 recovery"
            }
            Self::CrossProcessCrashReplay => {
                "16-event Ledger with 6 child-process termination and restart cases"
            }
        }
    }

    const fn unit(self) -> &'static str {
        match self {
            Self::CriticalAppendReplay => "events synchronously committed and replayed",
            Self::BoundedStreamingReplay => {
                "stream events committed in bounded batches and replayed"
            }
            Self::CasOneWinner => "CAS contenders resolved with exactly one winner",
            Self::BackupRestore => "events backed up, restored, replayed, and verified",
            Self::InterruptedMigration => {
                "migration interruption boundaries recovered as complete v1 or v2"
            }
            Self::CrossProcessCrashReplay => {
                "child crashes recovered as known-not-repeated or ambiguous-blocked"
            }
        }
    }

    const fn boundary(self) -> &'static str {
        match self {
            Self::CriticalAppendReplay => {
                "create store, append 16 synchronous transactions, close, reopen, replay, and verify"
            }
            Self::BoundedStreamingReplay => {
                "create store, commit 8 max-event streaming batches, close, reopen, replay, and verify"
            }
            Self::CasOneWinner => {
                "prepare a 64-event store, evaluate 8 stale-head CAS contenders, reopen, and verify one winner"
            }
            Self::BackupRestore => {
                "prepare a 64-event store, create a verified backup, restore into a fresh store, replay, and verify"
            }
            Self::InterruptedMigration => {
                "prepare v1, interrupt migration before commit boundaries, publish v2, reopen, and verify no mixed schema"
            }
            Self::CrossProcessCrashReplay => {
                "spawn and terminate one child before write, at four transaction progress points, and after sync before acknowledgement; restart and reconcile"
            }
        }
    }
}

impl StorageEngine {
    const fn implementation(self) -> &'static str {
        match self {
            Self::SqliteWal => "sqlite-wal",
            Self::AppendLog => "append-log",
        }
    }

    const fn dependencies(self) -> &'static str {
        match self {
            Self::SqliteWal => "rusqlite=0.40.2;default-features=false;features=backup,bundled",
            Self::AppendLog => "crc32c=0.6.8;features=default",
        }
    }
}

struct StorageTarget {
    engine: StorageEngine,
    workload: StorageWorkload,
    fixture: StorageFixture,
    fixture_bytes: &'static [u8],
    events: Vec<EventRecord>,
    run_dir: Option<PathBuf>,
}

impl BenchmarkTarget for StorageTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "storage",
            comparison_version: 1,
            implementation: self.engine.implementation(),
            implementation_revision: "3",
            dependencies: self.engine.dependencies(),
            workload_id: self.workload.id(),
            workload_version: self.fixture.workload_version,
            input_shape: self.workload.input_shape(),
            unit: self.workload.unit(),
            boundary: self.workload.boundary(),
            process_mode: match self.workload {
                StorageWorkload::CrossProcessCrashReplay => "cross-process",
                _ => "in-process",
            },
            fixture_bytes: self.fixture_bytes,
        }
    }

    fn prepare_run(&mut self) -> AppResult<()> {
        if self.run_dir.is_some() {
            return Err(cli_error(
                "storage benchmark run directory is already active",
            ));
        }
        let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!(
            "greentyper-storage-bench-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        create_private_directory(&path)?;
        self.run_dir = Some(fs::canonicalize(path)?);
        Ok(())
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        let run_dir = self
            .run_dir
            .as_deref()
            .ok_or_else(|| cli_error("storage benchmark run directory was not prepared"))?;
        match self.workload {
            StorageWorkload::CasOneWinner => {
                return run_cas_workload(
                    self.engine,
                    run_dir,
                    &self.events,
                    self.fixture
                        .cas_contenders
                        .ok_or_else(|| cli_error("CAS fixture has no contender count"))?,
                );
            }
            StorageWorkload::BackupRestore => {
                return run_backup_restore_workload(self.engine, run_dir, &self.events);
            }
            StorageWorkload::InterruptedMigration => {
                return run_interrupted_migration_workload(
                    self.engine,
                    run_dir,
                    &self.events,
                    self.fixture.migration_interruptions.ok_or_else(|| {
                        cli_error("migration fixture has no interruption boundary count")
                    })?,
                );
            }
            StorageWorkload::CrossProcessCrashReplay => {
                return crash::run_workload(
                    self.engine,
                    run_dir,
                    &self.events,
                    self.fixture
                        .crash_cases
                        .ok_or_else(|| cli_error("crash fixture has no case count"))?,
                );
            }
            StorageWorkload::CriticalAppendReplay | StorageWorkload::BoundedStreamingReplay => {}
        }
        let observed_max_batch_events = match self.workload {
            StorageWorkload::BoundedStreamingReplay => Some(validate_max_event_batches(
                &self.events,
                self.fixture
                    .max_batch_events
                    .ok_or_else(|| cli_error("streaming fixture has no batch event limit"))?,
            )?),
            _ => None,
        };
        let observation = match self.engine {
            StorageEngine::SqliteWal => run_sqlite(run_dir, &self.events)?,
            StorageEngine::AppendLog => run_append_log(run_dir, &self.events)?,
        };
        if observation.replayed != self.events {
            return Err(cli_error(
                "storage benchmark replay differs from canonical events",
            ));
        }
        let mut gauges = BTreeMap::from([
            (
                "batch_event_limit".into(),
                u64::from(
                    self.fixture
                        .max_batch_events
                        .unwrap_or(self.fixture.events_per_transaction),
                ),
            ),
            (
                "final_storage_bytes".into(),
                observation.final_storage_bytes,
            ),
            (
                "post_append_storage_bytes".into(),
                observation.post_append_storage_bytes,
            ),
            (
                "transaction_count".into(),
                u64::from(self.fixture.transactions),
            ),
        ]);
        if let Some(observed) = observed_max_batch_events {
            gauges.insert("observed_max_batch_events".into(), observed);
        }
        Ok(BenchmarkObservation {
            operation_units: u64::try_from(observation.replayed.len())?,
            output_digest: canonical_digest(&observation.replayed)?,
            timings_ns: BTreeMap::from([
                ("append".into(), observation.append_ns),
                ("replay".into(), observation.replay_ns),
                ("setup".into(), observation.setup_ns),
            ]),
            gauges,
        })
    }

    fn cleanup_run(&mut self) -> AppResult<()> {
        let path = self
            .run_dir
            .as_deref()
            .ok_or_else(|| cli_error("storage benchmark run directory was not active"))?;
        crash::require_no_active_children(path)?;
        let path = self
            .run_dir
            .take()
            .ok_or_else(|| cli_error("storage benchmark run directory was not active"))?;
        fs::remove_dir_all(path)?;
        Ok(())
    }
}

struct StorageObservation {
    replayed: Vec<EventRecord>,
    setup_ns: u64,
    append_ns: u64,
    replay_ns: u64,
    post_append_storage_bytes: u64,
    final_storage_bytes: u64,
}

fn run_cas_workload(
    engine: StorageEngine,
    run_dir: &Path,
    events: &[EventRecord],
    contenders: u32,
) -> AppResult<BenchmarkObservation> {
    if contenders < 2 {
        return Err(cli_error("CAS workload requires at least two contenders"));
    }
    let candidate = cas_event(events)?;
    let expected_head = candidate
        .sequence
        .checked_sub(1)
        .ok_or_else(|| cli_error("CAS candidate sequence starts at zero"))?;
    let (replayed, prepare_ns, cas_ns, replay_ns, winners, storage_bytes) = match engine {
        StorageEngine::SqliteWal => {
            let path = run_dir.join("ledger.sqlite3");
            let prepare_started = Instant::now();
            let mut connection = create_sqlite_store(&path)?;
            append_sqlite_events(&mut connection, events)?;
            let prepare_ns = elapsed_ns(prepare_started)?;

            let cas_started = Instant::now();
            let mut winners = 0_u32;
            for _ in 0..contenders {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = transaction.execute(
                    "UPDATE ledger_state SET head_sequence = ?1
                     WHERE singleton = 1 AND head_sequence = ?2",
                    params![
                        i64::try_from(candidate.sequence)?,
                        i64::try_from(expected_head)?
                    ],
                )?;
                if changed == 1 {
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
                    winners = winners
                        .checked_add(1)
                        .ok_or_else(|| cli_error("CAS winner count overflow"))?;
                }
                transaction.commit()?;
            }
            let cas_ns = elapsed_ns(cas_started)?;
            drop(connection);

            let replay_started = Instant::now();
            let replayed = replay_sqlite(&path)?;
            let replay_ns = elapsed_ns(replay_started)?;
            let storage_bytes = directory_size(run_dir)?;
            (
                replayed,
                prepare_ns,
                cas_ns,
                replay_ns,
                winners,
                storage_bytes,
            )
        }
        StorageEngine::AppendLog => {
            let path = run_dir.join("ledger.gtlog");
            let prepare_started = Instant::now();
            let mut file = create_append_log(&path)?;
            append_log_events(&mut file, events)?;
            let prepare_ns = elapsed_ns(prepare_started)?;

            let cas_started = Instant::now();
            let mut winners = 0_u32;
            let mut current_head = expected_head;
            for _ in 0..contenders {
                if current_head == expected_head {
                    write_transaction(&mut file, std::slice::from_ref(&candidate))?;
                    file.flush()?;
                    file.sync_data()?;
                    current_head = candidate.sequence;
                    winners = winners
                        .checked_add(1)
                        .ok_or_else(|| cli_error("CAS winner count overflow"))?;
                }
            }
            let cas_ns = elapsed_ns(cas_started)?;
            drop(file);

            let replay_started = Instant::now();
            let outcome = replay_log(&path)?;
            if outcome.incomplete_tail {
                return Err(cli_error("CAS append log has an incomplete tail"));
            }
            let replay_ns = elapsed_ns(replay_started)?;
            let storage_bytes = directory_size(run_dir)?;
            (
                outcome.events,
                prepare_ns,
                cas_ns,
                replay_ns,
                winners,
                storage_bytes,
            )
        }
    };

    let mut expected = events.to_vec();
    expected.push(candidate);
    if winners != 1 || replayed != expected {
        return Err(cli_error(
            "storage CAS did not produce exactly one canonical winner",
        ));
    }
    Ok(BenchmarkObservation {
        operation_units: u64::from(contenders),
        output_digest: canonical_digest(&replayed)?,
        timings_ns: BTreeMap::from([
            ("cas".into(), cas_ns),
            ("prepare".into(), prepare_ns),
            ("replay".into(), replay_ns),
        ]),
        gauges: BTreeMap::from([
            ("cas_losers".into(), u64::from(contenders - winners)),
            ("cas_winners".into(), u64::from(winners)),
            ("final_storage_bytes".into(), storage_bytes),
        ]),
    })
}

fn cas_event(events: &[EventRecord]) -> AppResult<EventRecord> {
    let last = events
        .last()
        .ok_or_else(|| cli_error("CAS workload has no base events"))?;
    Ok(EventRecord {
        sequence: last
            .sequence
            .checked_add(1)
            .ok_or_else(|| cli_error("CAS event sequence overflow"))?,
        transaction: last
            .transaction
            .checked_add(1)
            .ok_or_else(|| cli_error("CAS transaction identity overflow"))?,
        index: 0,
        payload: b"GreenTyper checkpoint CAS winner v1".to_vec(),
    })
}

fn run_backup_restore_workload(
    engine: StorageEngine,
    run_dir: &Path,
    events: &[EventRecord],
) -> AppResult<BenchmarkObservation> {
    let (replayed, prepare_ns, backup_ns, restore_ns, backup_bytes, restored_bytes) = match engine {
        StorageEngine::SqliteWal => {
            let source_path = run_dir.join("source.sqlite3");
            let backup_path = run_dir.join("backup.sqlite3");
            let restored_path = run_dir.join("restored.sqlite3");
            let prepare_started = Instant::now();
            let mut source = create_sqlite_store(&source_path)?;
            append_sqlite_events(&mut source, events)?;
            let prepare_ns = elapsed_ns(prepare_started)?;

            let backup_started = Instant::now();
            source.backup(MAIN_DB, &backup_path, None)?;
            let backup_replay = replay_sqlite(&backup_path)?;
            if backup_replay != events {
                return Err(cli_error("SQLite backup differs from source events"));
            }
            let backup_ns = elapsed_ns(backup_started)?;
            let backup_bytes = fs::metadata(&backup_path)?.len();
            drop(source);

            let restore_started = Instant::now();
            let mut restored = Connection::open(&restored_path)?;
            restored.restore(
                MAIN_DB,
                &backup_path,
                None::<fn(rusqlite::backup::Progress)>,
            )?;
            drop(restored);
            let replayed = replay_sqlite(&restored_path)?;
            let restore_ns = elapsed_ns(restore_started)?;
            let restored_bytes = fs::metadata(&restored_path)?.len();
            (
                replayed,
                prepare_ns,
                backup_ns,
                restore_ns,
                backup_bytes,
                restored_bytes,
            )
        }
        StorageEngine::AppendLog => {
            let source_path = run_dir.join("source.gtlog");
            let backup_path = run_dir.join("backup.gtlog");
            let restored_path = run_dir.join("restored.gtlog");
            let prepare_started = Instant::now();
            let mut source = create_append_log(&source_path)?;
            append_log_events(&mut source, events)?;
            drop(source);
            let prepare_ns = elapsed_ns(prepare_started)?;

            let backup_started = Instant::now();
            publish_durable_copy(&source_path, &backup_path)?;
            let backup_replay = replay_log(&backup_path)?;
            if backup_replay.incomplete_tail || backup_replay.events != events {
                return Err(cli_error("append-log backup differs from source events"));
            }
            let backup_ns = elapsed_ns(backup_started)?;
            let backup_bytes = fs::metadata(&backup_path)?.len();

            let restore_started = Instant::now();
            publish_durable_copy(&backup_path, &restored_path)?;
            let restored = replay_log(&restored_path)?;
            if restored.incomplete_tail {
                return Err(cli_error("restored append log has an incomplete tail"));
            }
            let restore_ns = elapsed_ns(restore_started)?;
            let restored_bytes = fs::metadata(&restored_path)?.len();
            (
                restored.events,
                prepare_ns,
                backup_ns,
                restore_ns,
                backup_bytes,
                restored_bytes,
            )
        }
    };

    if replayed != events {
        return Err(cli_error(
            "restored storage candidate differs from canonical events",
        ));
    }
    Ok(BenchmarkObservation {
        operation_units: u64::try_from(replayed.len())?,
        output_digest: canonical_digest(&replayed)?,
        timings_ns: BTreeMap::from([
            ("backup".into(), backup_ns),
            ("prepare".into(), prepare_ns),
            ("restore_and_replay".into(), restore_ns),
        ]),
        gauges: BTreeMap::from([
            ("backup_bytes".into(), backup_bytes),
            ("restored_bytes".into(), restored_bytes),
        ]),
    })
}

fn run_interrupted_migration_workload(
    engine: StorageEngine,
    run_dir: &Path,
    events: &[EventRecord],
    expected_boundaries: u32,
) -> AppResult<BenchmarkObservation> {
    let (replayed, prepare_ns, migration_ns, old_recoveries, new_recoveries, schema_version) =
        match engine {
            StorageEngine::SqliteWal => run_sqlite_migration(run_dir, events)?,
            StorageEngine::AppendLog => run_append_log_migration(run_dir, events)?,
        };
    if replayed != events
        || old_recoveries + new_recoveries != u64::from(expected_boundaries)
        || old_recoveries != 2
        || new_recoveries != 1
        || schema_version != 2
    {
        return Err(cli_error(
            "storage migration did not recover as exactly two v1 states and one v2 state",
        ));
    }
    Ok(BenchmarkObservation {
        operation_units: old_recoveries + new_recoveries,
        output_digest: canonical_digest(&replayed)?,
        timings_ns: BTreeMap::from([
            ("migration_and_recovery".into(), migration_ns),
            ("prepare".into(), prepare_ns),
        ]),
        gauges: BTreeMap::from([
            ("final_schema_version".into(), schema_version),
            ("final_storage_bytes".into(), directory_size(run_dir)?),
            ("new_generation_recoveries".into(), new_recoveries),
            ("old_generation_recoveries".into(), old_recoveries),
        ]),
    })
}

fn run_sqlite_migration(
    run_dir: &Path,
    events: &[EventRecord],
) -> AppResult<(Vec<EventRecord>, u64, u64, u64, u64, u64)> {
    let path = run_dir.join("ledger.sqlite3");
    let prepare_started = Instant::now();
    let mut connection = create_sqlite_store(&path)?;
    append_sqlite_events(&mut connection, events)?;
    drop(connection);
    verify_sqlite_schema(&path, 1, false, events)?;
    let prepare_ns = elapsed_ns(prepare_started)?;

    let migration_started = Instant::now();
    rollback_sqlite_migration(&path, false)?;
    verify_sqlite_schema(&path, 1, false, events)?;
    rollback_sqlite_migration(&path, true)?;
    verify_sqlite_schema(&path, 1, false, events)?;
    commit_sqlite_migration(&path)?;
    let replayed = verify_sqlite_schema(&path, 2, true, events)?;
    let migration_ns = elapsed_ns(migration_started)?;
    Ok((replayed, prepare_ns, migration_ns, 2, 1, 2))
}

fn rollback_sqlite_migration(database_path: &Path, include_backfill: bool) -> AppResult<()> {
    let mut connection = Connection::open(database_path)?;
    configure_sqlite_durability(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch("ALTER TABLE events ADD COLUMN payload_size INTEGER;")?;
    if include_backfill {
        transaction.execute_batch(
            "UPDATE events SET payload_size = length(payload);
             PRAGMA user_version = 2;",
        )?;
    }
    transaction.rollback()?;
    Ok(())
}

fn commit_sqlite_migration(database_path: &Path) -> AppResult<()> {
    let mut connection = Connection::open(database_path)?;
    configure_sqlite_durability(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE events ADD COLUMN payload_size INTEGER;
         UPDATE events SET payload_size = length(payload);
         PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn verify_sqlite_schema(
    database_path: &Path,
    expected_version: u64,
    expect_payload_size: bool,
    expected_events: &[EventRecord],
) -> AppResult<Vec<EventRecord>> {
    let connection = Connection::open(database_path)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let has_payload_size = sqlite_column_exists(&connection, "events", "payload_size")?;
    if u64::try_from(version)? != expected_version || has_payload_size != expect_payload_size {
        return Err(cli_error("SQLite migration exposed a mixed schema"));
    }
    if expect_payload_size {
        let invalid_rows: i64 = connection.query_row(
            "SELECT COUNT(*) FROM events
             WHERE payload_size IS NULL OR payload_size != length(payload)",
            [],
            |row| row.get(0),
        )?;
        if invalid_rows != 0 {
            return Err(cli_error("SQLite migration left invalid payload sizes"));
        }
    }
    drop(connection);
    let replayed = replay_sqlite(database_path)?;
    if replayed != expected_events {
        return Err(cli_error("SQLite migration changed canonical events"));
    }
    Ok(replayed)
}

fn sqlite_column_exists(connection: &Connection, table: &str, column: &str) -> AppResult<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run_append_log_migration(
    run_dir: &Path,
    events: &[EventRecord],
) -> AppResult<(Vec<EventRecord>, u64, u64, u64, u64, u64)> {
    let v1_path = run_dir.join("ledger-v1.gtlog");
    let v2_partial_path = run_dir.join("ledger-v2.partial");
    let v2_temporary_path = run_dir.join(".ledger-v2.gtlog.tmp");
    let v2_path = run_dir.join("ledger-v2.gtlog");

    let prepare_started = Instant::now();
    let mut v1 = create_append_log(&v1_path)?;
    append_log_events(&mut v1, events)?;
    drop(v1);
    let initial = select_append_log_generation(run_dir)?;
    if initial.format_version != 1 || initial.events != events {
        return Err(cli_error("append-log v1 generation is invalid"));
    }
    let prepare_ns = elapsed_ns(prepare_started)?;

    let migration_started = Instant::now();
    let first_transaction = transaction_slices(events)?
        .into_iter()
        .next()
        .ok_or_else(|| cli_error("migration workload has no transaction"))?;
    let mut partial = create_append_log_with_header(&v2_partial_path, LOG_HEADER_V2)?;
    write_transaction(&mut partial, first_transaction)?;
    partial.flush()?;
    partial.sync_all()?;
    let partial_length = partial.metadata()?.len();
    if partial_length <= u64::try_from(LOG_HEADER_V2.len())? {
        return Err(cli_error("append-log migration partial frame is empty"));
    }
    partial.set_len(partial_length - 1)?;
    partial.sync_all()?;
    drop(partial);
    let incomplete = replay_log(&v2_partial_path)?;
    if incomplete.format_version != 2 || !incomplete.incomplete_tail {
        return Err(cli_error(
            "append-log partial v2 generation was not detected",
        ));
    }
    let old_after_partial = select_append_log_generation(run_dir)?;
    if old_after_partial.format_version != 1 || old_after_partial.events != events {
        return Err(cli_error("append-log selected an unpublished generation"));
    }

    let mut temporary = create_append_log_with_header(&v2_temporary_path, LOG_HEADER_V2)?;
    append_log_events(&mut temporary, events)?;
    drop(temporary);
    let candidate = replay_log(&v2_temporary_path)?;
    if candidate.format_version != 2 || candidate.incomplete_tail || candidate.events != events {
        return Err(cli_error("append-log complete v2 candidate is invalid"));
    }
    let old_before_publish = select_append_log_generation(run_dir)?;
    if old_before_publish.format_version != 1 || old_before_publish.events != events {
        return Err(cli_error("append-log selected v2 before publication"));
    }

    fs::rename(&v2_temporary_path, &v2_path)?;
    sync_directory(run_dir)?;
    let published = select_append_log_generation(run_dir)?;
    if published.format_version != 2 || published.incomplete_tail || published.events != events {
        return Err(cli_error("append-log published v2 generation is invalid"));
    }
    let migration_ns = elapsed_ns(migration_started)?;
    Ok((published.events, prepare_ns, migration_ns, 2, 1, 2))
}

fn select_append_log_generation(run_dir: &Path) -> AppResult<ReplayOutcome> {
    let v1 = replay_log(&run_dir.join("ledger-v1.gtlog"))?;
    if v1.format_version != 1 || v1.incomplete_tail {
        return Err(cli_error("append-log v1 migration base is invalid"));
    }
    let v2_path = run_dir.join("ledger-v2.gtlog");
    let selected = if v2_path.exists() {
        let v2 = replay_log(&v2_path)?;
        if v2.format_version != 2 || v2.events != v1.events {
            return Err(cli_error(
                "append-log v2 generation does not match its migration base",
            ));
        }
        v2
    } else {
        v1
    };
    if selected.incomplete_tail {
        return Err(cli_error(
            "selected append-log generation has an incomplete tail",
        ));
    }
    Ok(selected)
}

fn publish_durable_copy(source: &Path, destination: &Path) -> AppResult<()> {
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| cli_error("backup destination has no UTF-8 file name"))?;
    let temporary = destination.with_file_name(format!(".{file_name}.tmp"));
    let mut reader = File::open(source)?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    writer.sync_all()?;
    drop(writer);
    fs::rename(&temporary, destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| cli_error("backup destination has no parent directory"))?;
    sync_directory(parent)?;
    Ok(())
}

fn run_sqlite(run_dir: &Path, events: &[EventRecord]) -> AppResult<StorageObservation> {
    let database_path = run_dir.join("ledger.sqlite3");
    let setup_started = Instant::now();
    let mut connection = create_sqlite_store(&database_path)?;
    let setup_ns = elapsed_ns(setup_started)?;

    let append_started = Instant::now();
    append_sqlite_events(&mut connection, events)?;
    let append_ns = elapsed_ns(append_started)?;
    let post_append_storage_bytes = directory_size(run_dir)?;
    drop(connection);

    let replay_started = Instant::now();
    let replayed = replay_sqlite(&database_path)?;
    validate_event_sequence(&replayed)?;
    let replay_ns = elapsed_ns(replay_started)?;
    let final_storage_bytes = directory_size(run_dir)?;

    Ok(StorageObservation {
        replayed,
        setup_ns,
        append_ns,
        replay_ns,
        post_append_storage_bytes,
        final_storage_bytes,
    })
}

fn create_sqlite_store(database_path: &Path) -> AppResult<Connection> {
    let connection = Connection::open(database_path)?;
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(cli_error(format!(
            "SQLite refused WAL mode and returned {journal_mode}"
        )));
    }
    connection.execute_batch(
        "PRAGMA synchronous=FULL;
         PRAGMA wal_autocheckpoint=0;
         CREATE TABLE events (
             sequence INTEGER PRIMARY KEY,
             transaction_id INTEGER NOT NULL,
             event_index INTEGER NOT NULL,
             payload BLOB NOT NULL
         );
         CREATE TABLE ledger_state (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             head_sequence INTEGER NOT NULL
         );
         INSERT INTO ledger_state (singleton, head_sequence) VALUES (1, 0);
         PRAGMA user_version = 1;",
    )?;
    configure_sqlite_durability(&connection)?;
    Ok(connection)
}

fn configure_sqlite_durability(connection: &Connection) -> AppResult<()> {
    connection.execute_batch(
        "PRAGMA synchronous=FULL;
         PRAGMA wal_autocheckpoint=0;",
    )?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if synchronous != 2 {
        return Err(cli_error(format!(
            "SQLite synchronous mode is {synchronous}; expected FULL (2)"
        )));
    }
    Ok(())
}

fn append_sqlite_events(connection: &mut Connection, events: &[EventRecord]) -> AppResult<()> {
    for transaction_events in transaction_slices(events)? {
        let first = transaction_events
            .first()
            .ok_or_else(|| cli_error("cannot append an empty SQLite transaction"))?;
        let last = transaction_events
            .last()
            .ok_or_else(|| cli_error("cannot append an empty SQLite transaction"))?;
        let expected_head = first
            .sequence
            .checked_sub(1)
            .ok_or_else(|| cli_error("SQLite transaction sequence starts at zero"))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO events (sequence, transaction_id, event_index, payload)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for event in transaction_events {
                statement.execute(params![
                    i64::try_from(event.sequence)?,
                    i64::try_from(event.transaction)?,
                    i64::from(event.index),
                    &event.payload,
                ])?;
            }
        }
        let changed = transaction.execute(
            "UPDATE ledger_state SET head_sequence = ?1
             WHERE singleton = 1 AND head_sequence = ?2",
            params![i64::try_from(last.sequence)?, i64::try_from(expected_head)?],
        )?;
        if changed != 1 {
            return Err(cli_error("SQLite Ledger head compare-and-swap failed"));
        }
        transaction.commit()?;
    }
    Ok(())
}

fn replay_sqlite(database_path: &Path) -> AppResult<Vec<EventRecord>> {
    let replay_connection = Connection::open(database_path)?;
    let integrity: String =
        replay_connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(cli_error(format!(
            "SQLite integrity check failed: {integrity}"
        )));
    }
    let schema_version: i64 =
        replay_connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let has_payload_size = sqlite_column_exists(&replay_connection, "events", "payload_size")?;
    match (schema_version, has_payload_size) {
        (1, false) => {}
        (2, true) => {
            let invalid_rows: i64 = replay_connection.query_row(
                "SELECT COUNT(*) FROM events
                 WHERE payload_size IS NULL OR payload_size != length(payload)",
                [],
                |row| row.get(0),
            )?;
            if invalid_rows != 0 {
                return Err(cli_error("SQLite v2 payload sizes are invalid"));
            }
        }
        (1 | 2, _) => return Err(cli_error("SQLite Ledger schema is mixed")),
        _ => return Err(cli_error("SQLite Ledger schema version is unsupported")),
    }
    let replayed = read_sqlite_events(&replay_connection)?;
    validate_event_sequence(&replayed)?;
    let stored_head: i64 = replay_connection.query_row(
        "SELECT head_sequence FROM ledger_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let actual_head = replayed.last().map_or(0, |event| event.sequence);
    if u64::try_from(stored_head)? != actual_head {
        return Err(cli_error("SQLite Ledger head differs from replayed events"));
    }
    Ok(replayed)
}

fn read_sqlite_events(connection: &Connection) -> AppResult<Vec<EventRecord>> {
    let replayed = {
        let mut statement = connection.prepare(
            "SELECT sequence, transaction_id, event_index, payload
             FROM events ORDER BY sequence",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut replayed = Vec::new();
        for row in rows {
            let (sequence, transaction, index, payload) = row?;
            replayed.push(EventRecord {
                sequence: u64::try_from(sequence)?,
                transaction: u64::try_from(transaction)?,
                index: u32::try_from(index)?,
                payload,
            });
        }
        replayed
    };
    Ok(replayed)
}

fn run_append_log(run_dir: &Path, events: &[EventRecord]) -> AppResult<StorageObservation> {
    let log_path = run_dir.join("ledger.gtlog");
    let setup_started = Instant::now();
    let mut file = create_append_log(&log_path)?;
    let setup_ns = elapsed_ns(setup_started)?;

    let append_started = Instant::now();
    append_log_events(&mut file, events)?;
    let append_ns = elapsed_ns(append_started)?;
    let post_append_storage_bytes = directory_size(run_dir)?;
    drop(file);

    let replay_started = Instant::now();
    let outcome = replay_log(&log_path)?;
    if outcome.incomplete_tail {
        return Err(cli_error(
            "complete append-log benchmark ended with an incomplete tail",
        ));
    }
    validate_event_sequence(&outcome.events)?;
    let replay_ns = elapsed_ns(replay_started)?;
    let final_storage_bytes = directory_size(run_dir)?;

    Ok(StorageObservation {
        replayed: outcome.events,
        setup_ns,
        append_ns,
        replay_ns,
        post_append_storage_bytes,
        final_storage_bytes,
    })
}

fn create_append_log(log_path: &Path) -> AppResult<File> {
    create_append_log_with_header(log_path, LOG_HEADER)
}

fn create_append_log_with_header(log_path: &Path, header: &[u8; 8]) -> AppResult<File> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(log_path)?;
    file.write_all(header)?;
    file.flush()?;
    file.sync_all()?;
    let parent = log_path
        .parent()
        .ok_or_else(|| cli_error("append-log path has no parent directory"))?;
    sync_directory(parent)?;
    Ok(file)
}

fn append_log_events(file: &mut File, events: &[EventRecord]) -> AppResult<()> {
    for transaction_events in transaction_slices(events)? {
        write_transaction(file, transaction_events)?;
        file.flush()?;
        file.sync_data()?;
    }
    Ok(())
}

fn transaction_slices(events: &[EventRecord]) -> AppResult<Vec<&[EventRecord]>> {
    let mut slices = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let transaction = events[start].transaction;
        let mut end = start + 1;
        while end < events.len() && events[end].transaction == transaction {
            end += 1;
        }
        slices.push(&events[start..end]);
        start = end;
    }
    if slices.is_empty() {
        return Err(cli_error("storage workload contains no transactions"));
    }
    Ok(slices)
}

fn validate_max_event_batches(events: &[EventRecord], limit: u32) -> AppResult<u64> {
    if limit == 0 {
        return Err(cli_error("streaming batch event limit must be positive"));
    }
    let mut observed_max = 0_usize;
    for batch in transaction_slices(events)? {
        if batch.len() > usize::try_from(limit)? {
            return Err(cli_error("streaming batch exceeds its event limit"));
        }
        observed_max = observed_max.max(batch.len());
    }
    Ok(u64::try_from(observed_max)?)
}

fn write_transaction(file: &mut File, events: &[EventRecord]) -> AppResult<()> {
    file.write_all(&encode_transaction(events)?)?;
    Ok(())
}

fn encode_transaction(events: &[EventRecord]) -> AppResult<Vec<u8>> {
    let first = events
        .first()
        .ok_or_else(|| cli_error("cannot write an empty transaction"))?;
    if events.len() > usize::try_from(MAX_EVENTS_PER_TRANSACTION)? {
        return Err(cli_error(
            "append-log transaction exceeds configured event maximum",
        ));
    }
    let mut body = Vec::new();
    body.extend_from_slice(&first.transaction.to_le_bytes());
    body.extend_from_slice(&first.sequence.to_le_bytes());
    body.extend_from_slice(&u32::try_from(events.len())?.to_le_bytes());
    for (expected_index, event) in events.iter().enumerate() {
        let expected_sequence = first
            .sequence
            .checked_add(u64::try_from(expected_index)?)
            .ok_or_else(|| cli_error("append-log event sequence overflow"))?;
        if event.transaction != first.transaction
            || event.index != u32::try_from(expected_index)?
            || event.sequence != expected_sequence
            || event.payload.len() > MAX_PAYLOAD_BYTES
        {
            return Err(cli_error("transaction events are not canonical"));
        }
        body.extend_from_slice(&event.sequence.to_le_bytes());
        body.extend_from_slice(&event.index.to_le_bytes());
        body.extend_from_slice(&u32::try_from(event.payload.len())?.to_le_bytes());
        body.extend_from_slice(&event.payload);
    }
    let frame_len = body
        .len()
        .checked_add(size_of::<u32>())
        .ok_or_else(|| cli_error("append-log frame length overflow"))?;
    if frame_len > MAX_FRAME_BYTES {
        return Err(cli_error("append-log frame exceeds configured maximum"));
    }
    let mut frame = Vec::with_capacity(
        TRANSACTION_MAGIC
            .len()
            .checked_add(size_of::<u32>())
            .and_then(|length| length.checked_add(frame_len))
            .ok_or_else(|| cli_error("append-log encoded frame length overflow"))?,
    );
    frame.extend_from_slice(TRANSACTION_MAGIC);
    frame.extend_from_slice(&u32::try_from(frame_len)?.to_le_bytes());
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&crc32c::crc32c(&body).to_le_bytes());
    Ok(frame)
}

#[derive(Debug, Eq, PartialEq)]
struct ReplayOutcome {
    events: Vec<EventRecord>,
    incomplete_tail: bool,
    format_version: u64,
}

fn replay_log(path: &Path) -> AppResult<ReplayOutcome> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; LOG_HEADER.len()];
    if read_exact_state(&mut file, &mut header)? != ReadState::Full {
        return Err(cli_error("append-log header is missing or corrupt"));
    }
    let format_version = if &header == LOG_HEADER {
        1
    } else if &header == LOG_HEADER_V2 {
        2
    } else {
        return Err(cli_error("append-log schema version is unsupported"));
    };

    let mut events = Vec::new();
    let mut expected_transaction = 1_u64;
    let mut expected_sequence = 1_u64;
    loop {
        let mut magic = [0_u8; TRANSACTION_MAGIC.len()];
        match read_exact_state(&mut file, &mut magic)? {
            ReadState::CleanEof => {
                return Ok(ReplayOutcome {
                    events,
                    incomplete_tail: false,
                    format_version,
                });
            }
            ReadState::Partial => {
                return Ok(ReplayOutcome {
                    events,
                    incomplete_tail: true,
                    format_version,
                });
            }
            ReadState::Full => {}
        }
        if &magic != TRANSACTION_MAGIC {
            return Err(cli_error("append-log transaction magic is corrupt"));
        }

        let mut frame_len_bytes = [0_u8; size_of::<u32>()];
        if read_exact_state(&mut file, &mut frame_len_bytes)? != ReadState::Full {
            return Ok(ReplayOutcome {
                events,
                incomplete_tail: true,
                format_version,
            });
        }
        let frame_len = usize::try_from(u32::from_le_bytes(frame_len_bytes))?;
        let minimum_frame_len = 2 * size_of::<u64>() + 2 * size_of::<u32>();
        if !(minimum_frame_len..=MAX_FRAME_BYTES).contains(&frame_len) {
            return Err(cli_error("append-log frame length is invalid"));
        }
        let mut frame = vec![0_u8; frame_len];
        if read_exact_state(&mut file, &mut frame)? != ReadState::Full {
            return Ok(ReplayOutcome {
                events,
                incomplete_tail: true,
                format_version,
            });
        }
        let checksum_offset = frame_len - size_of::<u32>();
        let (body, checksum_bytes) = frame.split_at(checksum_offset);
        let stored_checksum = u32::from_le_bytes(
            checksum_bytes
                .try_into()
                .map_err(|_| cli_error("append-log checksum framing is invalid"))?,
        );
        if crc32c::crc32c(body) != stored_checksum {
            return Err(cli_error("append-log transaction checksum is invalid"));
        }

        let decoded = decode_transaction(body, expected_transaction, expected_sequence)?;
        expected_transaction = expected_transaction
            .checked_add(1)
            .ok_or_else(|| cli_error("append-log transaction identity overflow"))?;
        expected_sequence = expected_sequence
            .checked_add(u64::try_from(decoded.len())?)
            .ok_or_else(|| cli_error("append-log event sequence overflow"))?;
        events.extend(decoded);
    }
}

fn decode_transaction(
    body: &[u8],
    expected_transaction: u64,
    expected_sequence: u64,
) -> AppResult<Vec<EventRecord>> {
    let mut cursor = FrameCursor::new(body);
    let transaction = cursor.u64()?;
    let first_sequence = cursor.u64()?;
    let event_count = cursor.u32()?;
    if transaction != expected_transaction
        || first_sequence != expected_sequence
        || event_count == 0
        || event_count > MAX_EVENTS_PER_TRANSACTION
    {
        return Err(cli_error("append-log transaction metadata is invalid"));
    }

    let mut events = Vec::with_capacity(usize::try_from(event_count)?);
    for index in 0..event_count {
        let sequence = cursor.u64()?;
        let stored_index = cursor.u32()?;
        let payload_len = usize::try_from(cursor.u32()?)?;
        let expected_event_sequence = expected_sequence
            .checked_add(u64::from(index))
            .ok_or_else(|| cli_error("append-log event sequence overflow"))?;
        if sequence != expected_event_sequence
            || stored_index != index
            || payload_len > MAX_PAYLOAD_BYTES
        {
            return Err(cli_error("append-log event metadata is invalid"));
        }
        events.push(EventRecord {
            sequence,
            transaction,
            index,
            payload: cursor.bytes(payload_len)?.to_vec(),
        });
    }
    if !cursor.is_finished() {
        return Err(cli_error("append-log transaction has trailing bytes"));
    }
    Ok(events)
}

struct FrameCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> FrameCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn u32(&mut self) -> AppResult<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(size_of::<u32>())?
                .try_into()
                .map_err(|_| cli_error("append-log u32 framing is invalid"))?,
        ))
    }

    fn u64(&mut self) -> AppResult<u64> {
        Ok(u64::from_le_bytes(
            self.bytes(size_of::<u64>())?
                .try_into()
                .map_err(|_| cli_error("append-log u64 framing is invalid"))?,
        ))
    }

    fn bytes(&mut self, length: usize) -> AppResult<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| cli_error("append-log frame cursor overflow"))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| cli_error("append-log transaction is truncated"))?;
        self.position = end;
        Ok(bytes)
    }

    fn is_finished(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadState {
    Full,
    CleanEof,
    Partial,
}

fn read_exact_state(reader: &mut File, buffer: &mut [u8]) -> io::Result<ReadState> {
    let mut read = 0;
    while read < buffer.len() {
        match reader.read(&mut buffer[read..])? {
            0 if read == 0 => return Ok(ReadState::CleanEof),
            0 => return Ok(ReadState::Partial),
            count => read += count,
        }
    }
    Ok(ReadState::Full)
}

fn validate_event_sequence(events: &[EventRecord]) -> AppResult<()> {
    let mut expected_sequence = 1_u64;
    let mut current_transaction = 0_u64;
    let mut expected_index = 0_u32;
    for event in events {
        if event.transaction != current_transaction {
            let next_transaction = current_transaction
                .checked_add(1)
                .ok_or_else(|| cli_error("replayed transaction identity overflow"))?;
            if event.transaction != next_transaction || event.index != 0 {
                return Err(cli_error("replayed transaction order is invalid"));
            }
            current_transaction = event.transaction;
            expected_index = 0;
        }
        if event.sequence != expected_sequence || event.index != expected_index {
            return Err(cli_error("replayed event order is invalid"));
        }
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| cli_error("replayed event sequence overflow"))?;
        expected_index = expected_index
            .checked_add(1)
            .ok_or_else(|| cli_error("replayed event index overflow"))?;
    }
    Ok(())
}

fn canonical_digest(events: &[EventRecord]) -> AppResult<String> {
    let mut hasher = Sha256::new();
    for event in events {
        hasher.update(event.sequence.to_le_bytes());
        hasher.update(event.transaction.to_le_bytes());
        hasher.update(event.index.to_le_bytes());
        hasher.update(u64::try_from(event.payload.len())?.to_le_bytes());
        hasher.update(&event.payload);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn elapsed_ns(started: Instant) -> AppResult<u64> {
    u64::try_from(started.elapsed().as_nanos())
        .map_err(|_| cli_error("storage benchmark duration exceeds u64 nanoseconds"))
}

fn directory_size(path: &Path) -> AppResult<u64> {
    let mut total = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total = total
                .checked_add(entry.metadata()?.len())
                .ok_or_else(|| cli_error("storage benchmark file size overflow"))?;
        }
    }
    Ok(total)
}

fn sync_directory(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn create_private_directory(path: &Path) -> AppResult<()> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> StorageFixture {
        let fixture: StorageFixture =
            serde_json::from_str(STORAGE_FIXTURE_JSON).expect("storage fixture JSON");
        validate_fixture(&fixture, StorageWorkload::CriticalAppendReplay)
            .expect("valid storage fixture");
        fixture
    }

    fn isolated_dir(label: &str) -> PathBuf {
        let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "greentyper-storage-test-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("isolated test directory");
        path
    }

    fn write_complete_log(path: &Path, transactions: &[&[EventRecord]]) -> Vec<u64> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create log");
        file.write_all(LOG_HEADER).expect("header");
        let mut ends = vec![u64::try_from(LOG_HEADER.len()).expect("header length")];
        for transaction in transactions {
            write_transaction(&mut file, transaction).expect("transaction");
            file.flush().expect("flush");
            ends.push(file.metadata().expect("metadata").len());
        }
        file.sync_all().expect("sync");
        ends
    }

    #[test]
    fn storage_candidates_replay_the_same_canonical_events() {
        let options = |implementation: &str| Options {
            comparison: "storage".into(),
            implementation: implementation.into(),
            workload: "critical-append-replay".into(),
            candidate_id: "test".into(),
            source_revision: "0123456".into(),
            output: "unused.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Redacted,
        };
        let mut sqlite = target_for(&options("sqlite-wal")).expect("SQLite target");
        let mut append = target_for(&options("append-log")).expect("append target");
        let (_, sqlite_observation) = execute_once(sqlite.as_mut()).expect("SQLite run");
        let (_, append_observation) = execute_once(append.as_mut()).expect("append run");
        assert_eq!(sqlite_observation.operation_units, 64);
        assert_eq!(
            sqlite_observation.output_digest,
            append_observation.output_digest
        );
        assert!(sqlite_observation.gauges["final_storage_bytes"] > 0);
        assert!(append_observation.gauges["final_storage_bytes"] > 0);
    }

    #[test]
    fn append_log_classifies_every_tail_cut_without_partial_transactions() {
        let fixture = fixture();
        let events = generate_events(&fixture).expect("events");
        let transactions = transaction_slices(&events).expect("transactions");
        let directory = isolated_dir("truncate");
        let complete_path = directory.join("complete.gtlog");
        let ends = write_complete_log(&complete_path, &transactions[..2]);
        let complete = fs::read(&complete_path).expect("complete bytes");
        let first_end = usize::try_from(ends[1]).expect("first transaction end");
        let full_end = usize::try_from(ends[2]).expect("second transaction end");
        let cut_path = directory.join("cut.gtlog");

        for cut in 0..=full_end {
            fs::write(&cut_path, &complete[..cut]).expect("cut log");
            if cut < LOG_HEADER.len() {
                assert!(replay_log(&cut_path).is_err());
                continue;
            }
            let replay = replay_log(&cut_path).expect("framed tail is recoverable");
            match cut {
                value if value == LOG_HEADER.len() => {
                    assert!(!replay.incomplete_tail);
                    assert!(replay.events.is_empty());
                }
                value if value < first_end => {
                    assert!(replay.incomplete_tail);
                    assert!(replay.events.is_empty());
                }
                value if value == first_end => {
                    assert!(!replay.incomplete_tail);
                    assert_eq!(replay.events, transactions[0]);
                }
                value if value < full_end => {
                    assert!(replay.incomplete_tail);
                    assert_eq!(replay.events, transactions[0]);
                }
                _ => {
                    assert!(!replay.incomplete_tail);
                    assert_eq!(replay.events, events[..8]);
                }
            }
        }

        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn append_log_rejects_a_checksum_mismatch() {
        let fixture = fixture();
        let events = generate_events(&fixture).expect("events");
        let transactions = transaction_slices(&events).expect("transactions");
        let directory = isolated_dir("checksum");
        let path = directory.join("corrupt.gtlog");
        write_complete_log(&path, &transactions[..1]);
        let mut bytes = fs::read(&path).expect("log bytes");
        let body_offset = LOG_HEADER.len() + TRANSACTION_MAGIC.len() + size_of::<u32>();
        bytes[body_offset] ^= 0x80;
        fs::write(&path, bytes).expect("corrupt log");
        assert!(replay_log(&path).is_err());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn append_log_rejects_an_unbounded_frame_before_allocation() {
        let directory = isolated_dir("frame-length");
        let path = directory.join("invalid.gtlog");
        let mut bytes = Vec::from(LOG_HEADER.as_slice());
        bytes.extend_from_slice(TRANSACTION_MAGIC);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, bytes).expect("invalid log");
        assert!(replay_log(&path).is_err());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn append_log_writer_enforces_reader_bounds() {
        let directory = isolated_dir("writer-bounds");
        let path = directory.join("bounded.gtlog");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("log");
        let oversized = EventRecord {
            sequence: 1,
            transaction: 1,
            index: 0,
            payload: vec![0; MAX_PAYLOAD_BYTES + 1],
        };
        assert!(write_transaction(&mut file, &[oversized]).is_err());
        drop(file);
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn storage_fixture_shape_is_frozen() {
        let mut fixture = fixture();
        fixture.events_per_transaction = 5;
        assert!(validate_fixture(&fixture, StorageWorkload::CriticalAppendReplay).is_err());
    }

    #[test]
    fn streaming_batch_limit_is_enforced_by_execution() {
        let fixture: StorageFixture =
            serde_json::from_str(STREAMING_FIXTURE_JSON).expect("streaming fixture JSON");
        let events = generate_events(&fixture).expect("events");
        assert_eq!(validate_max_event_batches(&events, 32).expect("valid"), 32);
        assert!(validate_max_event_batches(&events, 31).is_err());
        assert!(validate_max_event_batches(&events, 0).is_err());
    }

    #[test]
    fn sqlite_replay_rejects_unknown_and_mixed_schema_versions() {
        let fixture = fixture();
        let events = generate_events(&fixture).expect("events");
        let directory = isolated_dir("sqlite-schema");
        let path = directory.join("ledger.sqlite3");
        let mut connection = create_sqlite_store(&path).expect("SQLite store");
        append_sqlite_events(&mut connection, &events).expect("events");
        connection
            .execute_batch("PRAGMA user_version = 2;")
            .expect("mixed version");
        drop(connection);
        assert!(replay_sqlite(&path).is_err());

        let connection = Connection::open(&path).expect("SQLite reopen");
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .expect("unsupported version");
        drop(connection);
        assert!(replay_sqlite(&path).is_err());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn append_log_generation_selection_fails_closed_on_published_corruption() {
        let fixture = fixture();
        let events = generate_events(&fixture).expect("events");
        let directory = isolated_dir("published-corruption");
        let v1_path = directory.join("ledger-v1.gtlog");
        let mut v1 = create_append_log(&v1_path).expect("v1");
        append_log_events(&mut v1, &events).expect("events");
        drop(v1);
        fs::write(directory.join("ledger-v2.gtlog"), LOG_HEADER_V2).expect("empty v2");
        assert!(select_append_log_generation(&directory).is_err());
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn bounded_streaming_fixture_replays_all_batches() {
        let options = |implementation: &str| Options {
            comparison: "storage".into(),
            implementation: implementation.into(),
            workload: "bounded-streaming-replay".into(),
            candidate_id: "test".into(),
            source_revision: "0123456".into(),
            output: "unused.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Redacted,
        };
        let mut sqlite = target_for(&options("sqlite-wal")).expect("SQLite target");
        let mut append = target_for(&options("append-log")).expect("append target");
        let (_, sqlite_observation) = execute_once(sqlite.as_mut()).expect("SQLite run");
        let (_, append_observation) = execute_once(append.as_mut()).expect("append run");
        assert_eq!(sqlite_observation.operation_units, 256);
        assert_eq!(
            sqlite_observation.output_digest,
            append_observation.output_digest
        );
        assert_eq!(sqlite_observation.gauges["batch_event_limit"], 32);
        assert_eq!(append_observation.gauges["observed_max_batch_events"], 32);
    }

    #[test]
    fn cas_workload_has_one_winner_for_both_candidates() {
        let options = |implementation: &str| Options {
            comparison: "storage".into(),
            implementation: implementation.into(),
            workload: "cas-one-winner".into(),
            candidate_id: "test".into(),
            source_revision: "0123456".into(),
            output: "unused.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Redacted,
        };
        let mut sqlite = target_for(&options("sqlite-wal")).expect("SQLite target");
        let mut append = target_for(&options("append-log")).expect("append target");
        let (_, sqlite_observation) = execute_once(sqlite.as_mut()).expect("SQLite CAS");
        let (_, append_observation) = execute_once(append.as_mut()).expect("append CAS");
        assert_eq!(sqlite_observation.gauges["cas_winners"], 1);
        assert_eq!(append_observation.gauges["cas_winners"], 1);
        assert_eq!(sqlite_observation.gauges["cas_losers"], 7);
        assert_eq!(
            sqlite_observation.output_digest,
            append_observation.output_digest
        );
    }

    #[test]
    fn backup_restore_workload_replays_identical_events() {
        let options = |implementation: &str| Options {
            comparison: "storage".into(),
            implementation: implementation.into(),
            workload: "backup-restore".into(),
            candidate_id: "test".into(),
            source_revision: "0123456".into(),
            output: "unused.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Redacted,
        };
        let mut sqlite = target_for(&options("sqlite-wal")).expect("SQLite target");
        let mut append = target_for(&options("append-log")).expect("append target");
        let (_, sqlite_observation) = execute_once(sqlite.as_mut()).expect("SQLite backup");
        let (_, append_observation) = execute_once(append.as_mut()).expect("append backup");
        assert_eq!(sqlite_observation.operation_units, 64);
        assert_eq!(append_observation.operation_units, 64);
        assert_eq!(
            sqlite_observation.output_digest,
            append_observation.output_digest
        );
        assert!(sqlite_observation.gauges["backup_bytes"] > 0);
        assert!(append_observation.gauges["restored_bytes"] > 0);
    }

    #[test]
    fn interrupted_migration_recovers_only_complete_generations() {
        let options = |implementation: &str| Options {
            comparison: "storage".into(),
            implementation: implementation.into(),
            workload: "interrupted-migration".into(),
            candidate_id: "test".into(),
            source_revision: "0123456".into(),
            output: "unused.json".into(),
            runs: 1,
            warmup_runs: 1,
            expect_baseline: None,
            machine_identifiers: MachineIdentifierPolicy::Redacted,
        };
        let mut sqlite = target_for(&options("sqlite-wal")).expect("SQLite target");
        let mut append = target_for(&options("append-log")).expect("append target");
        let (_, sqlite_observation) = execute_once(sqlite.as_mut()).expect("SQLite migration");
        let (_, append_observation) = execute_once(append.as_mut()).expect("append migration");
        for observation in [&sqlite_observation, &append_observation] {
            assert_eq!(observation.operation_units, 3);
            assert_eq!(observation.gauges["old_generation_recoveries"], 2);
            assert_eq!(observation.gauges["new_generation_recoveries"], 1);
            assert_eq!(observation.gauges["final_schema_version"], 2);
        }
        assert_eq!(
            sqlite_observation.output_digest,
            append_observation.output_digest
        );
    }
}
