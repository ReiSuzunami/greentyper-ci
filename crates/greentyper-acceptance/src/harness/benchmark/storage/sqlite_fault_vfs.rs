#![allow(unsafe_code, unsafe_op_in_unsafe_fn)]

use super::*;
use core::ffi::{c_char, c_int, c_void};
use rusqlite::{Connection, OpenFlags, ffi};
use std::collections::BTreeMap;
use std::mem::{align_of, size_of};
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

const VFS_NAME: &str = "greentyper-fault-v1";
const VFS_NAME_BYTES: &[u8] = b"greentyper-fault-v1\0";
const FILE_ALIGNMENT: usize = align_of::<u128>();
const FAULT_CASES: [FaultCase; 3] = [
    FaultCase::WriteError,
    FaultCase::ShortWrite,
    FaultCase::SyncError,
];

static REGISTRATION: OnceLock<Result<(), String>> = OnceLock::new();
static SESSION_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE_FAULT: Mutex<Option<ActiveFault>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FaultCase {
    WriteError,
    ShortWrite,
    SyncError,
}

impl FaultCase {
    const fn id(self) -> &'static str {
        match self {
            Self::WriteError => "wal-write-error",
            Self::ShortWrite => "wal-short-write",
            Self::SyncError => "wal-sync-error",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveFault {
    case: FaultCase,
    triggered: bool,
}

struct FaultSession {
    case: FaultCase,
    guard: Option<MutexGuard<'static, ()>>,
}

impl FaultSession {
    fn activate(case: FaultCase) -> AppResult<Self> {
        ensure_registered()?;
        let guard = SESSION_LOCK
            .lock()
            .map_err(|_| cli_error("SQLite fault VFS session lock is poisoned"))?;
        let mut active = ACTIVE_FAULT
            .lock()
            .map_err(|_| cli_error("SQLite fault VFS state lock is poisoned"))?;
        if active.is_some() {
            return Err(cli_error("SQLite fault VFS already has an active fault"));
        }
        *active = Some(ActiveFault {
            case,
            triggered: false,
        });
        drop(active);
        Ok(Self {
            case,
            guard: Some(guard),
        })
    }

    fn finish(mut self) -> AppResult<()> {
        let state = ACTIVE_FAULT
            .lock()
            .map_err(|_| cli_error("SQLite fault VFS state lock is poisoned"))?
            .take()
            .ok_or_else(|| cli_error("SQLite fault VFS lost its active fault"))?;
        self.guard.take();
        if state.case != self.case || !state.triggered {
            return Err(cli_error(format!(
                "SQLite fault VFS did not trigger {}",
                self.case.id()
            )));
        }
        Ok(())
    }
}

impl Drop for FaultSession {
    fn drop(&mut self) {
        if self.guard.is_some() {
            if let Ok(mut active) = ACTIVE_FAULT.lock() {
                *active = None;
            }
            self.guard.take();
        }
    }
}

pub(super) fn run_workload(
    run_dir: &Path,
    events: &[EventRecord],
    fault_case_count: u32,
) -> AppResult<BenchmarkObservation> {
    if usize::try_from(fault_case_count)? != FAULT_CASES.len() {
        return Err(cli_error("SQLite VFS fault case count is invalid"));
    }
    let transactions = transaction_slices(events)?;
    if transactions.len() != 2 {
        return Err(cli_error(
            "SQLite VFS fault workload requires exactly two transactions",
        ));
    }
    ensure_registered()?;

    let base = transactions[0];
    let candidate = transactions[1];
    let mut recovered_for_digest = Vec::new();
    let mut prepare_ns = 0_u64;
    let mut fault_and_recovery_ns = 0_u64;
    let mut candidate_transactions_visible = 0_u64;

    for case in FAULT_CASES {
        let case_dir = run_dir.join(case.id());
        create_private_directory(&case_dir)?;
        let database_path = case_dir.join("ledger.sqlite3");

        let prepare_started = Instant::now();
        let connection = open_fault_connection(&database_path)?;
        let mut connection = initialize_sqlite_store(connection)?;
        append_sqlite_events(&mut connection, base)?;
        prepare_ns = prepare_ns
            .checked_add(elapsed_ns(prepare_started)?)
            .ok_or_else(|| cli_error("SQLite VFS prepare duration overflow"))?;

        let fault_started = Instant::now();
        let session = FaultSession::activate(case)?;
        let append_result = append_sqlite_events(&mut connection, candidate);
        session.finish()?;
        if append_result.is_ok() {
            return Err(cli_error(format!(
                "SQLite committed despite injected {}",
                case.id()
            )));
        }
        drop(connection);

        let replayed = replay_sqlite(&database_path)?;
        let is_base = replayed == base;
        let is_complete = replayed == events;
        match case {
            FaultCase::WriteError | FaultCase::ShortWrite if !is_base => {
                return Err(cli_error(format!(
                    "SQLite {} recovery exposed a partial or repeated transaction",
                    case.id()
                )));
            }
            FaultCase::SyncError if !is_base && !is_complete => {
                return Err(cli_error(
                    "SQLite WAL sync failure recovered an incomplete transaction prefix",
                ));
            }
            _ => {}
        }
        if is_complete {
            candidate_transactions_visible = candidate_transactions_visible
                .checked_add(1)
                .ok_or_else(|| cli_error("SQLite visible candidate count overflow"))?;
        }
        recovered_for_digest.extend_from_slice(&replayed);
        fault_and_recovery_ns = fault_and_recovery_ns
            .checked_add(elapsed_ns(fault_started)?)
            .ok_or_else(|| cli_error("SQLite VFS recovery duration overflow"))?;
    }

    Ok(BenchmarkObservation {
        operation_units: u64::try_from(FAULT_CASES.len())?,
        output_digest: canonical_digest(&recovered_for_digest)?,
        timings_ns: BTreeMap::from([
            ("fault_and_recovery".into(), fault_and_recovery_ns),
            ("prepare".into(), prepare_ns),
        ]),
        gauges: BTreeMap::from([
            ("ambiguous_blocked".into(), 1),
            (
                "candidate_transactions_visible".into(),
                candidate_transactions_visible,
            ),
            ("complete_prefix_recoveries".into(), 3),
            ("fault_cases".into(), 3),
            ("integrity_checks".into(), 3),
            ("known_not_repeated".into(), 2),
        ]),
    })
}

fn open_fault_connection(database_path: &Path) -> AppResult<Connection> {
    ensure_registered()?;
    Ok(Connection::open_with_flags_and_vfs(
        database_path,
        OpenFlags::default(),
        VFS_NAME,
    )?)
}

fn ensure_registered() -> AppResult<()> {
    match REGISTRATION.get_or_init(|| unsafe { register_vfs() }) {
        Ok(()) => Ok(()),
        Err(message) => Err(cli_error(message.clone())),
    }
}

#[repr(C)]
struct FaultFile {
    base: ffi::sqlite3_file,
    real_file: *mut ffi::sqlite3_file,
    real_methods: *const ffi::sqlite3_io_methods,
    methods: ffi::sqlite3_io_methods,
    flags: c_int,
}

const FILE_HEADER_BYTES: usize =
    (size_of::<FaultFile>() + FILE_ALIGNMENT - 1) & !(FILE_ALIGNMENT - 1);

unsafe fn register_vfs() -> Result<(), String> {
    let initialized = ffi::sqlite3_initialize();
    if initialized != ffi::SQLITE_OK {
        return Err(format!(
            "SQLite initialization failed before fault VFS registration: {initialized}"
        ));
    }
    let parent = ffi::sqlite3_vfs_find(ptr::null());
    if parent.is_null() {
        return Err("SQLite has no default VFS to wrap".into());
    }
    let parent_file_bytes = usize::try_from((*parent).szOsFile)
        .map_err(|_| "SQLite default VFS file size is invalid".to_owned())?;
    let wrapped_file_bytes = FILE_HEADER_BYTES
        .checked_add(parent_file_bytes)
        .ok_or_else(|| "SQLite fault VFS file size overflow".to_owned())?;
    let wrapped_file_bytes = c_int::try_from(wrapped_file_bytes)
        .map_err(|_| "SQLite fault VFS file size exceeds c_int".to_owned())?;

    let mut wrapper = Box::new(ptr::read(parent));
    wrapper.iVersion = wrapper.iVersion.min(3);
    wrapper.szOsFile = wrapped_file_bytes;
    wrapper.pNext = ptr::null_mut();
    wrapper.zName = VFS_NAME_BYTES.as_ptr().cast::<c_char>();
    wrapper.pAppData = parent.cast::<c_void>();
    wrapper.xOpen = Some(fault_vfs_open);
    wrapper.xDelete = Some(fault_vfs_delete);
    wrapper.xAccess = Some(fault_vfs_access);
    wrapper.xFullPathname = Some(fault_vfs_full_pathname);
    wrapper.xDlOpen = Some(fault_vfs_dl_open);
    wrapper.xDlError = Some(fault_vfs_dl_error);
    wrapper.xDlSym = Some(fault_vfs_dl_sym);
    wrapper.xDlClose = Some(fault_vfs_dl_close);
    wrapper.xRandomness = Some(fault_vfs_randomness);
    wrapper.xSleep = Some(fault_vfs_sleep);
    wrapper.xCurrentTime = Some(fault_vfs_current_time);
    wrapper.xGetLastError = Some(fault_vfs_get_last_error);
    wrapper.xCurrentTimeInt64 = (wrapper.iVersion >= 2).then_some(fault_vfs_current_time_i64);
    wrapper.xSetSystemCall = (wrapper.iVersion >= 3).then_some(fault_vfs_set_system_call);
    wrapper.xGetSystemCall = (wrapper.iVersion >= 3).then_some(fault_vfs_get_system_call);
    wrapper.xNextSystemCall = (wrapper.iVersion >= 3).then_some(fault_vfs_next_system_call);

    let wrapper = Box::into_raw(wrapper);
    let registered = ffi::sqlite3_vfs_register(wrapper, 0);
    if registered != ffi::SQLITE_OK {
        drop(Box::from_raw(wrapper));
        return Err(format!(
            "SQLite fault VFS registration failed: {registered}"
        ));
    }
    Ok(())
}

unsafe fn parent_vfs(vfs: *mut ffi::sqlite3_vfs) -> *mut ffi::sqlite3_vfs {
    (*vfs).pAppData.cast::<ffi::sqlite3_vfs>()
}

unsafe extern "C" fn fault_vfs_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    output_flags: *mut c_int,
) -> c_int {
    if vfs.is_null() || file.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    (*file).pMethods = ptr::null();
    let parent = parent_vfs(vfs);
    if parent.is_null() {
        return ffi::SQLITE_CANTOPEN;
    }
    let parent_file_bytes = match usize::try_from((*parent).szOsFile) {
        Ok(bytes) => bytes,
        Err(_) => return ffi::SQLITE_CANTOPEN,
    };
    let real_file = file
        .cast::<u8>()
        .add(FILE_HEADER_BYTES)
        .cast::<ffi::sqlite3_file>();
    ptr::write_bytes(real_file.cast::<u8>(), 0, parent_file_bytes);
    let Some(open) = (*parent).xOpen else {
        return ffi::SQLITE_CANTOPEN;
    };
    let result = open(parent, name, real_file, flags, output_flags);
    if result != ffi::SQLITE_OK {
        return result;
    }
    let real_methods = (*real_file).pMethods;
    if real_methods.is_null() {
        return ffi::SQLITE_IOERR;
    }
    let wrapped = file.cast::<FaultFile>();
    ptr::write(
        wrapped,
        FaultFile {
            base: ffi::sqlite3_file {
                pMethods: ptr::null(),
            },
            real_file,
            real_methods,
            methods: wrapped_io_methods((*real_methods).iVersion),
            flags,
        },
    );
    (*wrapped).base.pMethods = ptr::addr_of!((*wrapped).methods);
    ffi::SQLITE_OK
}

fn wrapped_io_methods(version: c_int) -> ffi::sqlite3_io_methods {
    let version = version.clamp(1, 3);
    ffi::sqlite3_io_methods {
        iVersion: version,
        xClose: Some(fault_file_close),
        xRead: Some(fault_file_read),
        xWrite: Some(fault_file_write),
        xTruncate: Some(fault_file_truncate),
        xSync: Some(fault_file_sync),
        xFileSize: Some(fault_file_size),
        xLock: Some(fault_file_lock),
        xUnlock: Some(fault_file_unlock),
        xCheckReservedLock: Some(fault_file_check_reserved_lock),
        xFileControl: Some(fault_file_control),
        xSectorSize: Some(fault_file_sector_size),
        xDeviceCharacteristics: Some(fault_file_device_characteristics),
        xShmMap: (version >= 2).then_some(fault_file_shm_map),
        xShmLock: (version >= 2).then_some(fault_file_shm_lock),
        xShmBarrier: (version >= 2).then_some(fault_file_shm_barrier),
        xShmUnmap: (version >= 2).then_some(fault_file_shm_unmap),
        xFetch: (version >= 3).then_some(fault_file_fetch),
        xUnfetch: (version >= 3).then_some(fault_file_unfetch),
    }
}

unsafe fn fault_file_parts(
    file: *mut ffi::sqlite3_file,
) -> (
    *mut ffi::sqlite3_file,
    *const ffi::sqlite3_io_methods,
    c_int,
) {
    let wrapped = file.cast::<FaultFile>();
    (
        (*wrapped).real_file,
        (*wrapped).real_methods,
        (*wrapped).flags,
    )
}

unsafe extern "C" fn fault_file_close(file: *mut ffi::sqlite3_file) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    let result = match (*methods).xClose {
        Some(close) => close(real),
        None => ffi::SQLITE_IOERR_CLOSE,
    };
    (*file).pMethods = ptr::null();
    result
}

