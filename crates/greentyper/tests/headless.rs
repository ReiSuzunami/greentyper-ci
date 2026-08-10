use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use greentyper_core::config::ConfigLayers;
use greentyper_core::pricing::{
    PriceSchedule, PriceScheduleBook, PriceScheduleDefinition, PriceScheduleSource, TokenRates,
};
use greentyper_core::provider::{
    DeterministicProvider, ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime,
    UsageRecord,
};
use greentyper_core::runtime::RuntimeKernel;
use greentyper_core::usage::UsageTimestamp;

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
fn stats_reports_replayed_usage_without_user_text() {
    let path = temp_path("stats");
    let private_input = "usage-private-input-marker";
    let output = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", private_input])
        .output()
        .expect("run headless command");
    assert!(output.status.success(), "{output:?}");

    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis()
        .saturating_add(1)
        .to_string();
    let stats = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str()])
        .output()
        .expect("run stats command");
    assert!(stats.status.success(), "{stats:?}");
    let text = String::from_utf8(stats.stdout).expect("stats UTF-8");
    assert!(!text.contains(private_input), "{text}");
    let document: serde_json::Value = serde_json::from_str(&text).expect("stats JSON");
    assert_eq!(document["attempts"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["attempts"][0]["outcome"], "succeeded");
    assert_eq!(document["attempts"][0]["usage"]["accuracy"], "estimated");
    assert_eq!(document["attempts"][0]["cost_provenance"], "unknown");
    assert_eq!(document["thread"]["usage"]["attempts"], 1);
    assert_eq!(document["team"], serde_json::Value::Null);
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn stats_summary_and_pages_are_bounded_and_revision_bound() {
    let path = temp_path("paged-stats");
    for input in [
        "paged-private-first",
        "paged-private-second",
        "paged-private-third",
    ] {
        let output = binary()
            .args(["headless", "--ledger"])
            .arg(&path)
            .args(["--input", input])
            .output()
            .expect("run paged headless command");
        assert!(output.status.success(), "{output:?}");
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis()
        .saturating_add(1)
        .to_string();

    let summary = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str(), "--summary-only"])
        .output()
        .expect("run summary-only stats command");
    assert!(summary.status.success(), "{summary:?}");
    let summary_text = String::from_utf8(summary.stdout).expect("summary stats UTF-8");
    assert!(!summary_text.contains("paged-private"), "{summary_text}");
    let summary_json: serde_json::Value =
        serde_json::from_str(&summary_text).expect("summary stats JSON");
    assert_eq!(summary_json["summary"]["total"]["attempts"], 3);
    assert_eq!(summary_json["page"], serde_json::Value::Null);

    let first = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str(), "--limit", "2"])
        .output()
        .expect("run first paged stats command");
    assert!(first.status.success(), "{first:?}");
    let first_json: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("first page stats JSON");
    assert_eq!(
        first_json["page"]["attempts"].as_array().map(Vec::len),
        Some(2)
    );
    let cursor = first_json["page"]["next_cursor"]
        .as_str()
        .expect("next page cursor");

    let second = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str(), "--limit", "2", "--cursor", cursor])
        .output()
        .expect("run second paged stats command");
    assert!(second.status.success(), "{second:?}");
    let second_json: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("second page stats JSON");
    assert_eq!(
        second_json["page"]["attempts"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(second_json["page"]["next_cursor"], serde_json::Value::Null);

    let appended = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "paged-private-fourth"])
        .output()
        .expect("append another usage attempt");
    assert!(appended.status.success(), "{appended:?}");
    let stale = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str(), "--limit", "2", "--cursor", cursor])
        .output()
        .expect("run stale paged stats command");
    assert!(!stale.status.success(), "{stale:?}");
    let stale_error = String::from_utf8_lossy(&stale.stderr);
    assert!(
        stale_error.contains("stale Ledger revision"),
        "{stale_error}"
    );
    fs::remove_file(path).expect("cleanup Runtime ledger");
}

#[test]
fn stats_reports_the_frozen_payg_estimate_without_user_text() {
    let path = temp_path("priced-stats");
    let private_input = "priced-private-input-marker";
    let mut runtime = RuntimeKernel::open(&path).expect("open priced Runtime");
    let mut provider = CompleteUsageProvider;
    let prepared = runtime
        .execute_with_observability(
            &ConfigLayers::default(),
            Vec::new(),
            simulator_price_book(),
            private_input,
            &mut provider,
        )
        .expect("execute priced Turn");
    runtime
        .acknowledge(prepared.delivery())
        .expect("acknowledge priced output");
    drop(runtime);

    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis()
        .saturating_add(1)
        .to_string();
    let stats = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str()])
        .output()
        .expect("run priced stats command");
    assert!(stats.status.success(), "{stats:?}");
    let text = String::from_utf8(stats.stdout).expect("priced stats UTF-8");
    assert!(!text.contains(private_input), "{text}");
    let document: serde_json::Value = serde_json::from_str(&text).expect("priced stats JSON");
    assert_eq!(document["attempts"][0]["cost_provenance"], "price_schedule");
    assert_eq!(
        document["attempts"][0]["payg_cost_estimate"]["schedule"]["version"],
        "2026-08-10"
    );
    assert_eq!(
        document["attempts"][0]["payg_cost_estimate"]["schedule"]["currency"],
        "USD"
    );
    assert_eq!(
        document["attempts"][0]["payg_cost_estimate"]["amount_pico_units"],
        202
    );
    assert_eq!(
        document["thread"]["usage"]["payg_cost_estimates"]["USD"]["exact_pico_units"],
        202
    );
    assert_eq!(document["thread"]["usage"]["cost_unknown_attempts"], 0);
    fs::remove_file(path).expect("cleanup priced Runtime ledger");
}

