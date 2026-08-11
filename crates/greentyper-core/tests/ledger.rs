use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::ledger::{
    EventData, FileLedger, LedgerError, LedgerHead, MAX_EVENT_PAYLOAD_BYTES, crc32c,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-ledger-{name}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn event(kind: u16, payload: &[u8]) -> EventData {
    EventData {
        schema: 1,
        kind,
        payload: payload.to_vec(),
    }
}

#[test]
fn crc32c_matches_the_standard_check_value() {
    assert_eq!(crc32c(b"123456789"), 0xe306_9283);
}

#[test]
fn append_receipt_and_replay_are_transactional() {
    let path = temp_path("roundtrip");
    let (mut ledger, initial) = FileLedger::open(&path).expect("create ledger");
    assert_eq!(initial.head, LedgerHead::default());
    let receipt = ledger
        .append(LedgerHead::default(), &[event(1, b"one"), event(2, b"two")])
        .expect("durable append");
    assert_eq!(receipt.transaction, 1);
    assert_eq!(receipt.first_sequence, 1);
    assert_eq!(receipt.last_sequence, 2);
    drop(ledger);

    let (ledger, replay) = FileLedger::open(&path).expect("reopen ledger");
    assert_eq!(replay.head.transaction, 1);
    assert_eq!(replay.head.sequence, 2);
    assert_eq!(replay.events.len(), 2);
    assert_eq!(replay.events[1].data.payload, b"two");
    assert_eq!(replay.truncated_tail_bytes, 0);
    drop(ledger);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn stale_head_is_rejected_without_mutation() {
    let path = temp_path("cas");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    ledger
        .append(LedgerHead::default(), &[event(1, b"one")])
        .expect("first append");
    let before = ledger.events().to_vec();
    assert!(matches!(
        ledger.append(LedgerHead::default(), &[event(2, b"stale")]),
        Err(LedgerError::HeadConflict { .. })
    ));
    assert_eq!(ledger.events(), before);
    drop(ledger);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn partial_final_transaction_recovers_only_the_complete_prefix() {
    let path = temp_path("partial-tail");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    let first = ledger
        .append(LedgerHead::default(), &[event(1, b"one")])
        .expect("first append");
    let first_length = fs::metadata(&path).expect("ledger metadata").len();
    ledger
        .append(ledger.head(), &[event(2, b"two")])
        .expect("second append");
    drop(ledger);
    let full_length = fs::metadata(&path).expect("ledger metadata").len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open ledger for truncation")
        .set_len(full_length - 3)
        .expect("truncate commit marker");

    let (ledger, replay) = FileLedger::open(&path).expect("recover complete prefix");
    assert_eq!(replay.events.len(), 1);
    assert_eq!(replay.head.transaction, first.transaction);
    assert!(replay.truncated_tail_bytes > 0);
    assert_eq!(
        fs::metadata(&path).expect("ledger metadata").len(),
        first_length
    );
    assert_eq!(ledger.head(), replay.head);
    drop(ledger);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn checksum_tamper_fails_closed() {
    let path = temp_path("checksum");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    ledger
        .append(LedgerHead::default(), &[event(1, b"payload")])
        .expect("append");
    drop(ledger);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open ledger");
    file.seek(SeekFrom::Start(44)).expect("seek payload");
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).expect("read byte");
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(44)).expect("seek payload");
    file.write_all(&byte).expect("tamper byte");
    file.sync_all().expect("sync tamper");
    drop(file);

    assert!(matches!(
        FileLedger::open(&path),
        Err(LedgerError::Corrupt { .. })
    ));
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn frame_length_tamper_is_corruption_not_a_partial_tail() {
    let path = temp_path("length-tamper");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    ledger
        .append(LedgerHead::default(), &[event(1, b"payload")])
        .expect("append");
    drop(ledger);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open ledger");
    file.seek(SeekFrom::Start(20)).expect("seek frame length");
    let mut length = [0_u8; 4];
    file.read_exact(&mut length).expect("read frame length");
    let changed = u32::from_le_bytes(length) + 1;
    file.seek(SeekFrom::Start(20)).expect("seek frame length");
    file.write_all(&changed.to_le_bytes())
        .expect("tamper frame length");
    file.sync_all().expect("sync tamper");
    drop(file);

    assert!(matches!(
        FileLedger::open(&path),
        Err(LedgerError::Corrupt { .. })
    ));
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn unsupported_format_fails_explicitly() {
    let path = temp_path("unsupported");
    let (ledger, _) = FileLedger::open(&path).expect("create ledger");
    drop(ledger);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open ledger");
    let mut header = [0_u8; 16];
    file.read_exact(&mut header).expect("read header");
    header[8..10].copy_from_slice(&2_u16.to_le_bytes());
    let checksum = crc32c(&header[..12]);
    header[12..16].copy_from_slice(&checksum.to_le_bytes());
    file.seek(SeekFrom::Start(0)).expect("seek header");
    file.write_all(&header).expect("write header");
    file.sync_all().expect("sync header");
    drop(file);

    assert!(matches!(
        FileLedger::open(&path),
        Err(LedgerError::UnsupportedFormat {
            supported: 1,
            actual: 2
        })
    ));
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn payload_bounds_are_checked_before_writing() {
    let path = temp_path("bounds");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    let oversized = EventData {
        schema: 1,
        kind: 1,
        payload: vec![0; MAX_EVENT_PAYLOAD_BYTES + 1],
    };
    assert!(matches!(
        ledger.append(LedgerHead::default(), &[oversized]),
        Err(LedgerError::PayloadTooLarge)
    ));
    assert_eq!(ledger.head(), LedgerHead::default());
    drop(ledger);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn a_second_writer_is_rejected_until_the_owner_closes() {
    let path = temp_path("lock");
    let (ledger, _) = FileLedger::open(&path).expect("create ledger");
    assert!(matches!(FileLedger::open(&path), Err(LedgerError::Locked)));
    drop(ledger);
    let (reopened, _) = FileLedger::open(&path).expect("reopen after owner closes");
    drop(reopened);
    fs::remove_file(path).expect("cleanup ledger");
}

#[test]
fn inspection_reports_but_does_not_repair_a_partial_tail() {
    let path = temp_path("inspect-tail");
    let (mut ledger, _) = FileLedger::open(&path).expect("create ledger");
    ledger
        .append(LedgerHead::default(), &[event(1, b"one")])
        .expect("first append");
    ledger
        .append(ledger.head(), &[event(2, b"two")])
        .expect("second append");
    drop(ledger);
    let full_length = fs::metadata(&path).expect("ledger metadata").len();
    let truncated_length = full_length - 2;
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open ledger for truncation")
        .set_len(truncated_length)
        .expect("truncate ledger");

    let report = FileLedger::inspect(&path).expect("inspect ledger");
    assert!(report.truncated_tail_bytes > 0);
    assert_eq!(
        fs::metadata(&path).expect("ledger metadata").len(),
        truncated_length
    );
    assert!(matches!(
        FileLedger::open_existing_strict(&path),
        Err(LedgerError::IncompleteTail { bytes }) if bytes > 0
    ));
    assert_eq!(
        fs::metadata(&path).expect("ledger metadata").len(),
        truncated_length
    );
    let (repaired, _) = FileLedger::open(&path).expect("repair through writer open");
    drop(repaired);
    assert!(fs::metadata(&path).expect("ledger metadata").len() < truncated_length);
    fs::remove_file(path).expect("cleanup ledger");
}

#[cfg(unix)]
#[test]
fn symbolic_link_ledger_paths_are_rejected() {
    use std::os::unix::fs::symlink;

    let target = temp_path("symlink-target");
    let link = temp_path("symlink-link");
    fs::write(&target, b"do not overwrite").expect("write target");
    symlink(&target, &link).expect("create symlink");
    assert!(matches!(
        FileLedger::open(&link),
        Err(LedgerError::SymlinkPath)
    ));
    assert_eq!(fs::read(&target).expect("read target"), b"do not overwrite");
    fs::remove_file(link).expect("cleanup symlink");
    fs::remove_file(target).expect("cleanup target");
}