unsafe extern "C" fn fault_file_read(
    file: *mut ffi::sqlite3_file,
    buffer: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xRead {
        Some(read) => read(real, buffer, amount, offset),
        None => ffi::SQLITE_IOERR_READ,
    }
}

unsafe extern "C" fn fault_file_write(
    file: *mut ffi::sqlite3_file,
    buffer: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    let (real, methods, flags) = fault_file_parts(file);
    let action = take_fault(flags, false);
    if action == Some(FaultCase::WriteError) {
        return ffi::SQLITE_IOERR_WRITE;
    }
    let Some(write) = (*methods).xWrite else {
        return ffi::SQLITE_IOERR_WRITE;
    };
    if action == Some(FaultCase::ShortWrite) {
        let partial = amount / 2;
        if partial <= 0 {
            return ffi::SQLITE_IOERR_WRITE;
        }
        let result = write(real, buffer, partial, offset);
        return if result == ffi::SQLITE_OK {
            ffi::SQLITE_IOERR_WRITE
        } else {
            result
        };
    }
    write(real, buffer, amount, offset)
}

unsafe extern "C" fn fault_file_truncate(
    file: *mut ffi::sqlite3_file,
    size: ffi::sqlite3_int64,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xTruncate {
        Some(truncate) => truncate(real, size),
        None => ffi::SQLITE_IOERR_TRUNCATE,
    }
}