#[test]
fn headless_local_echo_mode_uses_the_product_driver_and_returns_ready() {
    let path = temp_path("product-driver");
    let output = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--tool", "local.echo", "--input", "hello"])
        .output()
        .expect("run Product driver command");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"simulated: hello\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("team-operation-committed"),
        "{output:?}"
    );

    let status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("run status command");
    assert!(status.status.success(), "{status:?}");
    assert_eq!(status.stdout, b"ready\n");

    let replayed = binary()
        .args(["headless", "--ledger"])
        .arg(&path)
        .args(["--input", "again"])
        .output()
        .expect("reopen Product driver from sidecar state");
    assert!(replayed.status.success(), "{replayed:?}");
    assert_eq!(replayed.stdout, b"simulated: again\n");
    assert!(
        !String::from_utf8_lossy(&replayed.stderr).contains("team-operation-committed"),
        "replayed Team receipt must not be emitted twice: {replayed:?}"
    );

    let replayed_status = binary()
        .args(["status", "--ledger"])
        .arg(&path)
        .output()
        .expect("inspect replayed Product driver");
    assert!(replayed_status.status.success(), "{replayed_status:?}");
    assert_eq!(replayed_status.stdout, b"ready\n");

    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis()
        .saturating_add(1)
        .to_string();
    let stats = binary()
        .args(["stats", "--ledger"])
        .arg(&path)
        .args(["--at", at.as_str()])
        .output()
        .expect("inspect Product driver usage");
    assert!(stats.status.success(), "{stats:?}");
    let document: serde_json::Value = serde_json::from_slice(&stats.stdout).expect("stats JSON");
    assert_eq!(document["attempts"].as_array().map(Vec::len), Some(2));
    assert_eq!(document["turns"].as_array().map(Vec::len), Some(2));
    assert_eq!(document["agents"].as_array().map(Vec::len), Some(1));
    assert_eq!(document["agents"][0]["usage"]["attempts"], 2);
    assert_eq!(document["team"]["attempts"], 2);
    fs::remove_file(&path).expect("cleanup Runtime ledger");
    fs::remove_file(sidecar(&path, "team")).expect("cleanup Team ledger");
    fs::remove_file(sidecar(&path, "tool")).expect("cleanup Tool ledger");
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

#[test]
fn headless_resolves_an_explicit_model_preset_without_model_name_inference() {
    let ledger = temp_path("model-preset");
    let config_root = temp_path("model-preset-config");
    let config_path = user_config_path(&config_root);
    fs::create_dir_all(config_path.parent().unwrap()).expect("create config parent");
    fs::write(
        &config_path,
        r#"schema_version = 1

[providers.edge]
template = "openai-compatible"
credential = "edge-credential"
base_url = "https://provider.invalid/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"

[providers.edge.pricing]
source = "unknown"

[model_presets.frontier]
provider = "edge"
model = "fixture-model"
dialect = "responses"
max_output_tokens = 2048
"#,
    )
    .expect("write Model Preset config");

    let output = binary_with_config_root(&config_root)
        .args(["headless", "--preset", "frontier", "--ledger"])
        .arg(&ledger)
        .args(["--input", "select the exact preset"])
        .output()
        .expect("run preset-selected headless command");
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Provider credential binding was not found")
            || stderr.contains("provider unavailable"),
        "{output:?}"
    );
    assert!(!stderr.contains("unknown option"), "{output:?}");
    assert!(
        !ledger.exists(),
        "Preset failure must precede Turn admission"
    );

    let missing = binary_with_config_root(&config_root)
        .args(["headless", "--preset", "missing", "--ledger"])
        .arg(&ledger)
        .args(["--input", "do not infer from model"])
        .output()
        .expect("run missing Preset command");
    assert!(!missing.status.success(), "{missing:?}");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("model_presets.missing"),
        "{missing:?}"
    );
    assert!(!ledger.exists());

    fs::remove_dir_all(config_root).expect("cleanup config root");
}

struct PanicProvider;

impl ProviderRuntime for PanicProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        panic!("injected crash after admission")
    }
}

struct CompleteUsageProvider;

impl ProviderRuntime for CompleteUsageProvider {
    fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        Ok(vec![
            ProviderEvent::TextDelta("priced".to_owned()),
            ProviderEvent::Completed(
                UsageRecord::new(
                    Some(100),
                    Some(10),
                    Some(5),
                    Some(20),
                    Some(2),
                    Some(120),
                    None,
                )
                .expect("valid complete Usage Record"),
            ),
        ])
    }
}

fn simulator_price_book() -> PriceScheduleBook {
    PriceScheduleBook::new(vec![
        PriceSchedule::new(PriceScheduleDefinition {
            id: "synthetic-simulator-price".to_owned(),
            version: "2026-08-10".to_owned(),
            currency: "USD".to_owned(),
            provider_profile: "simulator".to_owned(),
            model: "deterministic-v1".to_owned(),
            dialect: None,
            service_tier: None,
            minimum_context_tokens: 0,
            maximum_context_tokens: None,
            effective_from: UsageTimestamp::from_unix_millis(0).expect("valid schedule start"),
            effective_until: None,
            source: PriceScheduleSource::Manual,
            source_ref: "synthetic-price-source".to_owned(),
            rates: TokenRates::new(1, 2, 3, 4, 5),
        })
        .expect("valid synthetic Price Schedule"),
    ])
    .expect("valid synthetic Price Schedule book")
}

fn sidecar(path: &Path, kind: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_owned();
    sidecar.push(".");
    sidecar.push(kind);
    PathBuf::from(sidecar)
}
