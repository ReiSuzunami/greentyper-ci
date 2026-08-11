//! Synchronous append-only Event Ledger with complete-transaction replay.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::Path;

use crate::schema::SchemaKind;

const HEADER_MAGIC: &[u8; 8] = b"GTLEDGER";
const HEADER_BYTES: u64 = 16;
const FORMAT_VERSION: u16 = SchemaKind::LedgerFormat.current().get();
const TRANSACTION_MAGIC: &[u8; 4] = b"GTXN";
const COMMIT_MAGIC: &[u8; 4] = b"CMIT";

pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_EVENTS_PER_TRANSACTION: usize = 4096;
pub const MAX_FRAME_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REPLAY_EVENTS: usize = 1_000_000;
pub const MAX_REPLAY_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LedgerHead {
    pub transaction: u64,
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventData {
    pub schema: u16,
    pub kind: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredEvent {
    pub sequence: u64,
    pub transaction: u64,
    pub index_in_transaction: u32,
    pub events_in_transaction: u32,
    pub data: EventData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityReceipt {
    pub transaction: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub event_count: u32,
    pub transaction_crc32c: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayReport {
    pub head: LedgerHead,
    pub events: Vec<StoredEvent>,
    pub truncated_tail_bytes: u64,
}

struct LockedFile(File);

impl Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub struct FileLedger {
    file: LockedFile,
    head: LedgerHead,
    events: Vec<StoredEvent>,
    payload_bytes: usize,
    poisoned: bool,
}

impl FileLedger {
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, ReplayReport), LedgerError> {
        Self::open_with_policy(path.as_ref(), true, true)
    }

    /// Opens an existing complete Ledger for mutation without creating or
    /// repairing it. The exclusive lock remains held after validation.
    pub fn open_existing_strict(
        path: impl AsRef<Path>,
    ) -> Result<(Self, ReplayReport), LedgerError> {
        Self::open_with_policy(path.as_ref(), false, false)
    }

    fn open_with_policy(
        path: &Path,
        create: bool,
        repair_tail: bool,
    ) -> Result<(Self, ReplayReport), LedgerError> {
        validate_path(path, create)?;
        let mut options = OpenOptions::new();
        options
            .create(create)
            .read(true)
            .write(true)
            .truncate(false);
        configure_no_follow(&mut options);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(LedgerError::Io)?;
        file.try_lock().map_err(map_lock_error)?;
        let mut file = LockedFile(file);
        ensure_regular_file(&file)?;
        tighten_private_permissions(&file)?;
        let length = file.metadata().map_err(LedgerError::Io)?.len();
        if length == 0 && create {
            write_header(&mut file)?;
        } else {
            validate_header(&mut file, length)?;
        }

        let ScanResult {
            head,
            events,
            valid_bytes,
            payload_bytes,
        } = scan_transactions(&mut file)?;
        let length = file.metadata().map_err(LedgerError::Io)?.len();
        let truncated_tail_bytes = length
            .checked_sub(valid_bytes)
            .ok_or(LedgerError::IntegerOverflow)?;
        if truncated_tail_bytes > 0 {
            if !repair_tail {
                return Err(LedgerError::IncompleteTail {
                    bytes: truncated_tail_bytes,
                });
            }
            file.set_len(valid_bytes).map_err(LedgerError::Io)?;
            file.sync_all().map_err(LedgerError::Io)?;
        }
        file.seek(SeekFrom::End(0)).map_err(LedgerError::Io)?;

        let report = ReplayReport {
            head,
            events: events.clone(),
            truncated_tail_bytes,
        };
        Ok((
            Self {
                file,
                head,
                events,
                payload_bytes,
                poisoned: false,
            },
            report,
        ))
    }

    pub fn inspect(path: impl AsRef<Path>) -> Result<ReplayReport, LedgerError> {
        let path = path.as_ref();
        validate_path(path, false)?;
        let mut options = OpenOptions::new();
        options.read(true);
        configure_no_follow(&mut options);
        let file = options.open(path).map_err(LedgerError::Io)?;
        file.try_lock_shared().map_err(map_lock_error)?;
        let mut file = LockedFile(file);
        ensure_regular_file(&file)?;
        let length = file.metadata().map_err(LedgerError::Io)?.len();
        validate_header(&mut file, length)?;
        let ScanResult {
            head,
            events,
            valid_bytes,
            payload_bytes: _,
        } = scan_transactions(&mut file)?;
        Ok(ReplayReport {
            head,
            events,
            truncated_tail_bytes: length
                .checked_sub(valid_bytes)
                .ok_or(LedgerError::IntegerOverflow)?,
        })
    }

    #[must_use]
    pub const fn head(&self) -> LedgerHead {
        self.head
    }

    #[must_use]
    pub fn events(&self) -> &[StoredEvent] {
        &self.events
    }

    pub fn append(
        &mut self,
        expected: LedgerHead,
        events: &[EventData],
    ) -> Result<DurabilityReceipt, LedgerError> {
        self.append_with_io(expected, events, write_frame_synchronously)
    }

    fn append_with_io<F>(
        &mut self,
        expected: LedgerHead,
        events: &[EventData],
        write_frame: F,
    ) -> Result<DurabilityReceipt, LedgerError>
    where
        F: FnOnce(&mut File, &[u8]) -> io::Result<()>,
    {
        if self.poisoned {
            return Err(LedgerError::WriterPoisoned);
        }
        if expected != self.head {
            return Err(LedgerError::HeadConflict {
                expected,
                actual: self.head,
            });
        }
        validate_events(events)?;
        let replayed_events = self
            .events
            .len()
            .checked_add(events.len())
            .ok_or(LedgerError::IntegerOverflow)?;
        if replayed_events > MAX_REPLAY_EVENTS {
            return Err(LedgerError::ReplayLimitExceeded);
        }
        let transaction_payload_bytes = events.iter().try_fold(0_usize, |total, event| {
            total
                .checked_add(event.payload.len())
                .ok_or(LedgerError::IntegerOverflow)
        })?;
        let payload_bytes =
            checked_replay_payload_bytes(self.payload_bytes, transaction_payload_bytes)?;

        let transaction = self
            .head
            .transaction
            .checked_add(1)
            .ok_or(LedgerError::IntegerOverflow)?;
        let first_sequence = self
            .head
            .sequence
            .checked_add(1)
            .ok_or(LedgerError::IntegerOverflow)?;
        let (frame, stored, receipt) = encode_transaction(transaction, first_sequence, events)?;

        let write_result = write_frame(&mut self.file, &frame);
        if let Err(source) = write_result {
            self.poisoned = true;
            return Err(LedgerError::DurabilityAmbiguous(source));
        }

        self.head = LedgerHead {
            transaction,
            sequence: receipt.last_sequence,
        };
        self.events.extend(stored);
        self.payload_bytes = payload_bytes;
        Ok(receipt)
    }

    #[cfg(test)]
    pub(crate) fn append_with_test_io<F>(
        &mut self,
        expected: LedgerHead,
        events: &[EventData],
        write_frame: F,
    ) -> Result<DurabilityReceipt, LedgerError>
    where
        F: FnOnce(&mut File, &[u8]) -> io::Result<()>,
    {
        self.append_with_io(expected, events, write_frame)
    }
}

fn write_frame_synchronously(file: &mut File, frame: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::End(0))?;
    file.write_all(frame)?;
    file.flush()?;
    file.sync_data()
}

fn configure_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn validate_path(path: &Path, allow_missing: bool) -> Result<(), LedgerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(LedgerError::SymlinkPath);
            }
            if !metadata.is_file() {
                return Err(LedgerError::NotRegularFile);
            }
            Ok(())
        }
        Err(source) if allow_missing && source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LedgerError::Io(source)),
    }
}