unsafe extern "C" fn fault_file_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    let (real, methods, open_flags) = fault_file_parts(file);
    if take_fault(open_flags, true) == Some(FaultCase::SyncError) {
        return ffi::SQLITE_IOERR_FSYNC;
    }
    match (*methods).xSync {
        Some(sync) => sync(real, flags),
        None => ffi::SQLITE_IOERR_FSYNC,
    }
}

fn take_fault(open_flags: c_int, sync: bool) -> Option<FaultCase> {
    if open_flags & ffi::SQLITE_OPEN_WAL == 0 {
        return None;
    }
    let Ok(mut active) = ACTIVE_FAULT.lock() else {
        return None;
    };
    let active = active.as_mut()?;
    let matches = if sync {
        active.case == FaultCase::SyncError
    } else {
        matches!(active.case, FaultCase::WriteError | FaultCase::ShortWrite)
    };
    if !matches || active.triggered {
        return None;
    }
    active.triggered = true;
    Some(active.case)
}

unsafe extern "C" fn fault_file_size(
    file: *mut ffi::sqlite3_file,
    size: *mut ffi::sqlite3_int64,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xFileSize {
        Some(file_size) => file_size(real, size),
        None => ffi::SQLITE_IOERR_FSTAT,
    }
}

unsafe extern "C" fn fault_file_lock(file: *mut ffi::sqlite3_file, lock: c_int) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xLock {
        Some(callback) => callback(real, lock),
        None => ffi::SQLITE_IOERR_LOCK,
    }
}

