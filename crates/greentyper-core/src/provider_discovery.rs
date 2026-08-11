//! Bounded, independent Provider model-discovery observations.
//!
//! The state stores only opaque Provider Profile fingerprints and model IDs.
//! It carries no credentials, origins, routes, capabilities, or execution
//! authority.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::MAX_CONFIG_ID_BYTES;
use crate::schema::SchemaKind;

pub const PROVIDER_DISCOVERY_SCHEMA_VERSION: u16 = SchemaKind::ProviderDiscovery.current().get();
const MAX_DISCOVERY_STATE_BYTES: usize = 32 * 1024 * 1024;
const MAX_DISCOVERY_PROFILES: usize = 64;
const MAX_DISCOVERED_MODELS: usize = 1024;
const MAX_DISCOVERED_MODEL_ID_BYTES: usize = 256;
const MAX_CATALOG_KEY_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDiscoveryState {
    schema_version: u16,
    profiles: Vec<ProviderDiscoveryProfile>,
}

impl ProviderDiscoveryState {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: PROVIDER_DISCOVERY_SCHEMA_VERSION,
            profiles: Vec::new(),
        }
    }

    pub fn inspect(path: &Path) -> Result<Self, ProviderDiscoveryError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(error) => return Err(ProviderDiscoveryError::Io(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(ProviderDiscoveryError::SymlinkPath);
        }
        if !metadata.is_file() {
            return Err(ProviderDiscoveryError::NotRegularFile);
        }
        if metadata.len() > max_state_bytes_u64()? {
            return Err(ProviderDiscoveryError::TooLarge);
        }

        let mut options = OpenOptions::new();
        options.read(true);
        configure_no_follow(&mut options);
        let mut file = options.open(path).map_err(map_open_error)?;
        let opened = file.metadata().map_err(ProviderDiscoveryError::Io)?;
        if !opened.is_file() {
            return Err(ProviderDiscoveryError::NotRegularFile);
        }
        let limit = max_state_bytes_u64()?
            .checked_add(1)
            .ok_or(ProviderDiscoveryError::TooLarge)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit)
            .read_to_end(&mut bytes)
            .map_err(ProviderDiscoveryError::Io)?;
        if bytes.len() > MAX_DISCOVERY_STATE_BYTES {
            return Err(ProviderDiscoveryError::TooLarge);
        }
        let state: Self =
            serde_json::from_slice(&bytes).map_err(|_| ProviderDiscoveryError::Corrupt)?;
        state.validate()?;
        Ok(state)
    }

    pub fn replace_profile(
        path: &Path,
        profile: ProviderDiscoveryProfile,
    ) -> Result<Self, ProviderDiscoveryError> {
        profile.validate()?;
        reject_non_regular_write_target(path)?;
        let _lock = lock_state(path)?;
        let mut state = Self::inspect(path)?;
        match state
            .profiles
            .binary_search_by(|candidate| candidate.profile.cmp(&profile.profile))
        {
            Ok(index) => state.profiles[index] = profile,
            Err(index) => state.profiles.insert(index, profile),
        }
        state.validate()?;
        let bytes = serde_json::to_vec(&state).map_err(|_| ProviderDiscoveryError::Corrupt)?;
        if bytes.len() > MAX_DISCOVERY_STATE_BYTES {
            return Err(ProviderDiscoveryError::TooLarge);
        }
        atomic_write(path, &bytes)?;
        Ok(state)
    }

    #[must_use]
    pub fn profiles(&self) -> &[ProviderDiscoveryProfile] {
        &self.profiles
    }

    fn validate(&self) -> Result<(), ProviderDiscoveryError> {
        SchemaKind::ProviderDiscovery
            .require_current(self.schema_version)
            .map_err(|_| ProviderDiscoveryError::UnsupportedSchema)?;
        if self.profiles.len() > MAX_DISCOVERY_PROFILES {
            return Err(ProviderDiscoveryError::Corrupt);
        }
        let mut previous = None;
        for profile in &self.profiles {
            profile.validate()?;
            if previous.is_some_and(|value: &str| value >= profile.profile.as_str()) {
                return Err(ProviderDiscoveryError::Corrupt);
            }
            previous = Some(profile.profile.as_str());
        }
        Ok(())
    }
}

