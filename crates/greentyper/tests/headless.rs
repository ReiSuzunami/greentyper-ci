use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::ConfigLayers;
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime,
};
use greentyper_core::runtime::RuntimeKernel;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-headless-{name}-{}-{nonce}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ))
}

fn binary() -> Command {
    let config_root = temp_path("empty-config-root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command
        .env("HOME", &config_root)
        .env("APPDATA", &config_root)
        .env("XDG_CONFIG_HOME", &config_root);
    command
}

fn binary_with_config_root(config_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_greentyper"));
    command
        .env("HOME", config_root)
        .env("APPDATA", config_root)
        .env("XDG_CONFIG_HOME", config_root);
    command
}

fn user_config_path(config_root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        config_root.join("GreenTyper").join("config.toml")
    }
    #[cfg(target_os = "macos")]
    {
        config_root
            .join("Library")
            .join("Application Support")
            .join("GreenTyper")
            .join("config.toml")
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        config_root.join("greentyper").join("config.toml")
    }
}

#[test]
fn status_of_an_unused_path_is_ready_and_read_only() {
    let path = temp_path("missing-status");
    let status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("run status command");
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"ready\n");
    assert!(!path.exists());
}

#[test]
fn headless_command_outputs_then_durably_acknowledges() {
    let path = temp_path("happy");
    let output = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "hello"])
        .output()
        .expect("run headless command");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"simulated: hello\n");

    let status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("run status command");
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"ready\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn headless_refuses_to_repeat_prepared_unacknowledged_output() {
    let path = temp_path("reconcile");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let prepared = runtime
        .execute(&ConfigLayers::default(), "print once", &mut provider)
        .expect("prepare output");
    let delivery = prepared.delivery();
    drop(runtime);

    let blocked = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "must not run"])
        .output()
        .expect("run blocked headless command");
    assert!(!blocked.status.success());
    assert!(blocked.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("reconciliation-required"),
        "{blocked:?}"
    );

    let reconciled = binary()
        .args(["reconcile", "--ledger"])
        .arg(&path)
        .args(["--delivery", &delivery.get().to_string()])
        .output()
        .expect("run reconcile command");
    assert!(reconciled.status.success(), "{reconciled:?}");
    assert_eq!(reconciled.stdout, b"reconciled\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn resume_command_continues_a_durably_admitted_turn() {
    let path = temp_path("resume");
    let mut runtime = RuntimeKernel::open(&path).expect("open Runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut provider = PanicProvider;
        let _ = runtime.execute(&ConfigLayers::default(), "continue", &mut provider);
    }));
    assert!(result.is_err());
    drop(runtime);

    let resumed = binary()
        .args(["resume", "--ledger"])
        .arg(&path)
        .output()
        .expect("run resume command");
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(resumed.stdout, b"simulated: continue\n");
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn headless_uses_configured_provider_and_fails_closed_without_credential() {
    let ledger = temp_path("configured-provider");
    let config_root = temp_path("configured-provider-config");
    let config_path = user_config_path(&config_root);
    fs::create_dir_all(config_path.parent().unwrap()).expect("create config parent");
    fs::write(
        &config_path,
        r#"schema_version = 1

[provider]
profile = "edge"
model = "fixture-model"

[providers.edge]
template = "openai-compatible"
credential = "edge-credential"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"

[providers.edge.pricing]
source = "unknown"
"#,
    )
    .expect("write Provider config");

    let output = binary_with_config_root(&config_root)
        .args(["headless", "--ledger"])
        .arg(&ledger)
        .args(["--input", "must not reach simulator"])
        .output()
        .expect("run configured headless command");
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Provider credential binding was not found")
            || stderr.contains("provider unavailable"),
        "{output:?}"
    );
    assert!(
        !ledger.exists(),
        "missing credentials must fail before durable Turn admission"
    );

    fs::remove_dir_all(config_root).expect("cleanup config root");
}

struct PanicProvider;

impl ProviderRuntime for PanicProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected crash after admission")
    }
}