unsafe extern "C" fn fault_file_unlock(file: *mut ffi::sqlite3_file, lock: c_int) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xUnlock {
        Some(callback) => callback(real, lock),
        None => ffi::SQLITE_IOERR_UNLOCK,
    }
}

unsafe extern "C" fn fault_file_check_reserved_lock(
    file: *mut ffi::sqlite3_file,
    output: *mut c_int,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xCheckReservedLock {
        Some(callback) => callback(real, output),
        None => ffi::SQLITE_IOERR_CHECKRESERVEDLOCK,
    }
}

unsafe extern "C" fn fault_file_control(
    file: *mut ffi::sqlite3_file,
    operation: c_int,
    argument: *mut c_void,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xFileControl {
        Some(callback) => callback(real, operation, argument),
        None => ffi::SQLITE_NOTFOUND,
    }
}

unsafe extern "C" fn fault_file_sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    (*methods).xSectorSize.map_or(0, |callback| callback(real))
}

unsafe extern "C" fn fault_file_device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    (*methods)
        .xDeviceCharacteristics
        .map_or(0, |callback| callback(real))
}

unsafe extern "C" fn fault_file_shm_map(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    page_size: c_int,
    extend: c_int,
    output: *mut *mut c_void,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xShmMap {
        Some(callback) => callback(real, page, page_size, extend, output),
        None => ffi::SQLITE_IOERR_SHMMAP,
    }
}

