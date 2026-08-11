use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::provider_discovery::{
    DiscoveredProviderModel, ProviderDiscoveryError, ProviderDiscoveryProfile,
    ProviderDiscoveryState,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempTree(PathBuf);

impl TempTree {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "greentyper-provider-discovery-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create Provider discovery test directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove Provider discovery test directory");
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    PathBuf::from(value)
}

fn snapshot(observed_at_unix_ms: i64) -> ProviderDiscoveryProfile {
    ProviderDiscoveryProfile::new(
        "edge",
        "openai",
        7,
        observed_at_unix_ms,
        vec![DiscoveredProviderModel::new("edge-model", None).expect("discovered model")],
    )
    .expect("Provider discovery snapshot")
}

#[test]
fn discovery_storage_is_atomic_locked_and_private() {
    let temp = TempTree::new();
    let path = temp.path("provider-discovery.json");
    ProviderDiscoveryState::replace_profile(&path, snapshot(1))
        .expect("persist Provider discovery state");
    let before = fs::read(&path).expect("read Provider discovery state");

    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path(&path))
        .expect("open Provider discovery lock");
    lock.try_lock().expect("hold Provider discovery lock");
    assert!(matches!(
        ProviderDiscoveryState::replace_profile(&path, snapshot(2)),
        Err(ProviderDiscoveryError::Locked)
    ));
    assert_eq!(fs::read(&path).expect("read locked state"), before);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&path)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(lock_path(&path))
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn discovery_storage_rejects_oversized_nonregular_and_symlink_paths_without_repair() {
    let temp = TempTree::new();
    let oversized = temp.path("oversized.json");
    let oversized_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&oversized)
        .expect("create oversized state");
    oversized_file
        .set_len(32 * 1024 * 1024 + 1)
        .expect("size oversized state");
    assert!(matches!(
        ProviderDiscoveryState::inspect(&oversized),
        Err(ProviderDiscoveryError::TooLarge)
    ));
    assert_eq!(
        fs::metadata(&oversized).expect("oversized metadata").len(),
        32 * 1024 * 1024 + 1
    );

    let directory = temp.path("directory.json");
    fs::create_dir(&directory).expect("create nonregular state path");
    assert!(matches!(
        ProviderDiscoveryState::inspect(&directory),
        Err(ProviderDiscoveryError::NotRegularFile)
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let target = temp.path("target.json");
        ProviderDiscoveryState::replace_profile(&target, snapshot(1))
            .expect("persist symlink target state");
        let target_before = fs::read(&target).expect("read symlink target");
        let link = temp.path("link.json");
        symlink(&target, &link).expect("create Provider discovery state symlink");
        assert!(matches!(
            ProviderDiscoveryState::inspect(&link),
            Err(ProviderDiscoveryError::SymlinkPath)
        ));
        assert!(matches!(
            ProviderDiscoveryState::replace_profile(&link, snapshot(2)),
            Err(ProviderDiscoveryError::SymlinkPath)
        ));
        assert_eq!(
            fs::read(&target).expect("reread symlink target"),
            target_before
        );
        assert!(!lock_path(&link).exists());
    }
}