fn ensure_regular_file(file: &File) -> Result<(), LedgerError> {
    if file.metadata().map_err(LedgerError::Io)?.is_file() {
        Ok(())
    } else {
        Err(LedgerError::NotRegularFile)
    }
}

fn map_lock_error(error: TryLockError) -> LedgerError {
    match error {
        TryLockError::WouldBlock => LedgerError::Locked,
        TryLockError::Error(source) => LedgerError::Io(source),
    }
}

#[cfg(unix)]
fn tighten_private_permissions(file: &File) -> Result<(), LedgerError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file.metadata().map_err(LedgerError::Io)?;
    if metadata.mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(LedgerError::Io)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_private_permissions(_file: &File) -> Result<(), LedgerError> {
    Ok(())
}

fn write_header(file: &mut File) -> Result<(), LedgerError> {
    let mut header = Vec::with_capacity(HEADER_BYTES as usize);
    header.extend_from_slice(HEADER_MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&0_u16.to_le_bytes());
    header.extend_from_slice(&crc32c(&header).to_le_bytes());
    file.seek(SeekFrom::Start(0)).map_err(LedgerError::Io)?;
    file.write_all(&header).map_err(LedgerError::Io)?;
    file.flush().map_err(LedgerError::Io)?;
    file.sync_all().map_err(LedgerError::Io)
}