unsafe extern "C" fn fault_file_shm_lock(
    file: *mut ffi::sqlite3_file,
    offset: c_int,
    count: c_int,
    flags: c_int,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xShmLock {
        Some(callback) => callback(real, offset, count, flags),
        None => ffi::SQLITE_IOERR_SHMLOCK,
    }
}

unsafe extern "C" fn fault_file_shm_barrier(file: *mut ffi::sqlite3_file) {
    let (real, methods, _) = fault_file_parts(file);
    if let Some(callback) = (*methods).xShmBarrier {
        callback(real);
    }
}

unsafe extern "C" fn fault_file_shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xShmUnmap {
        Some(callback) => callback(real, delete),
        None => ffi::SQLITE_IOERR_SHMMAP,
    }
}

unsafe extern "C" fn fault_file_fetch(
    file: *mut ffi::sqlite3_file,
    offset: ffi::sqlite3_int64,
    amount: c_int,
    output: *mut *mut c_void,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xFetch {
        Some(callback) => callback(real, offset, amount, output),
        None => ffi::SQLITE_IOERR_MMAP,
    }
}

unsafe extern "C" fn fault_file_unfetch(
    file: *mut ffi::sqlite3_file,
    offset: ffi::sqlite3_int64,
    pointer: *mut c_void,
) -> c_int {
    let (real, methods, _) = fault_file_parts(file);
    match (*methods).xUnfetch {
        Some(callback) => callback(real, offset, pointer),
        None => ffi::SQLITE_IOERR_MMAP,
    }
}

