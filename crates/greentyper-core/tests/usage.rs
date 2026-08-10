use greentyper_core::config::{
    ConfigDocument, ConfigEpoch, ConfigError, ConfigLayers, ConfigPaths, ConfigRuntime,
    ReasoningEffort, ServiceTier,
};
use greentyper_core::ledger::FileLedger;
use greentyper_core::model::ConfigEpochId;
use greentyper_core::pricing::{
    PriceSchedule, PriceScheduleBook, PriceScheduleDefinition, PriceScheduleSource, TokenRates,
};
use greentyper_core::provider::{
    DeterministicProvider, ProviderDialect, ProviderError, ProviderEvent, ProviderProfileSnapshot,
    ProviderRequest, ProviderRuntime, UsageRecord,
};
use greentyper_core::runtime::{RUNTIME_EVENT_SCHEMA, RuntimeError, RuntimeKernel};
use greentyper_core::usage::{
    MAX_USAGE_PAGE_SIZE, MAX_USAGE_WINDOWS, RuntimeUsageQuery, UsageCursor, UsageError,
    UsageTimestamp, UsageTimezoneSource, UsageWeekday, UsageWindow,
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

fn simulator_price_book(version: &str, input_rate: u64) -> PriceScheduleBook {
    PriceScheduleBook::new(vec![
        PriceSchedule::new(PriceScheduleDefinition {
            id: "simulator-standard".to_owned(),
            version: version.to_owned(),
            currency: "USD".to_owned(),
            provider_profile: "simulator".to_owned(),
            model: "deterministic-v1".to_owned(),
            dialect: None,
            service_tier: None,
            minimum_context_tokens: 0,
            maximum_context_tokens: None,
            effective_from: UsageTimestamp::from_unix_millis(0).unwrap(),
            effective_until: None,
            source: PriceScheduleSource::Manual,
            source_ref: "synthetic-runtime-price-fixture".to_owned(),
            rates: TokenRates::new(input_rate, 2, 3, 4, 5),
        })
        .unwrap(),
    ])
    .unwrap()
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
                .unwrap(),
            ),
        ])
    }
}

struct ProfiledCompleteUsageProvider {
    profile: ProviderProfileSnapshot,
}

impl ProviderRuntime for ProfiledCompleteUsageProvider {
    fn profile_snapshot(&self) -> Option<&ProviderProfileSnapshot> {
        Some(&self.profile)
    }

    fn dialect(&self) -> Option<ProviderDialect> {
        Some(ProviderDialect::Responses)
    }