fn validate_header(file: &mut File, length: u64) -> Result<(), LedgerError> {
    if length < HEADER_BYTES {
        return Err(LedgerError::Corrupt {
            offset: 0,
            reason: "truncated ledger header",
        });
    }
    let mut header = [0_u8; HEADER_BYTES as usize];
    file.seek(SeekFrom::Start(0)).map_err(LedgerError::Io)?;
    file.read_exact(&mut header).map_err(LedgerError::Io)?;
    if &header[..8] != HEADER_MAGIC {
        return Err(LedgerError::Corrupt {
            offset: 0,
            reason: "invalid ledger magic",
        });
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version != FORMAT_VERSION {
        return Err(LedgerError::UnsupportedFormat {
            supported: FORMAT_VERSION,
            actual: version,
        });
    }
    if header[10..12] != [0, 0] {
        return Err(LedgerError::Corrupt {
            offset: 10,
            reason: "reserved ledger header bits are set",
        });
    }
    let expected = u32::from_le_bytes(header[12..16].try_into().expect("fixed header slice"));
    if crc32c(&header[..12]) != expected {
        return Err(LedgerError::Corrupt {
            offset: 12,
            reason: "ledger header checksum mismatch",
        });
    }
    Ok(())
}

struct ScanResult {
    head: LedgerHead,
    events: Vec<StoredEvent>,
    valid_bytes: u64,
    payload_bytes: usize,
}

fn scan_transactions(file: &mut File) -> Result<ScanResult, LedgerError> {
    let length = file.metadata().map_err(LedgerError::Io)?.len();
    let mut offset = HEADER_BYTES;
    let mut head = LedgerHead::default();
    let mut events = Vec::new();
    let mut payload_bytes = 0;
    file.seek(SeekFrom::Start(offset))
        .map_err(LedgerError::Io)?;

    while offset < length {
        let remaining = length
            .checked_sub(offset)
            .ok_or(LedgerError::IntegerOverflow)?;
        if remaining < 4 {
            break;
        }
        let mut magic = [0_u8; 4];
        file.read_exact(&mut magic).map_err(LedgerError::Io)?;
        if &magic != TRANSACTION_MAGIC {
            return Err(LedgerError::Corrupt {
                offset,
                reason: "invalid transaction magic",
            });
        }
        if remaining < 12 {
            break;
        }
        let mut length_bytes = [0_u8; 4];
        file.read_exact(&mut length_bytes)
            .map_err(LedgerError::Io)?;
        let body_length = u32::from_le_bytes(length_bytes) as usize;
        let mut complement_bytes = [0_u8; 4];
        file.read_exact(&mut complement_bytes)
            .map_err(LedgerError::Io)?;
        let complement = u32::from_le_bytes(complement_bytes);
        if complement != !u32::from_le_bytes(length_bytes) {
            return Err(LedgerError::Corrupt {
                offset: offset + 8,
                reason: "transaction frame length check mismatch",
            });
        }
        if body_length == 0 || body_length > MAX_FRAME_BODY_BYTES {
            return Err(LedgerError::Corrupt {
                offset: offset + 4,
                reason: "transaction frame length is out of bounds",
            });
        }
        let frame_bytes = 4_u64
            .checked_add(4)
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(body_length as u64))
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(4))
            .ok_or(LedgerError::IntegerOverflow)?;
        if remaining < frame_bytes {
            break;
        }

        let mut body = vec![0_u8; body_length];
        file.read_exact(&mut body).map_err(LedgerError::Io)?;
        let mut checksum_bytes = [0_u8; 4];
        file.read_exact(&mut checksum_bytes)
            .map_err(LedgerError::Io)?;
        let expected_checksum = u32::from_le_bytes(checksum_bytes);
        if crc32c(&body) != expected_checksum {
            return Err(LedgerError::Corrupt {
                offset: offset + 12 + body_length as u64,
                reason: "transaction checksum mismatch",
            });
        }
        let mut commit = [0_u8; 4];
        file.read_exact(&mut commit).map_err(LedgerError::Io)?;
        if &commit != COMMIT_MAGIC {
            return Err(LedgerError::Corrupt {
                offset: offset + frame_bytes - 4,
                reason: "transaction commit marker is invalid",
            });
        }

        let decoded = decode_transaction(&body, head, events.len(), payload_bytes)?;
        head = decoded.head;
        payload_bytes = decoded.payload_bytes;
        events.extend(decoded.events);
        offset = offset
            .checked_add(frame_bytes)
            .ok_or(LedgerError::IntegerOverflow)?;
    }

    Ok(ScanResult {
        head,
        events,
        valid_bytes: offset,
        payload_bytes,
    })
}

