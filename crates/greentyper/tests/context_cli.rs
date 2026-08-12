use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::ConfigLayers;
use greentyper_core::ledger::FileLedger;
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime,
};
use greentyper_core::runtime::RuntimeKernel;
use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "greentyper-context-{name}-{}-{nonce}-{}",
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
        .current_dir(config_root)
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

fn context_command(action: &str, ledger: &Path) -> Command {
    let mut command = binary();
    command.args(["context", action, "--ledger"]).arg(ledger);
    command
}

fn sidecar_path(runtime: &Path, kind: &str) -> PathBuf {
    let mut path = OsString::from(runtime.as_os_str());
    path.push(".");
    path.push(kind);
    PathBuf::from(path)
}

fn json_stdout(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

struct ContextInspectThenPanicProvider;

impl ProviderRuntime for ContextInspectThenPanicProvider {
    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        let context = request.context.as_ref().expect("request Context");
        assert_eq!(context.archived_items(), 0);
        assert_eq!(context.items().len(), 2);
        assert_eq!(context.items()[0].text(), "first Context Turn");
        assert_eq!(context.items()[1].text(), "simulated: first Context Turn");
        assert_eq!(request.input, "recover this Context Turn");
        panic!("injected crash after Context request projection")
    }
}

#[test]
fn context_status_of_a_missing_runtime_is_empty_and_read_only() {
    let ledger = temp_path("missing-status");
    let output = context_command("status", &ledger)
        .output()
        .expect("inspect missing Context state");
    let json = json_stdout(&output);

    assert_eq!(json["head"]["transaction"], 0);
    assert_eq!(json["head"]["sequence"], 0);
    assert_eq!(json["recovered_tail_bytes"], 0);
    assert!(json["checkpoint"].is_null());
    assert!(!ledger.exists());
}

#[test]
fn context_reduce_publishes_a_bounded_checkpoint_and_status_is_read_only() {
    let ledger = temp_path("reduce");
    let mut runtime = RuntimeKernel::open(&ledger).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(
            &ConfigLayers::default(),
            "private context request",
            &mut provider,
        )
        .expect("prepare output");
    runtime
        .acknowledge(output.delivery())
        .expect("complete Turn");
    drop(runtime);

    let before = fs::read(&ledger).expect("read Runtime Ledger");
    let status = context_command("status", &ledger)
        .output()
        .expect("inspect Context state");
    let status_json = json_stdout(&status);
    assert!(status_json["checkpoint"].is_null());
    assert_eq!(fs::read(&ledger).expect("reread Runtime Ledger"), before);

    let reduced = context_command("reduce", &ledger)
        .args(["--max-raw-bytes", "64", "--max-raw-items", "1"])
        .output()
        .expect("reduce Context state");
    let reduced_json = json_stdout(&reduced);
    assert_eq!(reduced_json["checkpoint"]["artifact_count"], 1);
    assert_eq!(reduced_json["checkpoint"]["recent_item_count"], 1);
    assert!(reduced_json["checkpoint"]["raw_bytes"].as_u64().unwrap() <= 64);
    assert!(
        reduced_json["head"]["sequence"].as_u64().unwrap()
            > reduced_json["checkpoint"]["source"]["sequence"]
                .as_u64()
                .unwrap()
    );
    assert!(!String::from_utf8_lossy(&reduced.stdout).contains("private context request"));

    let after_reduce = fs::read(&ledger).expect("read reduced Runtime Ledger");
    assert_ne!(after_reduce, before);
    let status = context_command("status", &ledger)
        .output()
        .expect("inspect reduced Context state");
    let status_json = json_stdout(&status);
    assert_eq!(status_json["checkpoint"], reduced_json["checkpoint"]);
    assert_eq!(
        fs::read(&ledger).expect("reread reduced Runtime Ledger"),
        after_reduce
    );

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
}