    fn run(&mut self, request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        let mut provider = CompleteUsageProvider;
        provider.run(request)
    }
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
fn config_epoch_freezes_price_schedules_and_binds_them_to_its_fingerprint() {
    let id = ConfigEpochId::new(1).unwrap();
    let plain = ConfigEpoch::freeze(id, &ConfigLayers::default()).unwrap();
    let book = simulator_price_book("2026-08-10", 1);
    let frozen = ConfigEpoch::freeze_with_observability(
        id,
        &ConfigLayers::default(),
        Vec::new(),
        book.clone(),
    )
    .unwrap();
    assert_eq!(frozen.price_schedules(), &book);
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
    assert_eq!(RUNTIME_EVENT_SCHEMA, 8);
    let temp = TempConfig::new();
    let ledger = temp.root.join("runtime.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let mut provider = DeterministicProvider::default();
    let mut layers = ConfigLayers::default();
    layers.cli.reasoning_effort = Some(ReasoningEffort::High);
    layers.cli.service_tier = Some(ServiceTier::Priority);
    let output = runtime
        .execute_with_usage_windows(&layers, Vec::new(), "count this", &mut provider)
        .unwrap();
    runtime.acknowledge(output.delivery()).unwrap();
    drop(runtime);

    let snapshot = RuntimeKernel::inspect_usage(&ledger, UsageTimestamp::now().unwrap()).unwrap();
    assert_eq!(snapshot.attempts().len(), 1);
    let attempt = &snapshot.attempts()[0];
    assert_eq!(attempt.provider_profile(), "simulator");
    assert_eq!(attempt.requested_model(), "deterministic-v1");
    assert_eq!(attempt.requested_reasoning_effort(), Some("high"));
    assert_eq!(attempt.requested_service_tier(), Some("priority"));
    assert!(attempt.started_at().is_some());
    assert!(attempt.completed_at() >= attempt.started_at());
    let total = snapshot.thread().unwrap().usage();
    assert_eq!(total.attempts(), 1);
    assert_eq!(total.input_tokens().exact(), Some(0));
    assert!(total.input_tokens().estimated().unwrap() > 0);
}

#[test]
fn runtime_appends_usage_before_immutable_cost_and_rebuilds_the_same_stats() {
    let temp = TempConfig::new();
    let ledger = temp.root.join("priced-runtime.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let mut provider = CompleteUsageProvider;
    let output = runtime
        .execute_with_observability(
            &ConfigLayers::default(),
            Vec::new(),
            simulator_price_book("2026-08-10", 1),
            "price this",
            &mut provider,
        )
        .unwrap();
    runtime.acknowledge(output.delivery()).unwrap();
    drop(runtime);

    let report = FileLedger::inspect(&ledger).unwrap();
    assert!(report.events.windows(2).any(|events| {
        events[0].data.kind == 12
            && events[1].data.kind == 13
            && events[0].transaction == events[1].transaction
    }));
    let snapshot = RuntimeKernel::inspect_usage(&ledger, UsageTimestamp::now().unwrap()).unwrap();
    let attempt = &snapshot.attempts()[0];
    let estimate = attempt.cost_estimate().expect("frozen Cost Estimate");
    assert_eq!(estimate.schedule().version(), "2026-08-10");
    assert_eq!(estimate.currency(), "USD");
    assert_eq!(attempt.cost_provenance().as_str(), "price_schedule");
    let costs = snapshot.thread().unwrap().usage().payg_cost_estimates();
    assert_eq!(costs["USD"].exact_pico_units(), Some(202));
    assert_eq!(costs["USD"].scale_decimal_places(), 12);
    assert_eq!(
        snapshot.thread().unwrap().usage().cost_unknown_attempts(),
        0
    );
}

#[test]
fn resolved_config_price_schedule_flows_into_the_frozen_runtime_epoch() {
    let temp = TempConfig::new();
    fs::write(
        temp.root.join("user.toml"),
        r#"schema_version = 1

[provider]
profile = "openai-main"
model = "gpt-5.6-sol"

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.openai-main.pricing]
source = "manual"

[price_schedules.openai-sol]
version = "2026-08-10.1"
currency = "USD"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
minimum_context_tokens = 0
effective_from = "1970-01-01T00:00:00Z"
source = "manual"
source_ref = "synthetic-manual-rate-card"

[price_schedules.openai-sol.rates]
input_micros_per_million = 1
cached_input_micros_per_million = 2
cache_write_micros_per_million = 3
output_micros_per_million = 4
reasoning_output_micros_per_million = 5
"#,
    )
    .unwrap();
    fs::write(temp.root.join("project.toml"), "schema_version = 1\n").unwrap();
    let config = ConfigRuntime::open(temp.paths(), ConfigDocument::empty()).unwrap();
    let mut provider = ProfiledCompleteUsageProvider {
        profile: config.selected_provider_profile().unwrap().unwrap(),
    };
    let ledger = temp.root.join("configured-price.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let output = runtime
        .execute_with_observability(
            config.config_layers().unwrap(),
            config.resolved_usage_windows().unwrap(),
            config.resolved_price_schedules().unwrap(),
            "priced from Config Runtime",
            &mut provider,
        )
        .unwrap();
    runtime.acknowledge(output.delivery()).unwrap();
    let snapshot = runtime.usage_snapshot(UsageTimestamp::now().unwrap());
    assert_eq!(
        snapshot.attempts()[0]
            .cost_estimate()
            .unwrap()
            .schedule()
            .id(),
        "openai-sol"
    );
}

#[test]
fn missing_runtime_has_empty_usage_views_at_the_requested_instant() {
    let temp = TempConfig::new();
    let ledger = temp.root.join("missing.ledger");
    let at = UsageTimestamp::from_unix_millis(123_456_789).unwrap();
    let snapshot = RuntimeKernel::inspect_usage(&ledger, at).unwrap();
    assert_eq!(snapshot.as_of(), at);
    assert!(snapshot.attempts().is_empty());
    assert!(snapshot.thread().is_none());
    assert!(!ledger.exists());

    let report = RuntimeKernel::inspect_usage_report(
        &ledger,
        at,
        RuntimeUsageQuery::page(10, None).unwrap(),
    )
    .unwrap();
    assert_eq!(report.revision().transaction(), 0);
    assert_eq!(report.revision().sequence(), 0);
    assert_eq!(report.summary().total().attempts(), 0);
    assert!(report.page().unwrap().attempts().is_empty());
    assert!(report.page().unwrap().next_cursor().is_none());
    assert!(!ledger.exists());
}

#[test]
fn usage_report_pages_attempts_and_rejects_stale_revision_cursors() {
    let temp = TempConfig::new();
    let ledger = temp.root.join("paged-runtime.ledger");
    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let mut provider = DeterministicProvider::default();
    for input in ["first", "second", "third"] {
        let output = runtime
            .execute_with_usage_windows(&ConfigLayers::default(), Vec::new(), input, &mut provider)
            .unwrap();
        runtime.acknowledge(output.delivery()).unwrap();
    }
    drop(runtime);

    let at = UsageTimestamp::from_unix_millis(2_000_000_000_000).unwrap();
    let summary =
        RuntimeKernel::inspect_usage_report(&ledger, at, RuntimeUsageQuery::summary_only())
            .unwrap();
    assert_eq!(summary.summary().total().attempts(), 3);
    assert!(summary.page().is_none());

    let first =
        RuntimeKernel::inspect_usage_report(&ledger, at, RuntimeUsageQuery::page(2, None).unwrap())
            .unwrap();
    assert_eq!(
        first
            .page()
            .unwrap()
            .attempts()
            .iter()
            .map(|attempt| attempt.turn())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let cursor = first
        .page()
        .unwrap()
        .next_cursor()
        .expect("next page cursor")
        .clone();
    let different_instant = UsageTimestamp::from_unix_millis(at.unix_millis() + 1).unwrap();
    assert!(matches!(
        RuntimeKernel::inspect_usage_report(
            &ledger,
            different_instant,
            RuntimeUsageQuery::page(2, Some(cursor.clone())).unwrap(),
        ),
        Err(RuntimeError::Usage(UsageError::CursorQueryMismatch))
    ));
    let mut corrupted = cursor.to_string();
    let replacement = if corrupted.ends_with('0') { '1' } else { '0' };
    corrupted.pop();
    corrupted.push(replacement);
    assert!(matches!(
        corrupted.parse::<UsageCursor>(),
        Err(UsageError::InvalidCursor)
    ));
    let second = RuntimeKernel::inspect_usage_report(
        &ledger,
        at,
        RuntimeUsageQuery::page(2, Some(cursor.clone())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        second
            .page()
            .unwrap()
            .attempts()
            .iter()
            .map(|attempt| attempt.turn())
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert!(second.page().unwrap().next_cursor().is_none());

    let mut runtime = RuntimeKernel::open(&ledger).unwrap();
    let output = runtime
        .execute_with_usage_windows(
            &ConfigLayers::default(),
            Vec::new(),
            "fourth",
            &mut provider,
        )
        .unwrap();
    runtime.acknowledge(output.delivery()).unwrap();
    drop(runtime);
    assert!(matches!(
        RuntimeKernel::inspect_usage_report(
            &ledger,
            at,
            RuntimeUsageQuery::page(2, Some(cursor)).unwrap(),
        ),
        Err(RuntimeError::Usage(UsageError::StaleCursor))
    ));
    assert!(matches!(
        RuntimeUsageQuery::page(0, None),
        Err(UsageError::InvalidPageSize)
    ));
    assert!(matches!(
        RuntimeUsageQuery::page(MAX_USAGE_PAGE_SIZE + 1, None),
        Err(UsageError::InvalidPageSize)
    ));
    assert!(RuntimeUsageQuery::page(MAX_USAGE_PAGE_SIZE, None).is_ok());
    assert!(matches!(
        "非ascii".parse::<UsageCursor>(),
        Err(UsageError::InvalidCursor)
    ));
    assert!(matches!(
        "x".repeat(161).parse::<UsageCursor>(),
        Err(UsageError::InvalidCursor)
    ));
}
