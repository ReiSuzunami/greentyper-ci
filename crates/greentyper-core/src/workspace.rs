//! Local Workspace Coordinator facts, leases, and stale-read protection.
//!
//! Workspace state deliberately lives outside Team events.  This module owns
//! only bounded, local facts: a canonical workspace identity, one shared lock
//! for readers/exclusive lock for writers, and content digests for files a
//! writer observed before mutation.  It never executes Git or mutates files
//! during read-set validation.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_ROOT_PATH_BYTES: usize = 4096;
const MAX_RELATIVE_PATH_BYTES: usize = 512;
pub const MAX_READ_SET_ENTRIES: usize = 1024;
pub const MAX_READ_SET_JSON_BYTES: u64 = 1024 * 1024;
const MAX_READ_SET_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreeId(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseId(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReadSetId(String);

impl WorkspaceId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl WorktreeId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl LeaseId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ReadSetId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFacts {
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub root_fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLeaseFacts {
    pub lease_id: LeaseId,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub access: WorkspaceAccess,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSetEntry {
    pub path: String,
    pub bytes: u64,
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSet {
    pub read_set_id: ReadSetId,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub observed_revision: String,
    pub entries: Vec<ReadSetEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadSetValidation {
    pub read_set_id: ReadSetId,
    pub workspace_id: WorkspaceId,
    pub worktree_id: WorktreeId,
    pub current_revision: String,
    pub stale_paths: Vec<String>,
    pub valid: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRoot {
    path: PathBuf,
    workspace_id: WorkspaceId,
    worktree_id: WorktreeId,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl WorkspaceRoot {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let path = path.as_ref();
        validate_root_path(path)?;
        let canonical = fs::canonicalize(path).map_err(WorkspaceError::Io)?;
        validate_root_path(&canonical)?;
        let bytes = canonical.as_os_str().to_string_lossy();
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(&canonical).map_err(WorkspaceError::Io)?;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(unix)]
        let worktree_identity = format!("{bytes}:{device}:{inode}");
        #[cfg(not(unix))]
        let worktree_identity = bytes.to_string();
        Ok(Self {
            path: canonical.clone(),
            workspace_id: WorkspaceId(hash_tagged("workspace", bytes.as_bytes())),
            worktree_id: WorktreeId(hash_tagged("worktree", worktree_identity.as_bytes())),
            #[cfg(unix)]
            device,
            #[cfg(unix)]
            inode,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    #[must_use]
    pub fn worktree_id(&self) -> &WorktreeId {
        &self.worktree_id
    }

    #[must_use]
    pub fn facts(&self) -> WorkspaceFacts {
        WorkspaceFacts {
            workspace_id: self.workspace_id.clone(),
            worktree_id: self.worktree_id.clone(),
            root_fingerprint: hash_tagged("root", self.worktree_id.as_str().as_bytes()),
        }
    }

    pub fn acquire_lease(&self, access: WorkspaceAccess) -> Result<WorkspaceLease, WorkspaceError> {
        let file = open_root_directory(self)?;
        let lock_result = match access {
            WorkspaceAccess::ReadOnly => file.try_lock_shared(),
            WorkspaceAccess::ReadWrite => file.try_lock(),
        };
        lock_result.map_err(|error| match error {
            TryLockError::WouldBlock => WorkspaceError::Locked,
            TryLockError::Error(source) => WorkspaceError::Io(source),
        })?;

        let nonce = format!(
            "{}:{}:{}",
            std::process::id(),
            now_unix_nanos(),
            match access {
                WorkspaceAccess::ReadOnly => "r",
                WorkspaceAccess::ReadWrite => "w",
            }
        );
        let lease_id = LeaseId(hash_tagged(
            "lease",
            format!("{}:{nonce}", self.worktree_id.as_str()).as_bytes(),
        ));
        Ok(WorkspaceLease {
            file,
            facts: WorkspaceLeaseFacts {
                lease_id,
                workspace_id: self.workspace_id.clone(),
                worktree_id: self.worktree_id.clone(),
                access,
            },
        })
    }
}

pub struct WorkspaceLease {
    file: File,
    facts: WorkspaceLeaseFacts,
}

impl WorkspaceLease {
    #[must_use]
    pub fn facts(&self) -> &WorkspaceLeaseFacts {
        &self.facts
    }

    pub fn release(self) {
        drop(self);
    }

    /// Capture a Read Set while holding this Lease's lock.
    pub fn capture_read_set(
        &self,
        root: &WorkspaceRoot,
        paths: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<ReadSet, WorkspaceError> {
        self.ensure_root(root)?;
        ReadSet::capture(root, paths)
    }

    /// Validate a Read Set while holding this Lease's lock.
    pub fn validate_read_set(
        &self,
        root: &WorkspaceRoot,
        read_set: &ReadSet,
    ) -> Result<ReadSetValidation, WorkspaceError> {
        self.ensure_root(root)?;
        read_set.validate(root)
    }

    /// Writer gate: stale data or a read-only Lease fails before mutation.
    pub fn validate_before_mutation(
        &self,
        root: &WorkspaceRoot,
        read_set: &ReadSet,
    ) -> Result<(), WorkspaceError> {
        if self.facts.access != WorkspaceAccess::ReadWrite {
            return Err(WorkspaceError::WriteLeaseRequired);
        }
        self.ensure_root(root)?;
        read_set.validate_before_mutation(root)
    }

    fn ensure_root(&self, root: &WorkspaceRoot) -> Result<(), WorkspaceError> {
        if self.facts.workspace_id != *root.workspace_id()
            || self.facts.worktree_id != *root.worktree_id()
        {
            Err(WorkspaceError::IdentityMismatch)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for WorkspaceLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.facts.fmt(formatter)
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl ReadSet {
    pub fn from_json_reader(reader: impl Read) -> Result<Self, WorkspaceError> {
        let mut bytes = Vec::new();
        reader
            .take(MAX_READ_SET_JSON_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(WorkspaceError::Io)?;
        if bytes.len() as u64 > MAX_READ_SET_JSON_BYTES {
            return Err(WorkspaceError::TooLarge);
        }
        let read_set: Self = serde_json::from_slice(&bytes).map_err(|_| WorkspaceError::Corrupt)?;
        read_set.validate_shape()?;
        Ok(read_set)
    }

    pub fn capture(
        root: &WorkspaceRoot,
        paths: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, WorkspaceError> {
        let mut entries = Vec::new();
        for path in paths {
            if entries.len() == MAX_READ_SET_ENTRIES {
                return Err(WorkspaceError::TooManyEntries);
            }
            let path = path.into();
            let relative = validate_relative_path(&path)?;
            entries.push(read_entry(root, relative.to_string_lossy().as_ref())?);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        if entries.windows(2).any(|pair| pair[0].path == pair[1].path) {
            return Err(WorkspaceError::DuplicatePath);
        }
        let observed_revision = revision_for_entries(&entries);
        let read_set_id = ReadSetId(hash_tagged(
            "read-set",
            format!("{}:{}", root.worktree_id.as_str(), observed_revision).as_bytes(),
        ));
        Ok(Self {
            read_set_id,
            workspace_id: root.workspace_id.clone(),
            worktree_id: root.worktree_id.clone(),
            observed_revision,
            entries,
        })
    }

    pub fn validate(&self, root: &WorkspaceRoot) -> Result<ReadSetValidation, WorkspaceError> {
        self.validate_shape()?;
        if self.workspace_id != *root.workspace_id() || self.worktree_id != *root.worktree_id() {
            return Err(WorkspaceError::IdentityMismatch);
        }
        let mut stale_paths = Vec::new();
        let mut current = Vec::with_capacity(self.entries.len());
        for expected in &self.entries {
            match read_entry(root, &expected.path) {
                Ok(actual) if actual == *expected => current.push(actual),
                Ok(_)
                | Err(
                    WorkspaceError::MissingPath
                    | WorkspaceError::SymlinkPath
                    | WorkspaceError::NotRegularFile,
                ) => {
                    stale_paths.push(expected.path.clone());
                }
                Err(error) => return Err(error),
            }
        }
        let current_revision = revision_for_entries(&current);
        let valid = stale_paths.is_empty() && current_revision == self.observed_revision;
        Ok(ReadSetValidation {
            read_set_id: self.read_set_id.clone(),
            workspace_id: root.workspace_id.clone(),
            worktree_id: root.worktree_id.clone(),
            current_revision,
            stale_paths,
            valid,
        })
    }

    pub fn validate_before_mutation(&self, root: &WorkspaceRoot) -> Result<(), WorkspaceError> {
        let result = self.validate(root)?;
        if result.stale_paths.is_empty() && result.current_revision == self.observed_revision {
            Ok(())
        } else {
            Err(WorkspaceError::StaleReadSet {
                changed_paths: result.stale_paths,
            })
        }
    }

    fn validate_shape(&self) -> Result<(), WorkspaceError> {
        validate_digest(self.workspace_id.as_str())?;
        validate_digest(self.worktree_id.as_str())?;
        validate_digest(self.read_set_id.as_str())?;
        validate_digest(&self.observed_revision)?;
        if self.entries.len() > MAX_READ_SET_ENTRIES {
            return Err(WorkspaceError::TooManyEntries);
        }
        let mut previous = None;
        for entry in &self.entries {
            let relative = validate_relative_path(&entry.path)?;
            if relative.to_string_lossy() != entry.path {
                return Err(WorkspaceError::InvalidRelativePath);
            }
            if entry.bytes > MAX_READ_SET_FILE_BYTES {
                return Err(WorkspaceError::TooLarge);
            }
            validate_digest(&entry.digest)?;
            if previous.is_some_and(|value: &str| value >= entry.path.as_str()) {
                return Err(WorkspaceError::DuplicatePath);
            }
            previous = Some(entry.path.as_str());
        }
        if revision_for_entries(&self.entries) != self.observed_revision {
            return Err(WorkspaceError::Corrupt);
        }
        let expected_id = hash_tagged(
            "read-set",
            format!("{}:{}", self.worktree_id.as_str(), self.observed_revision).as_bytes(),
        );
        if expected_id != self.read_set_id.0 {
            return Err(WorkspaceError::Corrupt);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum WorkspaceError {
    InvalidRoot,
    RootNotFound,
    SymlinkPath,
    NotDirectory,
    InvalidRelativePath,
    MissingPath,
    NotRegularFile,
    TooLarge,
    TooManyEntries,
    DuplicatePath,
    Locked,
    WriteLeaseRequired,
    IdentityMismatch,
    StaleReadSet { changed_paths: Vec<String> },
    Corrupt,
    UnsupportedPlatform,
    Io(io::Error),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot => write!(formatter, "workspace root must be an absolute directory"),
            Self::RootNotFound => write!(formatter, "workspace root does not exist"),
            Self::SymlinkPath => write!(formatter, "workspace path must not be a symlink"),
            Self::NotDirectory => write!(formatter, "workspace path is not a directory"),
            Self::InvalidRelativePath => write!(formatter, "workspace read-set path is invalid"),
            Self::MissingPath => write!(formatter, "workspace read-set path is missing"),
            Self::NotRegularFile => {
                write!(formatter, "workspace read-set path is not a regular file")
            }
            Self::TooLarge => write!(formatter, "workspace read-set file is too large"),
            Self::TooManyEntries => write!(formatter, "workspace read-set has too many entries"),
            Self::DuplicatePath => write!(formatter, "workspace read-set contains duplicate paths"),
            Self::Locked => write!(formatter, "workspace lease is busy"),
            Self::WriteLeaseRequired => write!(formatter, "workspace write lease is required"),
            Self::IdentityMismatch => {
                write!(formatter, "workspace identity does not match read-set")
            }
            Self::StaleReadSet { changed_paths } => {
                write!(
                    formatter,
                    "workspace read-set is stale ({} changed paths)",
                    changed_paths.len()
                )
            }
            Self::Corrupt => write!(formatter, "workspace read-set is corrupt"),
            Self::UnsupportedPlatform => write!(
                formatter,
                "workspace leases and read sets are unavailable on this platform"
            ),
            Self::Io(_) => write!(formatter, "workspace I/O failed"),
        }
    }
}

impl Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

fn validate_root_path(path: &Path) -> Result<(), WorkspaceError> {
    if path.as_os_str().is_empty()
        || !path.is_absolute()
        || path.as_os_str().len() > MAX_ROOT_PATH_BYTES
    {
        return Err(WorkspaceError::InvalidRoot);
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkspaceError::RootNotFound
        } else {
            WorkspaceError::Io(error)
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(WorkspaceError::SymlinkPath);
    }
    if !metadata.is_dir() {
        return Err(WorkspaceError::NotDirectory);
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<PathBuf, WorkspaceError> {
    if path.is_empty() || path.len() > MAX_RELATIVE_PATH_BYTES || path.chars().any(char::is_control)
    {
        return Err(WorkspaceError::InvalidRelativePath);
    }
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WorkspaceError::InvalidRelativePath);
    }
    Ok(relative.to_path_buf())
}

fn read_entry(root: &WorkspaceRoot, relative: &str) -> Result<ReadSetEntry, WorkspaceError> {
    let relative = validate_relative_path(relative)?;
    let file = open_relative_file(root, &relative)?;
    let metadata = file.metadata().map_err(WorkspaceError::Io)?;
    if !metadata.is_file() {
        return Err(WorkspaceError::NotRegularFile);
    }
    if metadata.len() > MAX_READ_SET_FILE_BYTES {
        return Err(WorkspaceError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_READ_SET_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(WorkspaceError::Io)?;
    if bytes.len() as u64 > MAX_READ_SET_FILE_BYTES {
        return Err(WorkspaceError::TooLarge);
    }
    Ok(ReadSetEntry {
        path: relative.to_string_lossy().into_owned(),
        bytes: bytes.len() as u64,
        digest: hex_digest(&bytes),
    })
}

fn revision_for_entries(entries: &[ReadSetEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update(entry.bytes.to_le_bytes());
        hasher.update(entry.digest.as_bytes());
        hasher.update([0xff]);
    }
    hex_digest(hasher.finalize())
}

fn hash_tagged(tag: &str, value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    hasher.update([0]);
    hasher.update(value);
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_digest(value: &str) -> Result<(), WorkspaceError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(WorkspaceError::Corrupt)
    }
}

#[cfg(unix)]
fn map_open_error(error: io::Error) -> WorkspaceError {
    if error.kind() == io::ErrorKind::NotFound {
        return WorkspaceError::MissingPath;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return WorkspaceError::SymlinkPath;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ENOTDIR) {
        return WorkspaceError::NotRegularFile;
    }
    WorkspaceError::Io(error)
}

#[cfg(unix)]
fn open_root_directory(root: &WorkspaceRoot) -> Result<File, WorkspaceError> {
    use rustix::fs::{Mode, OFlags, fstat, open};

    let descriptor = open(
        root.path(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(map_rustix_open_error)?;
    let stat = fstat(&descriptor).map_err(map_rustix_open_error)?;
    if stat.st_dev as u64 != root.device || stat.st_ino as u64 != root.inode {
        return Err(WorkspaceError::IdentityMismatch);
    }
    Ok(File::from(descriptor))
}

#[cfg(not(unix))]
fn open_root_directory(_root: &WorkspaceRoot) -> Result<File, WorkspaceError> {
    Err(WorkspaceError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_relative_file(root: &WorkspaceRoot, relative: &Path) -> Result<File, WorkspaceError> {
    use rustix::fs::{Mode, OFlags, openat};

    let mut directory = open_root_directory(root)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(part) = component else {
            return Err(WorkspaceError::InvalidRelativePath);
        };
        let last = components.peek().is_none();
        let flags = if last {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        };
        let descriptor =
            openat(&directory, part, flags, Mode::empty()).map_err(map_rustix_open_error)?;
        if last {
            return Ok(File::from(descriptor));
        }
        directory = File::from(descriptor);
    }
    Err(WorkspaceError::InvalidRelativePath)
}

#[cfg(not(unix))]
fn open_relative_file(_root: &WorkspaceRoot, _relative: &Path) -> Result<File, WorkspaceError> {
    Err(WorkspaceError::UnsupportedPlatform)
}

#[cfg(unix)]
fn map_rustix_open_error(error: rustix::io::Errno) -> WorkspaceError {
    map_open_error(io::Error::from_raw_os_error(error.raw_os_error()))
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    fn temp_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "greentyper-workspace-{label}-{}-{}",
            std::process::id(),
            now_unix_nanos()
        ));
        fs::create_dir_all(&path).expect("temp root");
        path
    }

    #[test]
    fn facts_are_stable_and_paths_are_not_serialized() {
        let path = temp_root("facts");
        let root = WorkspaceRoot::open(&path).expect("root");
        let facts = root.facts();
        let json = serde_json::to_string(&facts).expect("facts json");
        assert!(json.contains("workspace_id"));
        assert!(!json.contains(path.to_string_lossy().as_ref()));
        fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn writer_is_exclusive_but_readers_can_share() {
        let path = temp_root("lease");
        let root = Arc::new(WorkspaceRoot::open(&path).expect("root"));
        let writer = root
            .acquire_lease(WorkspaceAccess::ReadWrite)
            .expect("writer");
        assert!(matches!(
            root.acquire_lease(WorkspaceAccess::ReadWrite),
            Err(WorkspaceError::Locked)
        ));
        assert!(matches!(
            root.acquire_lease(WorkspaceAccess::ReadOnly),
            Err(WorkspaceError::Locked)
        ));
        drop(writer);
        let reader = root
            .acquire_lease(WorkspaceAccess::ReadOnly)
            .expect("reader");
        let second = root
            .acquire_lease(WorkspaceAccess::ReadOnly)
            .expect("reader 2");
        drop(reader);
        drop(second);
        fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn read_set_detects_mutation_without_writing() {
        let path = temp_root("read-set");
        fs::write(path.join("tracked.txt"), b"before").expect("write");
        let root = WorkspaceRoot::open(&path).expect("root");
        let read_set = ReadSet::capture(&root, ["tracked.txt"]).expect("capture");
        read_set.validate_before_mutation(&root).expect("fresh");
        fs::write(path.join("tracked.txt"), b"after").expect("mutate");
        let result = read_set.validate(&root).expect("validate");
        assert_eq!(result.stale_paths, vec!["tracked.txt"]);
        assert!(matches!(
            read_set.validate_before_mutation(&root),
            Err(WorkspaceError::StaleReadSet { .. })
        ));
        fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn read_only_lease_cannot_pass_writer_gate() {
        let path = temp_root("writer-gate");
        fs::write(path.join("tracked.txt"), b"safe").expect("write");
        let root = WorkspaceRoot::open(&path).expect("root");
        let read_set = ReadSet::capture(&root, ["tracked.txt"]).expect("capture");
        let lease = root
            .acquire_lease(WorkspaceAccess::ReadOnly)
            .expect("reader");
        assert!(matches!(
            lease.validate_before_mutation(&root, &read_set),
            Err(WorkspaceError::WriteLeaseRequired)
        ));
        fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn traversal_and_symlink_entries_fail_closed() {
        let path = temp_root("paths");
        fs::write(path.join("tracked.txt"), b"safe").expect("write");
        let root = WorkspaceRoot::open(&path).expect("root");
        assert!(matches!(
            ReadSet::capture(&root, ["../tracked.txt"]),
            Err(WorkspaceError::InvalidRelativePath)
        ));
        #[cfg(unix)]
        std::os::unix::fs::symlink(path.join("tracked.txt"), path.join("link.txt"))
            .expect("symlink");
        #[cfg(unix)]
        assert!(matches!(
            ReadSet::capture(&root, ["link.txt"]),
            Err(WorkspaceError::SymlinkPath)
        ));
        fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn lease_blocks_across_threads() {
        let path = temp_root("thread");
        let root = Arc::new(WorkspaceRoot::open(&path).expect("root"));
        let held = root
            .acquire_lease(WorkspaceAccess::ReadWrite)
            .expect("writer");
        let clone = Arc::clone(&root);
        let join = thread::spawn(move || clone.acquire_lease(WorkspaceAccess::ReadWrite));
        assert!(matches!(
            join.join().expect("join"),
            Err(WorkspaceError::Locked)
        ));
        drop(held);
        fs::remove_dir_all(path).expect("cleanup");
    }
}
