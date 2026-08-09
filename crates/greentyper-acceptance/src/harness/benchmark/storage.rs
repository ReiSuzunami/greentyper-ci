use super::*;
use rusqlite::{Connection, TransactionBehavior, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const STORAGE_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/storage/v1/critical-append-replay.json"
));
const LOG_HEADER: &[u8; 8] = b"GTLG\x01\0\0\0";
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
        "workloads": [{"id": "critical-append-replay", "version": 1}],
        "purpose": "candidate evidence; not a storage selection"
    })
}

pub(super) fn target(implementation: &str) -> AppResult<Box<dyn BenchmarkTarget>> {
    let fixture: StorageFixture = serde_json::from_str(STORAGE_FIXTURE_JSON)?;
    validate_fixture(&fixture)?;
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
        fixture,
        events,
        run_dir: None,
    }))
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
}

fn validate_fixture(fixture: &StorageFixture) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    if fixture.comparison_id != "storage"
        || fixture.workload_id != "critical-append-replay"
        || fixture.workload_version != 1
        || fixture.transactions != 16
        || fixture.events_per_transaction != 4
        || fixture.payload_bytes != 256
    {
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
    fixture: StorageFixture,
    events: Vec<EventRecord>,
    run_dir: Option<PathBuf>,
}

impl BenchmarkTarget for StorageTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "storage",
            comparison_version: 1,
            implementation: self.engine.implementation(),
            implementation_revision: "1",
            dependencies: self.engine.dependencies(),
            workload_id: "critical-append-replay",
            workload_version: self.fixture.workload_version,
            input_shape: "16 sync transactions x 4 events x 256 payload bytes",
            unit: "events synchronously committed and replayed",
            boundary: "create store, append 16 synchronous transactions, close, reopen, replay, and verify",
            process_mode: "in-process",
            fixture_bytes: STORAGE_FIXTURE_JSON.as_bytes(),
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
        fs::create_dir(&path)?;
        self.run_dir = Some(path);
        Ok(())
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        let run_dir = self
            .run_dir
            .as_deref()
            .ok_or_else(|| cli_error("storage benchmark run directory was not prepared"))?;
        let observation = match self.engine {
            StorageEngine::SqliteWal => run_sqlite(run_dir, &self.events)?,
            StorageEngine::AppendLog => run_append_log(run_dir, &self.events)?,
        };
        if observation.replayed != self.events {
            return Err(cli_error(
                "storage benchmark replay differs from canonical events",
            ));
        }
        Ok(BenchmarkObservation {
            operation_units: u64::try_from(observation.replayed.len())?,
            output_digest: canonical_digest(&observation.replayed)?,
            timings_ns: BTreeMap::from([
                ("append".into(), observation.append_ns),
                ("replay".into(), observation.replay_ns),
                ("setup".into(), observation.setup_ns),
            ]),
            gauges: BTreeMap::from([
                (
                    "final_storage_bytes".into(),
                    observation.final_storage_bytes,
                ),
                (
                    "post_append_storage_bytes".into(),
                    observation.post_append_storage_bytes,
                ),
            ]),
        })
    }

    fn cleanup_run(&mut self) -> AppResult<()> {
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

fn run_sqlite(run_dir: &Path, events: &[EventRecord]) -> AppResult<StorageObservation> {
    let database_path = run_dir.join("ledger.sqlite3");
    let setup_started = Instant::now();
    let mut connection = Connection::open(&database_path)?;
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
         );",
    )?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if synchronous != 2 {
        return Err(cli_error(format!(
            "SQLite synchronous mode is {synchronous}; expected FULL (2)"
        )));
    }
    let setup_ns = elapsed_ns(setup_started)?;

    let append_started = Instant::now();
    for transaction_events in transaction_slices(events)? {
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
        transaction.commit()?;
    }
    let append_ns = elapsed_ns(append_started)?;
    let post_append_storage_bytes = directory_size(run_dir)?;
    drop(connection);

    let replay_started = Instant::now();
    let replay_connection = Connection::open(&database_path)?;
    let replayed = {
        let mut statement = replay_connection.prepare(
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
        let mut replayed = Vec::with_capacity(events.len());
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
    drop(replay_connection);
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

fn run_append_log(run_dir: &Path, events: &[EventRecord]) -> AppResult<StorageObservation> {
    let log_path = run_dir.join("ledger.gtlog");
    let setup_started = Instant::now();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)?;
    file.write_all(LOG_HEADER)?;
    file.flush()?;
    file.sync_all()?;
    sync_directory(run_dir)?;
    let setup_ns = elapsed_ns(setup_started)?;

    let append_started = Instant::now();
    for transaction_events in transaction_slices(events)? {
        write_transaction(&mut file, transaction_events)?;
        file.flush()?;
        file.sync_data()?;
    }
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

fn write_transaction(file: &mut File, events: &[EventRecord]) -> AppResult<()> {
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
    file.write_all(TRANSACTION_MAGIC)?;
    file.write_all(&u32::try_from(frame_len)?.to_le_bytes())?;
    file.write_all(&body)?;
    file.write_all(&crc32c::crc32c(&body).to_le_bytes())?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct ReplayOutcome {
    events: Vec<EventRecord>,
    incomplete_tail: bool,
}

fn replay_log(path: &Path) -> AppResult<ReplayOutcome> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; LOG_HEADER.len()];
    if read_exact_state(&mut file, &mut header)? != ReadState::Full || &header != LOG_HEADER {
        return Err(cli_error("append-log header is missing or corrupt"));
    }

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
                });
            }
            ReadState::Partial => {
                return Ok(ReplayOutcome {
                    events,
                    incomplete_tail: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> StorageFixture {
        let fixture: StorageFixture =
            serde_json::from_str(STORAGE_FIXTURE_JSON).expect("storage fixture JSON");
        validate_fixture(&fixture).expect("valid storage fixture");
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
        assert!(validate_fixture(&fixture).is_err());
    }
}