#[test]
fn context_reduce_rejects_provider_native_before_runtime_mutation() {
    let ledger = temp_path("provider-native-reduce");
    let config_root = temp_path("provider-native-reduce-config");
    let config_path = user_config_path(&config_root);
    fs::create_dir_all(config_path.parent().expect("Config parent"))
        .expect("create Config directory");
    fs::write(
        &config_path,
        r#"schema_version = 1

[agent]
default_model_preset = "native"

[model_presets.native]
provider = "simulator"
model = "deterministic-v1"
dialect = "responses"
context_mode = "provider_native"
"#,
    )
    .expect("write provider-native Config");
    let mut runtime = RuntimeKernel::open(&ledger).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute(&ConfigLayers::default(), "canonical history", &mut provider)
        .expect("complete canonical Turn");
    runtime
        .acknowledge(output.delivery())
        .expect("acknowledge canonical Turn");
    drop(runtime);
    let bytes_before = fs::read(&ledger).expect("read Runtime Ledger");

    let reduced = binary_with_config_root(&config_root)
        .args(["context", "reduce", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("reject provider-native Context reduction");
    assert!(!reduced.status.success(), "{reduced:?}");
    assert!(reduced.stdout.is_empty(), "{reduced:?}");
    assert!(
        String::from_utf8_lossy(&reduced.stderr)
            .contains("provider-native Context Mode is not available"),
        "{reduced:?}"
    );
    assert_eq!(
        fs::read(&ledger).expect("reread Runtime Ledger"),
        bytes_before
    );
    assert!(!sidecar_path(&ledger, "team").exists());
    assert!(!sidecar_path(&ledger, "tool").exists());

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
    fs::remove_dir_all(config_root).expect("cleanup Config root");
}

#[test]
fn context_reduce_drives_the_next_provider_request_and_survives_explicit_resume() {
    let ledger = temp_path("provider-recovery");
    let mut runtime = RuntimeKernel::open(&ledger).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let first = runtime
        .execute(
            &ConfigLayers::default(),
            "first Context Turn",
            &mut provider,
        )
        .expect("prepare first Context Turn");
    runtime
        .acknowledge(first.delivery())
        .expect("complete first Context Turn");
    drop(runtime);

    let reduced = context_command("reduce", &ledger)
        .args(["--max-raw-bytes", "128", "--max-raw-items", "2"])
        .output()
        .expect("reduce Context state");
    let reduced_json = json_stdout(&reduced);
    assert_eq!(reduced_json["checkpoint"]["artifact_count"], 0);
    assert_eq!(reduced_json["checkpoint"]["recent_item_count"], 2);

    let mut runtime = RuntimeKernel::open(&ledger).expect("reopen reduced Runtime");
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut provider = ContextInspectThenPanicProvider;
        let _ = runtime.execute(
            &ConfigLayers::default(),
            "recover this Context Turn",
            &mut provider,
        );
    }));
    assert!(crashed.is_err());
    drop(runtime);

    let resumed = binary()
        .args(["resume", "--ledger"])
        .arg(&ledger)
        .output()
        .expect("resume Context Turn");
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(resumed.stdout, b"simulated: recover this Context Turn\n");
    assert!(resumed.stderr.is_empty(), "{resumed:?}");

    let status = context_command("status", &ledger)
        .output()
        .expect("inspect resumed Context state");
    let status_json = json_stdout(&status);
    assert_eq!(status_json["checkpoint"], reduced_json["checkpoint"]);

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
}

#[test]
fn context_reduce_rejects_a_non_barrier_without_mutating_the_runtime() {
    let ledger = temp_path("busy");
    let mut runtime = RuntimeKernel::open(&ledger).expect("open Runtime");
    let mut provider = DeterministicProvider::default();
    let _output = runtime
        .execute(&ConfigLayers::default(), "awaiting delivery", &mut provider)
        .expect("prepare output");
    drop(runtime);
    let before = fs::read(&ledger).expect("read Runtime Ledger");

    let reduced = context_command("reduce", &ledger)
        .output()
        .expect("reject unsafe reduction");
    assert!(!reduced.status.success(), "{reduced:?}");
    assert!(reduced.stdout.is_empty(), "{reduced:?}");
    assert!(String::from_utf8_lossy(&reduced.stderr).contains("reconciliation"));
    assert_eq!(fs::read(&ledger).expect("reread Runtime Ledger"), before);

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
}

#[test]
fn context_reduce_checks_product_sidecars_without_mutating_them() {
    let ledger = temp_path("product");
    let team = sidecar_path(&ledger, "team");
    let tool = sidecar_path(&ledger, "tool");
    let (runtime, _recovery) = RuntimeKernel::open_with_team_and_tools(&ledger, &team, &tool, 1)
        .expect("open Product Runtime");
    drop(runtime);
    let runtime_before = fs::read(&ledger).expect("read Runtime Ledger");
    let team_before = fs::read(&team).expect("read Team Ledger");
    let tool_before = fs::read(&tool).expect("read Tool Ledger");

    let reduced = context_command("reduce", &ledger)
        .output()
        .expect("reduce Product Context state");
    let json = json_stdout(&reduced);
    assert_eq!(json["checkpoint"]["artifact_count"], 0);
    assert_eq!(json["checkpoint"]["recent_item_count"], 0);
    assert_ne!(
        fs::read(&ledger).expect("reread Runtime Ledger"),
        runtime_before
    );
    assert_eq!(fs::read(&team).expect("reread Team Ledger"), team_before);
    assert_eq!(fs::read(&tool).expect("reread Tool Ledger"), tool_before);

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
    fs::remove_file(team).expect("cleanup Team Ledger");
    fs::remove_file(tool).expect("cleanup Tool Ledger");
}

#[test]
fn context_reduce_rejects_incomplete_product_state_without_writes() {
    let ledger = temp_path("incomplete-product");
    let team = sidecar_path(&ledger, "team");
    let tool = sidecar_path(&ledger, "tool");
    drop(RuntimeKernel::open(&ledger).expect("open Runtime"));
    let (team_ledger, _) = FileLedger::open(&team).expect("open lone Team sidecar");
    drop(team_ledger);
    let runtime_before = fs::read(&ledger).expect("read Runtime Ledger");
    let team_before = fs::read(&team).expect("read Team Ledger");

    let reduced = context_command("reduce", &ledger)
        .output()
        .expect("reject incomplete Product state");
    assert!(!reduced.status.success(), "{reduced:?}");
    assert!(reduced.stdout.is_empty(), "{reduced:?}");
    assert!(String::from_utf8_lossy(&reduced.stderr).contains("incomplete"));
    assert_eq!(
        fs::read(&ledger).expect("reread Runtime Ledger"),
        runtime_before
    );
    assert_eq!(fs::read(&team).expect("reread Team Ledger"), team_before);
    assert!(!tool.exists());

    fs::remove_file(ledger).expect("cleanup Runtime Ledger");
    fs::remove_file(team).expect("cleanup Team Ledger");
}