unsafe extern "C" fn fault_vfs_delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    sync_directory: c_int,
) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xDelete {
        Some(callback) => callback(parent, name, sync_directory),
        None => ffi::SQLITE_IOERR_DELETE,
    }
}

unsafe extern "C" fn fault_vfs_access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    output: *mut c_int,
) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xAccess {
        Some(callback) => callback(parent, name, flags, output),
        None => ffi::SQLITE_IOERR_ACCESS,
    }
}

unsafe extern "C" fn fault_vfs_full_pathname(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    output_bytes: c_int,
    output: *mut c_char,
) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xFullPathname {
        Some(callback) => callback(parent, name, output_bytes, output),
        None => ffi::SQLITE_CANTOPEN,
    }
}

unsafe extern "C" fn fault_vfs_dl_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *mut c_void {
    let parent = parent_vfs(vfs);
    (*parent)
        .xDlOpen
        .map_or(ptr::null_mut(), |callback| callback(parent, name))
}

unsafe extern "C" fn fault_vfs_dl_error(
    vfs: *mut ffi::sqlite3_vfs,
    bytes: c_int,
    output: *mut c_char,
) {
    let parent = parent_vfs(vfs);
    if let Some(callback) = (*parent).xDlError {
        callback(parent, bytes, output);
    }
}

unsafe extern "C" fn fault_vfs_dl_sym(
    vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    symbol: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    let parent = parent_vfs(vfs);
    (*parent)
        .xDlSym
        .and_then(|callback| callback(parent, handle, symbol))
}

unsafe extern "C" fn fault_vfs_dl_close(vfs: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    let parent = parent_vfs(vfs);
    if let Some(callback) = (*parent).xDlClose {
        callback(parent, handle);
    }
}

unsafe extern "C" fn fault_vfs_randomness(
    vfs: *mut ffi::sqlite3_vfs,
    bytes: c_int,
    output: *mut c_char,
) -> c_int {
    let parent = parent_vfs(vfs);
    (*parent)
        .xRandomness
        .map_or(0, |callback| callback(parent, bytes, output))
}

unsafe extern "C" fn fault_vfs_sleep(vfs: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    let parent = parent_vfs(vfs);
    (*parent)
        .xSleep
        .map_or(0, |callback| callback(parent, microseconds))
}

unsafe extern "C" fn fault_vfs_current_time(vfs: *mut ffi::sqlite3_vfs, output: *mut f64) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xCurrentTime {
        Some(callback) => callback(parent, output),
        None => ffi::SQLITE_IOERR,
    }
}

unsafe extern "C" fn fault_vfs_get_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    bytes: c_int,
    output: *mut c_char,
) -> c_int {
    let parent = parent_vfs(vfs);
    (*parent)
        .xGetLastError
        .map_or(0, |callback| callback(parent, bytes, output))
}

unsafe extern "C" fn fault_vfs_current_time_i64(
    vfs: *mut ffi::sqlite3_vfs,
    output: *mut ffi::sqlite3_int64,
) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xCurrentTimeInt64 {
        Some(callback) => callback(parent, output),
        None => ffi::SQLITE_IOERR,
    }
}

unsafe extern "C" fn fault_vfs_set_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    callback: ffi::sqlite3_syscall_ptr,
) -> c_int {
    let parent = parent_vfs(vfs);
    match (*parent).xSetSystemCall {
        Some(set) => set(parent, name, callback),
        None => ffi::SQLITE_NOTFOUND,
    }
}

unsafe extern "C" fn fault_vfs_get_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> ffi::sqlite3_syscall_ptr {
    let parent = parent_vfs(vfs);
    (*parent)
        .xGetSystemCall
        .and_then(|callback| callback(parent, name))
}

unsafe extern "C" fn fault_vfs_next_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *const c_char {
    let parent = parent_vfs(vfs);
    (*parent)
        .xNextSystemCall
        .map_or(ptr::null(), |callback| callback(parent, name))
}
