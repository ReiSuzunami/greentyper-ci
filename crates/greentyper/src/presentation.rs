//! Terminal-neutral product presentation model.

use std::error::Error;
use std::fmt;

use greentyper_core::agent_team::{TaskStatus, TeamOperationStatus};
use greentyper_core::config::{
    CommandMatchKind, CommandQueryError, CommandTarget, ConfigErrorCategory, ConfigRuntimeStatus,
    ConfigScope, ModelPresetView, match_command_paths,
};
use greentyper_core::ledger::LedgerHead;
use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};
use greentyper_core::tool_runtime::{ToolCallStatus, ToolSnapshot};
use greentyper_core::usage::{RuntimeUsageSnapshot, UsageQuantity, UsageRollup};
use serde::Serialize;

const MAX_SLASH_RESULTS: usize = 12;
const MAX_MODEL_QUERY_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub(crate) enum Availability<T> {
    Known(T),
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SlashEntryView {
    canonical: &'static str,
    target: CommandTarget,
    match_kind: CommandMatchKind,
    root_visible: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SlashPanelView {
    query: String,
    entries: Vec<SlashEntryView>,
    selected: Option<usize>,
}

impl SlashPanelView {
    pub(crate) fn build(query: &str, selected: usize) -> Result<Self, CommandQueryError> {
        let query = if query.trim().is_empty() { "/" } else { query };
        let entries = match_command_paths(query)?
            .into_iter()
            .take(MAX_SLASH_RESULTS)
            .map(|matched| SlashEntryView {
                canonical: matched.path().canonical(),
                target: matched.path().target(),
                match_kind: matched.kind(),
                root_visible: matched.path().root_visible(),
            })
            .collect::<Vec<_>>();
        let selected = (!entries.is_empty()).then(|| selected.min(entries.len() - 1));
        Ok(Self {
            query: query.trim().to_owned(),
            entries,
            selected,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum RecoveryBadge {
    Ready,
    ResumeRequired { turn: u64 },
    ReconciliationRequired { turn: u64, delivery: u64 },
    Blocked { turn: u64, reason: String },
}

impl From<&RecoveryStatus> for RecoveryBadge {
    fn from(status: &RecoveryStatus) -> Self {
        match status {
            RecoveryStatus::Ready => Self::Ready,
            RecoveryStatus::ResumeRequired { turn } => Self::ResumeRequired { turn: turn.get() },
            RecoveryStatus::ReconciliationRequired { turn, delivery } => {
                Self::ReconciliationRequired {
                    turn: turn.get(),
                    delivery: delivery.get(),
                }
            }
            RecoveryStatus::Blocked { turn, reason } => Self::Blocked {
                turn: turn.get(),
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct UsageQuantityView {
    exact: Option<u64>,
    estimated: Option<u64>,
    unknown_records: u64,
}

impl From<&UsageQuantity> for UsageQuantityView {
    fn from(quantity: &UsageQuantity) -> Self {
        Self {
            exact: quantity.exact(),
            estimated: quantity.estimated(),
            unknown_records: quantity.unknown_records(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct UsageSummaryView {
    attempts: u64,
    total_tokens: UsageQuantityView,
    cost_unknown_attempts: u64,
}

impl From<&UsageRollup> for UsageSummaryView {
    fn from(rollup: &UsageRollup) -> Self {
        Self {
            attempts: rollup.attempts(),
            total_tokens: rollup.total_tokens().into(),
            cost_unknown_attempts: rollup.cost_unknown_attempts(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StatuslineView {
    recovery: RecoveryBadge,
    provider_profile: Availability<String>,
    model: Availability<String>,
    context_pressure_percent: Availability<u8>,
    one_hour_usage: Availability<UsageSummaryView>,
    thread: Option<u64>,
    item_count: usize,
    active_agents: Availability<usize>,
    blocker_count: Availability<usize>,
    config_ready: bool,
    recovered_tail_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BlockerView {
    RuntimeResume {
        turn: u64,
    },
    RuntimeReconciliation {
        turn: u64,
        delivery: u64,
    },
    RuntimeBlocked {
        turn: u64,
        reason: String,
    },
    TeamOperationAwaitingAcknowledgement {
        operation: u64,
    },
    TaskBlocked {
        task: u64,
        blocked_by: u64,
    },
    TaskFailed {
        task: u64,
        reason: String,
    },
    TaskCancelled {
        task: u64,
        reason: String,
    },
    ToolApproval {
        call: u64,
        agent: u64,
        tool: String,
        expires_at_unix_ms: Option<u64>,
    },
    ToolReconciliation {
        call: u64,
        agent: u64,
        tool: String,
        reason: Option<String>,
    },
    ConfigRepair {
        scope: ConfigScope,
        category: ConfigErrorCategory,
        backup_available: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModelSelectorEntryView {
    preset: ModelPresetView,
    compatibility: Availability<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModelSelectorView {
    favorites: Vec<ModelSelectorEntryView>,
    recent: Availability<Vec<ModelSelectorEntryView>>,
    compatible: Availability<Vec<ModelSelectorEntryView>>,
    all: Vec<ModelSelectorEntryView>,
}

impl ModelSelectorView {
    pub(crate) fn build(
        presets: &[ModelPresetView],
        query: &str,
    ) -> Result<Self, PresentationError> {
        if query.len() > MAX_MODEL_QUERY_BYTES
            || query.chars().any(|character| character.is_control())
        {
            return Err(PresentationError::InvalidModelQuery);
        }
        let tokens = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let all = presets
            .iter()
            .filter(|preset| model_matches(preset, &tokens))
            .cloned()
            .map(|preset| ModelSelectorEntryView {
                preset,
                compatibility: Availability::Unknown,
            })
            .collect::<Vec<_>>();
        let favorites = all
            .iter()
            .filter(|entry| entry.preset.favorite)
            .cloned()
            .collect();
        Ok(Self {
            favorites,
            recent: Availability::Unknown,
            compatible: Availability::Unknown,
            all,
        })
    }
}

fn model_matches(preset: &ModelPresetView, tokens: &[String]) -> bool {
    let dialect = preset.dialect.as_str();
    let fields = [
        Some(preset.id.as_str()),
        Some(preset.provider.as_str()),
        Some(preset.model.as_str()),
        Some(dialect),
        preset.reasoning_effort.as_deref(),
        preset.service_tier.as_deref(),
        preset.context_mode.as_deref(),
    ];
    tokens.iter().all(|token| {
        fields.iter().flatten().any(|field| {
            let field = field.to_ascii_lowercase();
            field.contains(token) || is_model_subsequence(token, &field)
        })
    })
}

fn is_model_subsequence(query: &str, candidate: &str) -> bool {
    let mut query = query.bytes();
    let mut next = query.next();
    for candidate in candidate.bytes() {
        if next == Some(candidate) {
            next = query.next();
        }
    }
    next.is_none()
}

pub(crate) struct PresentationSources<'a> {
    pub(crate) runtime: &'a RuntimeSnapshot,
    pub(crate) usage: Option<&'a RuntimeUsageSnapshot>,
    pub(crate) team: Option<&'a KernelTeamSnapshot>,
    pub(crate) tools: Option<&'a ToolSnapshot>,
    pub(crate) config: &'a ConfigRuntimeStatus,
    pub(crate) provider_profile: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) context_pressure_percent: Option<u8>,
    pub(crate) model_presets: &'a [ModelPresetView],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TuiViewModel {
    pub(crate) slash: SlashPanelView,
    pub(crate) statusline: StatuslineView,
    pub(crate) blockers: Vec<BlockerView>,
    pub(crate) models: ModelSelectorView,
}

impl TuiViewModel {
    pub(crate) fn build(
        slash_query: &str,
        model_query: &str,
        selected: usize,
        sources: PresentationSources<'_>,
    ) -> Result<Self, PresentationError> {
        if sources
            .context_pressure_percent
            .is_some_and(|value| value > 100)
        {
            return Err(PresentationError::InvalidContextPressure);
        }
        let slash = SlashPanelView::build(slash_query, selected)?;
        let models = ModelSelectorView::build(sources.model_presets, model_query)?;
        let blockers = build_blockers(&sources);
        let blocker_count = if sources.team.is_some() && sources.tools.is_some() {
            Availability::Known(blockers.len())
        } else {
            Availability::Unknown
        };
        let statusline = StatuslineView {
            recovery: (&sources.runtime.status).into(),
            provider_profile: availability(sources.provider_profile),
            model: availability(sources.model),
            context_pressure_percent: sources
                .context_pressure_percent
                .map_or(Availability::Unknown, Availability::Known),
            one_hour_usage: sources.usage.map_or(Availability::Unknown, |usage| {
                Availability::Known(usage.rolling().one_hour().into())
            }),
            thread: sources.runtime.thread.map(|thread| thread.get()),
            item_count: sources.runtime.items.len(),
            active_agents: sources.team.map_or(Availability::Unknown, |team| {
                Availability::Known(team.projection.active_agent_count())
            }),
            blocker_count,
            config_ready: sources.config.ready,
            recovered_tail_bytes: sources.runtime.recovered_tail_bytes,
        };
        Ok(Self {
            slash,
            statusline,
            blockers,
            models,
        })
    }
}

pub(crate) fn build_smoke_view(query: &str) -> Result<TuiViewModel, PresentationSmokeError> {
    let runtime = RuntimeSnapshot {
        head: LedgerHead::default(),
        thread: None,
        items: Vec::new(),
        status: RecoveryStatus::Ready,
        recovered_tail_bytes: 0,
    };
    let status = ConfigRuntimeStatus {
        ready: true,
        issues: Vec::new(),
    };
    TuiViewModel::build(
        query,
        "",
        0,
        PresentationSources {
            runtime: &runtime,
            usage: None,
            team: None,
            tools: None,
            config: &status,
            provider_profile: Some("simulator"),
            model: Some("deterministic-v1"),
            context_pressure_percent: None,
            model_presets: &[],
        },
    )
    .map_err(Into::into)
}

fn availability(value: Option<&str>) -> Availability<String> {
    value.map_or(Availability::Unknown, |value| {
        Availability::Known(value.to_owned())
    })
}

fn build_blockers(sources: &PresentationSources<'_>) -> Vec<BlockerView> {
    let mut blockers = Vec::new();
    match &sources.runtime.status {
        RecoveryStatus::Ready => {}
        RecoveryStatus::ResumeRequired { turn } => {
            blockers.push(BlockerView::RuntimeResume { turn: turn.get() });
        }
        RecoveryStatus::ReconciliationRequired { turn, delivery } => {
            blockers.push(BlockerView::RuntimeReconciliation {
                turn: turn.get(),
                delivery: delivery.get(),
            });
        }
        RecoveryStatus::Blocked { turn, reason } => {
            blockers.push(BlockerView::RuntimeBlocked {
                turn: turn.get(),
                reason: reason.clone(),
            });
        }
    }

    if let Some(team) = sources.team {
        blockers.extend(
            team.operations
                .iter()
                .filter(|operation| {
                    operation.status == TeamOperationStatus::CommittedAwaitingAcknowledgement
                })
                .map(
                    |operation| BlockerView::TeamOperationAwaitingAcknowledgement {
                        operation: operation.operation.get(),
                    },
                ),
        );
        blockers.extend(
            team.projection
                .tasks
                .iter()
                .filter_map(|task| match &task.status {
                    TaskStatus::Blocked { blocked_by } => Some(BlockerView::TaskBlocked {
                        task: task.id.get(),
                        blocked_by: blocked_by.get(),
                    }),
                    TaskStatus::Failed { reason } => Some(BlockerView::TaskFailed {
                        task: task.id.get(),
                        reason: reason.clone(),
                    }),
                    TaskStatus::Cancelled { reason } => Some(BlockerView::TaskCancelled {
                        task: task.id.get(),
                        reason: reason.clone(),
                    }),
                    _ => None,
                }),
        );
    }

    if let Some(tools) = sources.tools {
        blockers.extend(tools.calls.iter().filter_map(|record| match record.status {
            ToolCallStatus::AwaitingApproval => Some(BlockerView::ToolApproval {
                call: record.call.get(),
                agent: record.agent.get(),
                tool: record.tool.clone(),
                expires_at_unix_ms: record.approval_expires_at_unix_ms,
            }),
            ToolCallStatus::ReconciliationRequired => Some(BlockerView::ToolReconciliation {
                call: record.call.get(),
                agent: record.agent.get(),
                tool: record.tool.clone(),
                reason: record.reason.clone(),
            }),
            _ => None,
        }));
    }

    blockers.extend(
        sources
            .config
            .issues
            .iter()
            .map(|issue| BlockerView::ConfigRepair {
                scope: issue.scope,
                category: issue.category,
                backup_available: issue.backup_available,
            }),
    );
    blockers
}

#[derive(Debug)]
pub(crate) enum PresentationError {
    Command(CommandQueryError),
    InvalidContextPressure,
    InvalidModelQuery,
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(source) => write!(formatter, "{source}"),
            Self::InvalidContextPressure => {
                formatter.write_str("context pressure must be between 0 and 100")
            }
            Self::InvalidModelQuery => formatter.write_str("model query is invalid"),
        }
    }
}

impl Error for PresentationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::InvalidContextPressure | Self::InvalidModelQuery => None,
        }
    }
}

impl From<CommandQueryError> for PresentationError {
    fn from(source: CommandQueryError) -> Self {
        Self::Command(source)
    }
}

#[derive(Debug)]
pub(crate) enum PresentationSmokeError {
    Presentation(PresentationError),
}

impl fmt::Display for PresentationSmokeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Presentation(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for PresentationSmokeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Presentation(source) => Some(source),
        }
    }
}

impl From<PresentationError> for PresentationSmokeError {
    fn from(source: PresentationError) -> Self {
        Self::Presentation(source)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use greentyper_core::agent_team::{
        EventSeq, TaskId, TaskScope, TaskStatus, TaskView, TeamOperationId, TeamOperationRecord,
        TeamOperationStatus, TeamSnapshot, TransactionId,
    };
    use greentyper_core::config::{
        CommandMatchKind, CommandTarget, ConfigDocument, ConfigEditorError, ConfigEditorSession,
        ConfigErrorCategory, ConfigFieldContents, ConfigObjectKind, ConfigObjectRef, ConfigPaths,
        ConfigRepairIssue, ConfigRuntime, ConfigRuntimeError, ConfigRuntimeStatus, ConfigScope,
        ConfigValue, ModelPresetView,
    };
    use greentyper_core::ledger::LedgerHead;
    use greentyper_core::model::{DeliveryId, ThreadId, TurnId};
    use greentyper_core::provider::ProviderDialect;
    use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};

    use super::{
        Availability, BlockerView, PresentationSources, RecoveryBadge, SlashPanelView, TuiViewModel,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "greentyper-presentation-{name}-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).expect("create presentation test directory");
            Self { root }
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths::new(self.root.join("user.toml"), self.root.join("project.toml"))
        }

        fn open_provider_runtime(&self) -> ConfigRuntime {
            fs::write(
                self.paths().project(),
                r#"schema_version = 1

[providers.edge]
template = "openai-compatible"
credential = "synthetic-edge-credential-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"

[providers.edge.pricing]
source = "unknown"
"#,
            )
            .expect("write provider fixture");
            ConfigRuntime::open(self.paths(), ConfigDocument::empty()).expect("open Config Runtime")
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove presentation test directory");
        }
    }

    fn runtime(status: RecoveryStatus) -> RuntimeSnapshot {
        RuntimeSnapshot {
            head: LedgerHead::default(),
            thread: Some(ThreadId::new(7).expect("thread")),
            items: Vec::new(),
            status,
            recovered_tail_bytes: 0,
        }
    }

    #[test]
    fn slash_panel_is_bounded_ranked_and_clamps_selection() {
        let root = SlashPanelView::build("", 99).expect("root slash panel");
        assert_eq!(root.entries.len(), 4);
        assert_eq!(root.selected, Some(3));
        assert!(root.entries.iter().all(|entry| entry.root_visible));

        let url = SlashPanelView::build("/config pro url", 0).expect("URL route");
        assert_eq!(url.entries[0].canonical, "/config provider url");
        assert_eq!(url.entries[0].match_kind, CommandMatchKind::Prefix);
        assert!(matches!(
            url.entries[0].target,
            CommandTarget::ConfigEditor {
                path_pattern: "providers.<id>.base_url",
                ..
            }
        ));
    }

    #[test]
    fn config_editor_focuses_provider_url_previews_and_commits_one_draft() {
        let temp = TempTree::new("config-editor");
        let mut runtime = temp.open_provider_runtime();
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut editor = ConfigEditorSession::open_from_query(
            &runtime,
            ConfigScope::Project,
            "/config pro url",
            0,
            Some(&object),
        )
        .expect("open focused provider URL editor");

        let initial = editor.preview(&mut runtime).expect("initial preview");
        assert_eq!(initial.field.path, "providers.edge.base_url");
        assert_eq!(initial.field.command_path, "/config provider url");
        assert!(initial.changes.is_empty());

        editor
            .stage_raw("https://new-gateway.example.com/v2")
            .expect("stage provider URL");
        let preview = editor.preview(&mut runtime).expect("validated preview");
        assert_eq!(preview.changes.len(), 1);
        assert_eq!(preview.changes[0].path, "providers.edge.base_url");
        assert_eq!(
            preview.field.contents,
            ConfigFieldContents::Value {
                effective: Some(ConfigValue::String("https://gateway.example.com/v1".into())),
                source: Some(ConfigScope::Project),
                target: Some(ConfigValue::String(
                    "https://new-gateway.example.com/v2".into()
                )),
            }
        );
        editor.reset().expect("reset staged provider URL");
        assert_eq!(
            editor
                .preview(&mut runtime)
                .expect("reset preview")
                .field
                .contents,
            ConfigFieldContents::Value {
                effective: Some(ConfigValue::String("https://gateway.example.com/v1".into())),
                source: Some(ConfigScope::Project),
                target: None,
            }
        );
        editor
            .stage_raw("https://new-gateway.example.com/v2")
            .expect("restage provider URL");

        let commit = editor.commit(&mut runtime).expect("commit editor draft");
        assert!(commit.written);
        assert_eq!(commit.changes, preview.changes);
        assert_eq!(
            runtime
                .inspect_field(ConfigScope::Project, "providers.edge.base_url")
                .expect("inspect committed provider URL")
                .contents,
            ConfigFieldContents::Value {
                effective: Some(ConfigValue::String(
                    "https://new-gateway.example.com/v2".into()
                )),
                source: Some(ConfigScope::Project),
                target: Some(ConfigValue::String(
                    "https://new-gateway.example.com/v2".into()
                )),
            }
        );
    }

    #[test]
    fn config_editor_keeps_invalid_draft_recoverable_and_routes_credentials_safely() {
        let temp = TempTree::new("config-editor-validation");
        let mut runtime = temp.open_provider_runtime();
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut editor = ConfigEditorSession::open_from_query(
            &runtime,
            ConfigScope::Project,
            "/config provider url",
            0,
            Some(&object),
        )
        .expect("open provider URL editor");
        editor
            .stage_raw("http://provider.invalid/v1")
            .expect("stage syntactically typed URL");
        assert!(matches!(
            editor.preview(&mut runtime),
            Err(ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { path, .. }))
                if path == "providers.edge.base_url"
        ));
        assert!(matches!(
            editor.preview(&mut runtime),
            Err(ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { path, .. }))
                if path == "providers.edge.base_url"
        ));
        editor
            .stage_raw("https://recovered.example.com/v1")
            .expect("replace invalid staged URL");
        assert_eq!(
            editor
                .preview(&mut runtime)
                .expect("recovered preview")
                .changes
                .len(),
            1
        );

        let mut credential = ConfigEditorSession::open_from_query(
            &runtime,
            ConfigScope::Project,
            "/config provider credential",
            0,
            Some(&object),
        )
        .expect("open credential binding editor");
        let encoded = serde_json::to_string(
            &credential
                .preview(&mut runtime)
                .expect("credential binding preview"),
        )
        .expect("serialize credential binding preview");
        assert!(!encoded.contains("synthetic-edge-credential-reference"));
        assert!(!format!("{credential:?}").contains("synthetic-edge-credential-reference"));
        assert!(matches!(
            credential.stage_raw("synthetic-replacement-reference"),
            Err(ConfigEditorError::CredentialOperationRequired)
        ));
        assert!(matches!(
            credential.reset(),
            Err(ConfigEditorError::CredentialOperationRequired)
        ));
        assert!(matches!(
            credential.commit(&mut runtime),
            Err(ConfigEditorError::CredentialOperationRequired)
        ));
        assert!(matches!(
            ConfigEditorSession::open_from_query(
                &runtime,
                ConfigScope::Project,
                "/config",
                0,
                None,
            ),
            Err(ConfigEditorError::CommandTargetNotEditor)
        ));
        assert!(matches!(
            ConfigEditorSession::open_from_query(
                &runtime,
                ConfigScope::Project,
                "/config provider url",
                99,
                Some(&object),
            ),
            Err(ConfigEditorError::NoCommandMatch)
        ));
        assert!(matches!(
            ConfigEditorSession::open_from_query(
                &runtime,
                ConfigScope::Project,
                "/config provider url",
                0,
                None,
            ),
            Err(ConfigEditorError::ConfigObjectRequired)
        ));
    }

    #[test]
    fn config_editor_commit_detects_a_stale_revision_without_overwriting_the_winner() {
        let temp = TempTree::new("config-editor-race");
        let mut winner_runtime = temp.open_provider_runtime();
        let mut loser_runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
            .expect("open second runtime");
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut winner = ConfigEditorSession::open_from_query(
            &winner_runtime,
            ConfigScope::Project,
            "/config provider url",
            0,
            Some(&object),
        )
        .expect("open winner editor");
        let mut loser = ConfigEditorSession::open_from_query(
            &loser_runtime,
            ConfigScope::Project,
            "/config provider url",
            0,
            Some(&object),
        )
        .expect("open loser editor");
        winner
            .stage_raw("https://winner.example.com/v1")
            .expect("stage winner");
        loser
            .stage_raw("https://loser.example.com/v1")
            .expect("stage loser");
        winner.commit(&mut winner_runtime).expect("commit winner");

        assert!(matches!(
            loser.commit(&mut loser_runtime),
            Err(ConfigEditorError::Config(
                ConfigRuntimeError::RevisionConflict { .. }
            ))
        ));
        loser_runtime.reload().expect("reload winner state");
        assert_eq!(
            loser_runtime
                .inspect_field(ConfigScope::Project, "providers.edge.base_url")
                .expect("inspect winner")
                .contents,
            ConfigFieldContents::Value {
                effective: Some(ConfigValue::String("https://winner.example.com/v1".into())),
                source: Some(ConfigScope::Project),
                target: Some(ConfigValue::String("https://winner.example.com/v1".into())),
            }
        );
    }

    #[test]
    fn view_model_preserves_every_actionable_blocker() {
        let operation = TeamOperationRecord {
            operation: TeamOperationId::default(),
            transaction: TransactionId::default(),
            first_sequence: EventSeq::default(),
            last_sequence: EventSeq::default(),
            event_count: 1,
            acknowledgement_transaction: None,
            status: TeamOperationStatus::CommittedAwaitingAcknowledgement,
        };
        let task = TaskView {
            id: TaskId::default(),
            title: "blocked fixture".into(),
            scope: TaskScope::from_labels(["fixture"]),
            dependencies: Vec::new(),
            owner: None,
            status: TaskStatus::Blocked {
                blocked_by: TaskId::default(),
            },
            completion: None,
        };
        let team = KernelTeamSnapshot {
            projection: TeamSnapshot {
                revision: EventSeq::default(),
                tasks: vec![task],
                agents: Vec::new(),
                messages: Vec::new(),
            },
            ledger_head: LedgerHead::default(),
            recovered_tail_bytes: 0,
            operations: vec![operation],
        };
        let config = ConfigRuntimeStatus {
            ready: false,
            issues: vec![ConfigRepairIssue {
                scope: ConfigScope::Project,
                path: PathBuf::from("private-config-path"),
                category: ConfigErrorCategory::RepairRequired,
                detail: "private-config-detail".into(),
                backup_available: true,
            }],
        };
        let runtime = runtime(RecoveryStatus::ReconciliationRequired {
            turn: TurnId::new(2).expect("turn"),
            delivery: DeliveryId::new(3).expect("delivery"),
        });
        let view = TuiViewModel::build(
            "/",
            "",
            0,
            PresentationSources {
                runtime: &runtime,
                usage: None,
                team: Some(&team),
                tools: None,
                config: &config,
                provider_profile: None,
                model: None,
                context_pressure_percent: None,
                model_presets: &[],
            },
        )
        .expect("view model");

        assert_eq!(view.blockers.len(), 4);
        assert!(matches!(
            view.blockers[0],
            BlockerView::RuntimeReconciliation {
                turn: 2,
                delivery: 3
            }
        ));
        assert!(matches!(
            view.blockers[1],
            BlockerView::TeamOperationAwaitingAcknowledgement { .. }
        ));
        assert!(matches!(view.blockers[2], BlockerView::TaskBlocked { .. }));
        assert!(matches!(
            view.blockers[3],
            BlockerView::ConfigRepair {
                scope: ConfigScope::Project,
                backup_available: true,
                ..
            }
        ));
        let encoded = serde_json::to_string(&view).expect("serialize view model");
        assert!(!encoded.contains("private-config-path"));
        assert!(!encoded.contains("private-config-detail"));
    }

    #[test]
    fn statusline_keeps_unavailable_facts_unknown() {
        let runtime = runtime(RecoveryStatus::Blocked {
            turn: TurnId::new(9).expect("turn"),
            reason: "fixture blocker".into(),
        });
        let config = ConfigRuntimeStatus {
            ready: true,
            issues: Vec::new(),
        };
        let view = TuiViewModel::build(
            "/",
            "",
            0,
            PresentationSources {
                runtime: &runtime,
                usage: None,
                team: None,
                tools: None,
                config: &config,
                provider_profile: None,
                model: None,
                context_pressure_percent: None,
                model_presets: &[],
            },
        )
        .expect("view model");

        assert_eq!(
            &view.statusline.recovery,
            &RecoveryBadge::Blocked {
                turn: 9,
                reason: "fixture blocker".into(),
            }
        );
        assert_eq!(&view.statusline.provider_profile, &Availability::Unknown);
        assert_eq!(&view.statusline.model, &Availability::Unknown);
        assert_eq!(
            &view.statusline.context_pressure_percent,
            &Availability::Unknown
        );
        assert_eq!(&view.statusline.one_hour_usage, &Availability::Unknown);
        assert_eq!(view.statusline.active_agents, Availability::Unknown);
        assert_eq!(view.statusline.blocker_count, Availability::Unknown);
    }

    #[test]
    fn model_selector_filters_configured_presets_without_inventing_compatibility() {
        let presets = vec![
            ModelPresetView {
                id: "fast".into(),
                provider: "edge".into(),
                model: "fixture-model".into(),
                dialect: ProviderDialect::Responses,
                reasoning_effort: Some("medium".into()),
                service_tier: None,
                max_output_tokens: Some(4096),
                context_mode: None,
                favorite: true,
                fallback: Vec::new(),
            },
            ModelPresetView {
                id: "cheap".into(),
                provider: "edge".into(),
                model: "economy-model".into(),
                dialect: ProviderDialect::Responses,
                reasoning_effort: None,
                service_tier: None,
                max_output_tokens: None,
                context_mode: None,
                favorite: false,
                fallback: Vec::new(),
            },
        ];
        let runtime = runtime(RecoveryStatus::Ready);
        let config = ConfigRuntimeStatus {
            ready: true,
            issues: Vec::new(),
        };
        let selector = TuiViewModel::build(
            "/",
            "fst",
            0,
            PresentationSources {
                runtime: &runtime,
                usage: None,
                team: None,
                tools: None,
                config: &config,
                provider_profile: None,
                model: None,
                context_pressure_percent: None,
                model_presets: &presets,
            },
        )
        .expect("model selector")
        .models;
        assert_eq!(selector.all.len(), 1);
        assert_eq!(selector.all[0].preset.id, "fast");
        assert_eq!(selector.favorites.len(), 1);
        assert_eq!(selector.recent, Availability::Unknown);
        assert_eq!(selector.compatible, Availability::Unknown);
        assert_eq!(selector.all[0].compatibility, Availability::Unknown);
    }
}