fn checked_replay_payload_bytes(current: usize, additional: usize) -> Result<usize, LedgerError> {
    let total = current
        .checked_add(additional)
        .ok_or(LedgerError::IntegerOverflow)?;
    if total > MAX_REPLAY_PAYLOAD_BYTES {
        Err(LedgerError::ReplayPayloadLimitExceeded)
    } else {
        Ok(total)
    }
}

fn validate_events(events: &[EventData]) -> Result<(), LedgerError> {
    if events.is_empty() {
        return Err(LedgerError::EmptyTransaction);
    }
    if events.len() > MAX_EVENTS_PER_TRANSACTION {
        return Err(LedgerError::TooManyEvents);
    }
    for event in events {
        if event.schema == 0 {
            return Err(LedgerError::ZeroEventSchema);
        }
        if event.kind == 0 {
            return Err(LedgerError::ZeroEventKind);
        }
        if event.payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(LedgerError::PayloadTooLarge);
        }
    }
    Ok(())
}

fn encode_transaction(
    transaction: u64,
    first_sequence: u64,
    events: &[EventData],
) -> Result<(Vec<u8>, Vec<StoredEvent>, DurabilityReceipt), LedgerError> {
    let count = u32::try_from(events.len()).map_err(|_| LedgerError::TooManyEvents)?;
    let mut body = Vec::new();
    body.extend_from_slice(&transaction.to_le_bytes());
    body.extend_from_slice(&first_sequence.to_le_bytes());
    body.extend_from_slice(&count.to_le_bytes());

    let mut stored = Vec::with_capacity(events.len());
    for (index, event) in events.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| LedgerError::IntegerOverflow)?;
        let sequence = first_sequence
            .checked_add(u64::from(index))
            .ok_or(LedgerError::IntegerOverflow)?;
        let payload_length =
            u32::try_from(event.payload.len()).map_err(|_| LedgerError::PayloadTooLarge)?;
        body.extend_from_slice(&sequence.to_le_bytes());
        body.extend_from_slice(&index.to_le_bytes());
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&event.schema.to_le_bytes());
        body.extend_from_slice(&event.kind.to_le_bytes());
        body.extend_from_slice(&payload_length.to_le_bytes());
        body.extend_from_slice(&event.payload);
        stored.push(StoredEvent {
            sequence,
            transaction,
            index_in_transaction: index,
            events_in_transaction: count,
            data: event.clone(),
        });
    }
    if body.len() > MAX_FRAME_BODY_BYTES {
        return Err(LedgerError::FrameTooLarge);
    }
    let body_length = u32::try_from(body.len()).map_err(|_| LedgerError::FrameTooLarge)?;
    let checksum = crc32c(&body);
    let mut frame = Vec::with_capacity(body.len() + 20);
    frame.extend_from_slice(TRANSACTION_MAGIC);
    frame.extend_from_slice(&body_length.to_le_bytes());
    frame.extend_from_slice(&(!body_length).to_le_bytes());
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&checksum.to_le_bytes());
    frame.extend_from_slice(COMMIT_MAGIC);
    let last_sequence = first_sequence
        .checked_add(u64::from(count) - 1)
        .ok_or(LedgerError::IntegerOverflow)?;
    Ok((
        frame,
        stored,
        DurabilityReceipt {
            transaction,
            first_sequence,
            last_sequence,
            event_count: count,
            transaction_crc32c: checksum,
        },
    ))
}

struct DecodedTransaction {
    head: LedgerHead,
    events: Vec<StoredEvent>,
    payload_bytes: usize,
}

