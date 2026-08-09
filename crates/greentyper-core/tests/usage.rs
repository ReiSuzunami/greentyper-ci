use greentyper_core::config::{
    ConfigDocument, ConfigEpoch, ConfigError, ConfigLayers, ConfigPaths, ConfigRuntime,
};
use greentyper_core::model::ConfigEpochId;
use greentyper_core::provider::DeterministicProvider;
use greentyper_core::runtime::{RUNTIME_EVENT_SCHEMA, RuntimeError, RuntimeKernel};
use greentyper_core::usage::{
    MAX_USAGE_WINDOWS, UsageTimestamp, UsageTimezoneSource, UsageWeekday, UsageWindow,
};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempConfig {
    root: std::path::PathBuf,
}

impl TempConfig {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "greentyper-usage-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(self.root.join("user.toml"), self.root.join("project.toml"))
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn timestamp(value: &str) -> UsageTimestamp {
    let parsed: jiff::Timestamp = value.parse().expect("valid fixture timestamp");
    UsageTimestamp::from_unix_millis(parsed.as_millisecond()).unwrap()
}

#[test]
fn usage_window_is_half_open_and_cross_midnight_uses_the_start_day() {
    let office = UsageWindow::resolve(
        "office",
        "09:00",
        "17:00",
        [UsageWeekday::Mon],
        "Asia/Hong_Kong",
    )
    .unwrap();
    assert!(office.contains(timestamp("2025-01-06T01:00:00Z")).unwrap());
    assert!(!office.contains(timestamp("2025-01-06T09:00:00Z")).unwrap());

    let overnight = UsageWindow::resolve(
        "overnight",
        "22:00",
        "02:00",
        [UsageWeekday::Mon],
        "Asia/Hong_Kong",
    )
    .unwrap();
    assert!(
        overnight
            .contains(timestamp("2025-01-06T14:00:00Z"))
            .unwrap()
    );
    assert!(
        overnight
            .contains(timestamp("2025-01-06T17:00:00Z"))
            .unwrap()
    );
    assert!(
        !overnight
            .contains(timestamp("2025-01-06T18:00:00Z"))
            .unwrap()
    );
}

#[test]
fn usage_window_handles_repeated_and_skipped_dst_hours_by_instant() {
    let repeated = UsageWindow::resolve(
        "repeated",
        "01:00",
        "02:00",
        [UsageWeekday::Sun],
        "America/New_York",
    )
    .unwrap();
    assert!(
        repeated
            .contains(timestamp("2025-11-02T05:30:00Z"))
            .unwrap()
    );
    assert!(
        repeated
            .contains(timestamp("2025-11-02T06:30:00Z"))
            .unwrap()
    );

    let skipped = UsageWindow::resolve(
        "skipped",
        "02:00",
        "03:00",
        [UsageWeekday::Sun],
        "America/New_York",
    )
    .unwrap();
    assert!(!skipped.contains(timestamp("2025-03-09T06:30:00Z")).unwrap());
    assert!(!skipped.contains(timestamp("2025-03-09T07:30:00Z")).unwrap());
}

#[test]
fn usage_window_rejects_empty_ranges_and_unpinned_timezones() {
    assert!(UsageTimestamp::from_unix_millis(i64::MAX).is_err());
    assert!(
        UsageWindow::resolve(
            "empty",
            "09:00",
            "09:00",
            [UsageWeekday::Mon],
            "Asia/Hong_Kong",
        )
        .is_err()
    );
    assert!(
        UsageWindow::resolve(
            "unknown",
            "09:00",
            "10:00",
            [UsageWeekday::Mon],
            "Mars/Olympus_Mons",
        )
        .is_err()
    );
}

#[test]
fn usage_window_resolves_local_to_a_pinned_iana_identity() {
    let window = UsageWindow::resolve(
        "local-workday",
        "09:00",
        "17:00",
        [UsageWeekday::Mon],
        "local",
    )
    .expect("resolve system time zone");
    assert_ne!(window.timezone(), "local");
    assert_eq!(window.timezone_source(), UsageTimezoneSource::LocalSystem);
    assert!(!window.ruleset_version().is_empty());
}

#[test]
fn config_runtime_resolves_usage_windows_to_pinned_snapshots() {
    let temp = TempConfig::new();
    fs::write(
        temp.root.join("user.toml"),
        r#"schema_version = 1

[[stats.windows]]
id = "workday"
start = "09:00"
end = "18:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "Asia/Hong_Kong"
"#,
    )
    .unwrap();
    fs::write(temp.root.join("project.toml"), "schema_version = 1\n").unwrap();
    let runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty()).unwrap();
    let windows = runtime.resolved_usage_windows().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].id(), "workday");
    assert_eq!(windows[0].timezone(), "Asia/Hong_Kong");
    assert!(!windows[0].ruleset_version().is_empty());
}