impl Default for ProviderDiscoveryState {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDiscoveryProfile {
    profile: String,
    template: String,
    fingerprint: u64,
    observed_at_unix_ms: i64,
    models: Vec<DiscoveredProviderModel>,
}

impl ProviderDiscoveryProfile {
    pub fn new(
        profile: impl Into<String>,
        template: impl Into<String>,
        fingerprint: u64,
        observed_at_unix_ms: i64,
        mut models: Vec<DiscoveredProviderModel>,
    ) -> Result<Self, ProviderDiscoveryError> {
        models.sort_by(|left, right| left.id.cmp(&right.id));
        let profile = Self {
            profile: profile.into(),
            template: template.into(),
            fingerprint,
            observed_at_unix_ms,
            models,
        };
        profile.validate()?;
        Ok(profile)
    }

    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub fn template(&self) -> &str {
        &self.template
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    #[must_use]
    pub const fn observed_at_unix_ms(&self) -> i64 {
        self.observed_at_unix_ms
    }

    #[must_use]
    pub fn models(&self) -> &[DiscoveredProviderModel] {
        &self.models
    }

    fn validate(&self) -> Result<(), ProviderDiscoveryError> {
        validate_config_id(&self.profile)?;
        validate_config_id(&self.template)?;
        if self.observed_at_unix_ms <= 0 {
            return Err(ProviderDiscoveryError::Corrupt);
        }
        if self.models.len() > MAX_DISCOVERED_MODELS {
            return Err(ProviderDiscoveryError::Corrupt);
        }
        let mut previous = None;
        for model in &self.models {
            model.validate()?;
            if previous.is_some_and(|value: &str| value >= model.id.as_str()) {
                return Err(ProviderDiscoveryError::Corrupt);
            }
            previous = Some(model.id.as_str());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredProviderModel {
    id: String,
    release_catalog_key: Option<String>,
}

impl DiscoveredProviderModel {
    pub fn new(
        id: impl Into<String>,
        release_catalog_key: Option<String>,
    ) -> Result<Self, ProviderDiscoveryError> {
        let model = Self {
            id: id.into(),
            release_catalog_key,
        };
        model.validate()?;
        Ok(model)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn release_catalog_key(&self) -> Option<&str> {
        self.release_catalog_key.as_deref()
    }

    fn validate(&self) -> Result<(), ProviderDiscoveryError> {
        if self.id.is_empty()
            || self.id.len() > MAX_DISCOVERED_MODEL_ID_BYTES
            || self.id.chars().any(char::is_whitespace)
            || self.id.chars().any(char::is_control)
        {
            return Err(ProviderDiscoveryError::Corrupt);
        }
        if self.release_catalog_key.as_ref().is_some_and(|key| {
            key.is_empty()
                || key.len() > MAX_CATALOG_KEY_BYTES
                || key.chars().any(char::is_whitespace)
                || key.chars().any(char::is_control)
        }) {
            return Err(ProviderDiscoveryError::Corrupt);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ProviderDiscoveryError {
    Corrupt,
    UnsupportedSchema,
    TooLarge,
    SymlinkPath,
    NotRegularFile,
    Locked,
    ObservationMismatch,
    MissingObservation,
    StaleObservation,
    UnknownModel,
    Io(io::Error),
}

impl fmt::Display for ProviderDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt => write!(formatter, "Provider discovery state is corrupt"),
            Self::UnsupportedSchema => {
                write!(formatter, "Provider discovery state schema is unsupported")
            }
            Self::TooLarge => write!(formatter, "Provider discovery state is too large"),
            Self::SymlinkPath => write!(formatter, "Provider discovery state path is a symlink"),
            Self::NotRegularFile => {
                write!(
                    formatter,
                    "Provider discovery state path is not a regular file"
                )
            }
            Self::Locked => write!(formatter, "Provider discovery state is busy"),
            Self::ObservationMismatch => {
                write!(
                    formatter,
                    "Provider discovery observation does not match the Profile"
                )
            }
            Self::MissingObservation => {
                write!(formatter, "Provider discovery observation is missing")
            }
            Self::StaleObservation => {
                write!(formatter, "Provider discovery observation is stale")
            }
            Self::UnknownModel => write!(formatter, "Provider discovery model is unknown"),
            Self::Io(_) => write!(formatter, "Provider discovery state I/O failed"),
        }
    }
}

impl Error for ProviderDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

fn validate_config_id(value: &str) -> Result<(), ProviderDiscoveryError> {
    if value.is_empty()
        || value.len() > MAX_CONFIG_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(ProviderDiscoveryError::Corrupt);
    }
    Ok(())
}

fn max_state_bytes_u64() -> Result<u64, ProviderDiscoveryError> {
    u64::try_from(MAX_DISCOVERY_STATE_BYTES).map_err(|_| ProviderDiscoveryError::TooLarge)
}

fn map_open_error(error: io::Error) -> ProviderDiscoveryError {
    if error.raw_os_error().is_some_and(is_symlink_open_error) {
        ProviderDiscoveryError::SymlinkPath
    } else {
        ProviderDiscoveryError::Io(error)
    }
}

fn lock_state(path: &Path) -> Result<File, ProviderDiscoveryError> {
    let lock_path = lock_path(path);
    let parent = lock_path.parent().ok_or_else(|| {
        ProviderDiscoveryError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider discovery lock path has no parent",
        ))
    })?;
    fs::create_dir_all(parent).map_err(ProviderDiscoveryError::Io)?;
    reject_non_regular_write_target(&lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    configure_no_follow(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(&lock_path).map_err(map_open_error)?;
    if !file
        .metadata()
        .map_err(ProviderDiscoveryError::Io)?
        .is_file()
    {
        return Err(ProviderDiscoveryError::NotRegularFile);
    }
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => ProviderDiscoveryError::Locked,
        TryLockError::Error(source) => ProviderDiscoveryError::Io(source),
    })?;
    Ok(file)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ProviderDiscoveryError> {
    let parent = path.parent().ok_or_else(|| {
        ProviderDiscoveryError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Provider discovery state path has no parent",
        ))
    })?;
    fs::create_dir_all(parent).map_err(ProviderDiscoveryError::Io)?;
    reject_non_regular_write_target(path)?;
    #[cfg(unix)]
    let mut options = atomic_write_file::OpenOptions::new();
    #[cfg(not(unix))]
    let options = atomic_write_file::OpenOptions::new();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as AtomicOpenOptionsExt;
        use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;

        AtomicOpenOptionsExt::preserve_mode(&mut options, false);
        StdOpenOptionsExt::mode(&mut options, 0o600);
    }
    let mut file = options.open(path).map_err(ProviderDiscoveryError::Io)?;
    file.write_all(bytes).map_err(ProviderDiscoveryError::Io)?;
    file.flush().map_err(ProviderDiscoveryError::Io)?;
    file.commit().map_err(ProviderDiscoveryError::Io)?;
    Ok(())
}

fn reject_non_regular_write_target(path: &Path) -> Result<(), ProviderDiscoveryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ProviderDiscoveryError::SymlinkPath)
        }
        Ok(metadata) if !metadata.is_file() => Err(ProviderDiscoveryError::NotRegularFile),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ProviderDiscoveryError::Io(source)),
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    PathBuf::from(value)
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn is_symlink_open_error(code: i32) -> bool {
    code == libc::ELOOP
}

#[cfg(windows)]
fn is_symlink_open_error(code: i32) -> bool {
    const ERROR_CANT_ACCESS_FILE: i32 = 1920;
    code == ERROR_CANT_ACCESS_FILE
}

#[cfg(not(any(unix, windows)))]
fn is_symlink_open_error(_code: i32) -> bool {
    false
}