fn decode_transaction(
    body: &[u8],
    previous: LedgerHead,
    replayed_events: usize,
    replayed_payload_bytes: usize,
) -> Result<DecodedTransaction, LedgerError> {
    let mut cursor = BodyCursor::new(body);
    let transaction = cursor.u64("transaction id")?;
    let first_sequence = cursor.u64("first sequence")?;
    let count = cursor.u32("event count")?;
    if count == 0 || count as usize > MAX_EVENTS_PER_TRANSACTION {
        return Err(LedgerError::Corrupt {
            offset: 0,
            reason: "transaction event count is out of bounds",
        });
    }
    if transaction
        != previous
            .transaction
            .checked_add(1)
            .ok_or(LedgerError::IntegerOverflow)?
    {
        return Err(LedgerError::Corrupt {
            offset: 0,
            reason: "transaction id is not contiguous",
        });
    }
    if first_sequence
        != previous
            .sequence
            .checked_add(1)
            .ok_or(LedgerError::IntegerOverflow)?
    {
        return Err(LedgerError::Corrupt {
            offset: 8,
            reason: "event sequence is not contiguous",
        });
    }
    let total_events = replayed_events
        .checked_add(count as usize)
        .ok_or(LedgerError::IntegerOverflow)?;
    if total_events > MAX_REPLAY_EVENTS {
        return Err(LedgerError::ReplayLimitExceeded);
    }

    let mut events = Vec::with_capacity(count as usize);
    let mut payload_bytes = replayed_payload_bytes;
    for expected_index in 0..count {
        let sequence = cursor.u64("event sequence")?;
        let index = cursor.u32("event index")?;
        let total = cursor.u32("event total")?;
        let schema = cursor.u16("event schema")?;
        let kind = cursor.u16("event kind")?;
        let payload_length = cursor.u32("event payload length")? as usize;
        let expected_sequence = first_sequence
            .checked_add(u64::from(expected_index))
            .ok_or(LedgerError::IntegerOverflow)?;
        if sequence != expected_sequence || index != expected_index || total != count {
            return Err(LedgerError::Corrupt {
                offset: cursor.position() as u64,
                reason: "event transaction metadata is inconsistent",
            });
        }
        if schema == 0 || kind == 0 {
            return Err(LedgerError::Corrupt {
                offset: cursor.position() as u64,
                reason: "event schema and kind must be nonzero",
            });
        }
        if payload_length > MAX_EVENT_PAYLOAD_BYTES {
            return Err(LedgerError::Corrupt {
                offset: cursor.position() as u64,
                reason: "event payload is too large",
            });
        }
        payload_bytes = checked_replay_payload_bytes(payload_bytes, payload_length)?;
        let payload = cursor.bytes(payload_length, "event payload")?.to_vec();
        events.push(StoredEvent {
            sequence,
            transaction,
            index_in_transaction: index,
            events_in_transaction: total,
            data: EventData {
                schema,
                kind,
                payload,
            },
        });
    }
    if !cursor.is_empty() {
        return Err(LedgerError::Corrupt {
            offset: cursor.position() as u64,
            reason: "transaction has trailing bytes",
        });
    }
    let last_sequence = first_sequence
        .checked_add(u64::from(count) - 1)
        .ok_or(LedgerError::IntegerOverflow)?;
    Ok(DecodedTransaction {
        head: LedgerHead {
            transaction,
            sequence: last_sequence,
        },
        events,
        payload_bytes,
    })
}

struct BodyCursor<'a> {
    body: &'a [u8],
    position: usize,
}

impl<'a> BodyCursor<'a> {
    const fn new(body: &'a [u8]) -> Self {
        Self { body, position: 0 }
    }

    const fn position(&self) -> usize {
        self.position
    }

    fn bytes(&mut self, length: usize, field: &'static str) -> Result<&'a [u8], LedgerError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(LedgerError::IntegerOverflow)?;
        let bytes = self
            .body
            .get(self.position..end)
            .ok_or(LedgerError::Corrupt {
                offset: self.position as u64,
                reason: field,
            })?;
        self.position = end;
        Ok(bytes)
    }