#[test]
fn config_epoch_freezes_usage_windows_and_binds_them_to_its_fingerprint() {
    let window = UsageWindow::resolve(
        "workday",
        "09:00",
        "18:00",
        [UsageWeekday::Mon],
        "Asia/Hong_Kong",
    )
    .unwrap();
    let id = ConfigEpochId::new(1).unwrap();
    let plain = ConfigEpoch::freeze(id, &ConfigLayers::default()).unwrap();
    let frozen =
        ConfigEpoch::freeze_with_usage_windows(id, &ConfigLayers::default(), vec![window.clone()])
            .unwrap();
    assert_eq!(frozen.usage_windows(), &[window]);
    assert_ne!(frozen.fingerprint(), plain.fingerprint());
}

#[test]
fn excessive_usage_windows_are_rejected_before_runtime_append() {
    let windows = (0..=MAX_USAGE_WINDOWS)
        .map(|index| {
            UsageWindow::resolve(
                format!("w{index}"),
                "09:00",
                "18:00",
                [UsageWeekday::Mon],
                "Etc/UTC",
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ConfigEpoch::freeze_with_usage_windows(
            ConfigEpochId::new(1).unwrap(),
            &ConfigLayers::default(),
            windows.clone(),
        ),
        Err(ConfigError::TooManyUsageWindows)
    );

    let temp = TempConfig::new();
    let ledger = temp.root.join("runtime.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let before = runtime.snapshot().head;
    let mut provider = DeterministicProvider::default();
    assert!(matches!(
        runtime.execute_with_usage_windows(
            &ConfigLayers::default(),
            windows,
            "must not append",
            &mut provider,
        ),
        Err(RuntimeError::Config(ConfigError::TooManyUsageWindows))
    ));
    assert_eq!(runtime.snapshot().head, before);
    drop(runtime);
    RuntimeKernel::open(&ledger).expect("reopen unchanged Runtime Ledger");
}

#[test]
fn runtime_persists_attempts_and_rebuilds_cached_rollups() {
    assert_eq!(RUNTIME_EVENT_SCHEMA, 4);
    let temp = TempConfig::new();
    let ledger = temp.root.join("runtime.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let mut provider = DeterministicProvider::default();
    let output = runtime
        .execute_with_usage_windows(
            &ConfigLayers::default(),
            Vec::new(),
            "count this",
            &mut provider,
        )
        .unwrap();
    runtime.acknowledge(output.delivery()).unwrap();
    drop(runtime);

    let snapshot = RuntimeKernel::inspect_usage(&ledger, UsageTimestamp::now().unwrap()).unwrap();
    assert_eq!(snapshot.attempts().len(), 1);
    let attempt = &snapshot.attempts()[0];
    assert_eq!(attempt.provider_profile(), "simulator");
    assert_eq!(attempt.requested_model(), "deterministic-v1");
    assert!(attempt.started_at().is_some());
    assert!(attempt.completed_at() >= attempt.started_at());
    let total = snapshot.thread().unwrap().usage();
    assert_eq!(total.attempts(), 1);
    assert_eq!(total.input_tokens().exact(), Some(0));
    assert!(total.input_tokens().estimated().unwrap() > 0);
}

#[test]
fn missing_runtime_has_an_empty_snapshot_at_the_requested_instant() {
    let temp = TempConfig::new();
    let ledger = temp.root.join("missing.ledger");
    let at = UsageTimestamp::from_unix_millis(123_456_789).unwrap();
    let snapshot = RuntimeKernel::inspect_usage(&ledger, at).unwrap();
    assert_eq!(snapshot.as_of(), at);
    assert!(snapshot.attempts().is_empty());
    assert!(snapshot.thread().is_none());
    assert!(!ledger.exists());
}