    fn u16(&mut self, field: &'static str) -> Result<u16, LedgerError> {
        Ok(u16::from_le_bytes(
            self.bytes(2, field)?
                .try_into()
                .expect("fixed integer slice"),
        ))
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, LedgerError> {
        Ok(u32::from_le_bytes(
            self.bytes(4, field)?
                .try_into()
                .expect("fixed integer slice"),
        ))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, LedgerError> {
        Ok(u64::from_le_bytes(
            self.bytes(8, field)?
                .try_into()
                .expect("fixed integer slice"),
        ))
    }

    const fn is_empty(&self) -> bool {
        self.position == self.body.len()
    }
}

#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    !crc
}

#[derive(Debug)]
pub enum LedgerError {
    Io(io::Error),
    DurabilityAmbiguous(io::Error),
    HeadConflict {
        expected: LedgerHead,
        actual: LedgerHead,
    },
    UnsupportedFormat {
        supported: u16,
        actual: u16,
    },
    Corrupt {
        offset: u64,
        reason: &'static str,
    },
    EmptyTransaction,
    TooManyEvents,
    PayloadTooLarge,
    FrameTooLarge,
    ZeroEventSchema,
    ZeroEventKind,
    ReplayLimitExceeded,
    ReplayPayloadLimitExceeded,
    IncompleteTail {
        bytes: u64,
    },
    IntegerOverflow,
    WriterPoisoned,
    Locked,
    SymlinkPath,
    NotRegularFile,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "ledger I/O failed: {source}"),
            Self::DurabilityAmbiguous(source) => {
                write!(formatter, "ledger durability is ambiguous: {source}")
            }
            Self::HeadConflict { expected, actual } => write!(
                formatter,
                "ledger head conflict: expected {expected:?}, actual {actual:?}"
            ),
            Self::UnsupportedFormat { supported, actual } => write!(
                formatter,
                "unsupported ledger format {actual}; expected {supported}"
            ),
            Self::Corrupt { offset, reason } => {
                write!(formatter, "corrupt ledger at byte {offset}: {reason}")
            }
            Self::EmptyTransaction => write!(formatter, "ledger transaction cannot be empty"),
            Self::TooManyEvents => write!(formatter, "ledger transaction has too many events"),
            Self::PayloadTooLarge => write!(formatter, "ledger event payload is too large"),
            Self::FrameTooLarge => write!(formatter, "ledger transaction frame is too large"),
            Self::ZeroEventSchema => write!(formatter, "ledger event schema zero is reserved"),
            Self::ZeroEventKind => write!(formatter, "ledger event kind zero is reserved"),
            Self::ReplayLimitExceeded => write!(formatter, "ledger replay event limit exceeded"),
            Self::ReplayPayloadLimitExceeded => {
                write!(formatter, "ledger replay payload limit exceeded")
            }
            Self::IncompleteTail { bytes } => {
                write!(formatter, "ledger has an incomplete tail of {bytes} bytes")
            }
            Self::IntegerOverflow => write!(formatter, "ledger integer overflow"),
            Self::WriterPoisoned => write!(formatter, "ledger writer requires recovery"),
            Self::Locked => write!(formatter, "ledger is locked by another process"),
            Self::SymlinkPath => write!(formatter, "ledger path cannot be a symbolic link"),
            Self::NotRegularFile => write!(formatter, "ledger path is not a regular file"),
        }
    }
}

impl Error for LedgerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) | Self::DurabilityAmbiguous(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    fn test_path(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "greentyper-ledger-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn dropping_writer_unlocks_before_closing_duplicated_handle() {
        let path = test_path("drop-unlocks-duplicate");
        let (ledger, _) = FileLedger::open(&path).expect("create locked ledger");
        let inherited_handle = ledger.file.try_clone().expect("duplicate ledger handle");

        drop(ledger);
        let (reopened, _) = FileLedger::open(&path).expect("reopen after explicit unlock");

        drop(reopened);
        drop(inherited_handle);
        fs::remove_file(path).expect("remove test ledger");
    }

    #[test]
    fn replay_payload_limit_is_cumulative() {
        assert_eq!(
            checked_replay_payload_bytes(MAX_REPLAY_PAYLOAD_BYTES - 1, 1)
                .expect("exact replay payload limit"),
            MAX_REPLAY_PAYLOAD_BYTES
        );
        assert!(matches!(
            checked_replay_payload_bytes(MAX_REPLAY_PAYLOAD_BYTES, 1),
            Err(LedgerError::ReplayPayloadLimitExceeded)
        ));
    }
}
