//! Terminal-neutral product presentation model.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use greentyper_core::agent_team::{AgentStatus, TaskStatus, TeamOperationStatus};
use greentyper_core::config::{
    CommandMatchKind, CommandQueryError, CommandTarget, ConfigCommit, ConfigEditorError,
    ConfigEditorOperation, ConfigEditorSession, ConfigEditorView, ConfigErrorCategory,
    ConfigFieldContents, ConfigFieldInteraction, ConfigFieldView, ConfigObjectKind,
    ConfigObjectRef, ConfigRuntime, ConfigRuntimeError, ConfigRuntimeStatus, ConfigScope,
    ConfigSection, ConfigValue, ModelCatalogView, ModelPresetView, match_command_paths,
};
use greentyper_core::context::{ContextPressureAccuracy, ContextPressureSnapshot};
use greentyper_core::ledger::LedgerHead;
use greentyper_core::provider_catalog::{CatalogAvailability, ModelCapability};
use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};
use greentyper_core::tool_runtime::{ToolCallStatus, ToolSnapshot};
use greentyper_core::usage::{
    CostQuantity, RuntimeUsageSnapshot, UsageAttempt, UsageAttemptOutcome, UsageQuantity,
    UsageRollup,
};
use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::provider_connection::{ProviderConnectionTestStatus, ProviderConnectionTester};
use crate::provider_http::has_provider_adapter;

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
    overflowed: bool,
}

impl From<&UsageQuantity> for UsageQuantityView {
    fn from(quantity: &UsageQuantity) -> Self {
        Self {
            exact: quantity.exact(),
            estimated: quantity.estimated(),
            unknown_records: quantity.unknown_records(),
            overflowed: quantity.overflowed(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CostQuantityView {
    scale_decimal_places: u8,
    exact_pico_units: Option<u64>,
    estimated_pico_units: Option<u64>,
    records: u64,
    overflowed: bool,
}

impl From<&CostQuantity> for CostQuantityView {
    fn from(quantity: &CostQuantity) -> Self {
        Self {
            scale_decimal_places: quantity.scale_decimal_places(),
            exact_pico_units: quantity.exact_pico_units(),
            estimated_pico_units: quantity.estimated_pico_units(),
            records: quantity.records(),
            overflowed: quantity.overflowed(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct UsageSummaryView {
    attempts: u64,
    total_tokens: UsageQuantityView,
    payg_cost_estimates: BTreeMap<String, CostQuantityView>,
    cost_unknown_attempts: u64,
}

impl From<&UsageRollup> for UsageSummaryView {
    fn from(rollup: &UsageRollup) -> Self {
        Self {
            attempts: rollup.attempts(),
            total_tokens: rollup.total_tokens().into(),
            payg_cost_estimates: rollup
                .payg_cost_estimates()
                .iter()
                .map(|(currency, quantity)| (currency.clone(), quantity.into()))
                .collect(),
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
    context_pressure: Availability<ContextPressureSnapshot>,
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
#[serde(tag = "source", rename_all = "snake_case")]
pub(crate) enum ModelSelectorChoiceView {
    ConfiguredPreset { preset: ModelPresetView },
    ReleaseCatalog { model: ModelCatalogView },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ModelSelectorEntryView {
    choice: ModelSelectorChoiceView,
    compatibility: Availability<bool>,
    availability: Availability<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelSelectorGroup {
    Favorites,
    Recent,
    Compatible,
    All,
}

impl ModelSelectorGroup {
    const ALL: [Self; 4] = [Self::Favorites, Self::Recent, Self::Compatible, Self::All];

    const fn label(self) -> &'static str {
        match self {
            Self::Favorites => "Favorites",
            Self::Recent => "Recent",
            Self::Compatible => "Compatible",
            Self::All => "All",
        }
    }

    fn moved(self, offset: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|group| *group == self)
            .expect("Model selector group is registered");
        let len = isize::try_from(Self::ALL.len()).expect("Model selector group count fits isize");
        let next = usize::try_from(
            (isize::try_from(index).expect("Model selector group index fits isize") + offset)
                .rem_euclid(len),
        )
        .expect("wrapped Model selector group index is non-negative");
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatsGroup {
    Attempts,
    Thread,
    Agent,
    Team,
    NamedWindow,
    TokenCache,
}

impl StatsGroup {
    const ALL: [Self; 6] = [
        Self::Attempts,
        Self::Thread,
        Self::Agent,
        Self::Team,
        Self::NamedWindow,
        Self::TokenCache,
    ];

    fn moved(self, offset: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|group| *group == self)
            .expect("Stats group is registered");
        let len = isize::try_from(Self::ALL.len()).expect("Stats group count fits isize");
        let next = usize::try_from(
            (isize::try_from(index).expect("Stats group index fits isize") + offset)
                .rem_euclid(len),
        )
        .expect("wrapped Stats group index is non-negative");
        Self::ALL[next]
    }

    fn entry_count(self, stats: &RuntimeUsageSnapshot) -> usize {
        match self {
            Self::Attempts => stats.attempts().len(),
            Self::Thread => usize::from(stats.thread().is_some()),
            Self::Agent => stats.agents().len(),
            Self::Team => usize::from(stats.team().is_some()),
            Self::NamedWindow => stats.named_windows().len(),
            Self::TokenCache => 3,
        }
    }
}

impl ModelSelectorEntryView {
    fn configured_preset(preset: ModelPresetView) -> Self {
        Self {
            choice: ModelSelectorChoiceView::ConfiguredPreset { preset },
            compatibility: Availability::Unknown,
            availability: Availability::Unknown,
        }
    }

    fn release_catalog(model: ModelCatalogView) -> Self {
        let availability = match model.record().availability().value() {
            CatalogAvailability::Unverified => Availability::Unknown,
            CatalogAvailability::Available => Availability::Known(true),
            CatalogAvailability::Unavailable => Availability::Known(false),
        };
        let compatibility = Availability::Known(
            model.profile_compatible()
                && has_provider_adapter(
                    model.record().provider_template(),
                    model.record().primary_dialect().value(),
                ),
        );
        Self {
            choice: ModelSelectorChoiceView::ReleaseCatalog { model },
            compatibility,
            availability,
        }
    }

    fn id(&self) -> &str {
        match &self.choice {
            ModelSelectorChoiceView::ConfiguredPreset { preset } => &preset.id,
            ModelSelectorChoiceView::ReleaseCatalog { model } => model.record().key(),
        }
    }

    fn provider(&self) -> &str {
        match &self.choice {
            ModelSelectorChoiceView::ConfiguredPreset { preset } => &preset.provider,
            ModelSelectorChoiceView::ReleaseCatalog { model } => model.provider(),
        }
    }

    fn model(&self) -> &str {
        match &self.choice {
            ModelSelectorChoiceView::ConfiguredPreset { preset } => &preset.model,
            ModelSelectorChoiceView::ReleaseCatalog { model } => model.record().model_id().value(),
        }
    }

    fn favorite(&self) -> bool {
        matches!(
            &self.choice,
            ModelSelectorChoiceView::ConfiguredPreset { preset } if preset.favorite
        )
    }

    #[cfg(test)]
    fn is_release_catalog(&self) -> bool {
        matches!(self.choice, ModelSelectorChoiceView::ReleaseCatalog { .. })
    }
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
        catalog_models: &[ModelCatalogView],
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
            .cloned()
            .map(ModelSelectorEntryView::configured_preset)
            .chain(
                catalog_models
                    .iter()
                    .cloned()
                    .map(ModelSelectorEntryView::release_catalog),
            )
            .filter(|entry| model_matches(entry, &tokens))
            .collect::<Vec<_>>();
        let favorites = all
            .iter()
            .filter(|entry| entry.favorite())
            .cloned()
            .collect();
        let compatible = if catalog_models.is_empty() {
            Availability::Unknown
        } else {
            Availability::Known(
                all.iter()
                    .filter(|entry| entry.compatibility == Availability::Known(true))
                    .cloned()
                    .collect(),
            )
        };
        Ok(Self {
            favorites,
            recent: Availability::Unknown,
            compatible,
            all,
        })
    }

    fn filtered(&self, query: &str) -> Self {
        debug_assert!(
            query.len() <= MAX_MODEL_QUERY_BYTES
                && !query.chars().any(|character| character.is_control())
        );
        let tokens = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let all = self
            .all
            .iter()
            .filter(|entry| model_matches(entry, &tokens))
            .cloned()
            .collect::<Vec<_>>();
        let favorites = all
            .iter()
            .filter(|entry| entry.favorite())
            .cloned()
            .collect();
        let compatible = match self.compatible {
            Availability::Known(_) => Availability::Known(
                all.iter()
                    .filter(|entry| entry.compatibility == Availability::Known(true))
                    .cloned()
                    .collect(),
            ),
            Availability::Unknown => Availability::Unknown,
        };
        Self {
            favorites,
            recent: Availability::Unknown,
            compatible,
            all,
        }
    }

    fn group_entries(&self, group: ModelSelectorGroup) -> Option<&[ModelSelectorEntryView]> {
        match group {
            ModelSelectorGroup::Favorites => Some(&self.favorites),
            ModelSelectorGroup::Recent => match &self.recent {
                Availability::Known(entries) => Some(entries),
                Availability::Unknown => None,
            },
            ModelSelectorGroup::Compatible => match &self.compatible {
                Availability::Known(entries) => Some(entries),
                Availability::Unknown => None,
            },
            ModelSelectorGroup::All => Some(&self.all),
        }
    }
}

fn model_matches(entry: &ModelSelectorEntryView, tokens: &[String]) -> bool {
    let (dialect, display_name, template, reasoning_effort, service_tier, context_mode) =
        match &entry.choice {
            ModelSelectorChoiceView::ConfiguredPreset { preset } => (
                preset.dialect.as_str(),
                None,
                None,
                preset.reasoning_effort.as_ref().map(|value| value.as_str()),
                preset.service_tier.as_ref().map(|value| value.as_str()),
                preset.context_mode.as_deref(),
            ),
            ModelSelectorChoiceView::ReleaseCatalog { model } => (
                model.record().primary_dialect().value().as_str(),
                Some(model.record().display_name().value()),
                Some(model.record().provider_template()),
                None,
                None,
                None,
            ),
        };
    let fields = [
        Some(entry.id()),
        Some(entry.provider()),
        Some(entry.model()),
        Some(dialect),
        display_name,
        template,
        reasoning_effort,
        service_tier,
        context_mode,
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
    pub(crate) context_pressure: Option<&'a ContextPressureSnapshot>,
    pub(crate) model_presets: &'a [ModelPresetView],
    pub(crate) catalog_models: &'a [ModelCatalogView],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AgentCenterEntryView {
    id: u64,
    parent: Option<u64>,
    status: &'static str,
    task: u64,
    task_status: &'static str,
    dependency_count: usize,
    token_budget: u64,
    tool_budget: u32,
    reserved_tokens: u64,
    reserved_tools: u32,
    capability_count: usize,
    scope_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AgentCenterView {
    revision: u64,
    ledger_transaction: u64,
    ledger_sequence: u64,
    recovered_tail_bytes: u64,
    operations_awaiting_acknowledgement: usize,
    message_count: usize,
    agents: Vec<AgentCenterEntryView>,
}

impl From<&KernelTeamSnapshot> for AgentCenterView {
    fn from(team: &KernelTeamSnapshot) -> Self {
        let projection = &team.projection;
        let agents = projection
            .agents
            .iter()
            .map(|agent| {
                let task = projection.task(agent.task);
                AgentCenterEntryView {
                    id: agent.id.get(),
                    parent: agent.parent.map(|parent| parent.get()),
                    status: agent_status_label(agent.status),
                    task: agent.task.get(),
                    task_status: task.map_or("unavailable", |task| task_status_label(&task.status)),
                    dependency_count: task.map_or(0, |task| task.dependencies.len()),
                    token_budget: agent.budget.token_units,
                    tool_budget: agent.budget.tool_calls,
                    reserved_tokens: agent.reserved_budget.token_units,
                    reserved_tools: agent.reserved_budget.tool_calls,
                    capability_count: agent.capabilities.iter().count(),
                    scope_count: task.map_or(0, |task| task.scope.iter().count()),
                }
            })
            .collect();
        Self {
            revision: projection.revision.get(),
            ledger_transaction: team.ledger_head.transaction,
            ledger_sequence: team.ledger_head.sequence,
            recovered_tail_bytes: team.recovered_tail_bytes,
            operations_awaiting_acknowledgement: team
                .operations
                .iter()
                .filter(|operation| {
                    operation.status == TeamOperationStatus::CommittedAwaitingAcknowledgement
                })
                .count(),
            message_count: projection.messages.len(),
            agents,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TuiViewModel {
    pub(crate) slash: SlashPanelView,
    pub(crate) statusline: StatuslineView,
    pub(crate) blockers: Vec<BlockerView>,
    pub(crate) models: ModelSelectorView,
    pub(crate) stats: Availability<RuntimeUsageSnapshot>,
    pub(crate) agents: Availability<AgentCenterView>,
}

impl TuiViewModel {
    pub(crate) fn build(
        slash_query: &str,
        model_query: &str,
        selected: usize,
        sources: PresentationSources<'_>,
    ) -> Result<Self, PresentationError> {
        let slash = SlashPanelView::build(slash_query, selected)?;
        let models =
            ModelSelectorView::build(sources.model_presets, sources.catalog_models, model_query)?;
        let stats = sources
            .usage
            .cloned()
            .map_or(Availability::Unknown, Availability::Known);
        let agents = sources
            .team
            .map(AgentCenterView::from)
            .map_or(Availability::Unknown, Availability::Known);
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
                .context_pressure
                .and_then(|pressure| pressure.occupancy_percent())
                .map_or(Availability::Unknown, Availability::Known),
            context_pressure: sources
                .context_pressure
                .copied()
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
            stats,
            agents,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Viewport {
    width: u16,
    height: u16,
}

impl Viewport {
    pub(crate) fn new(width: u16, height: u16) -> Result<Self, ViewportError> {
        if width == 0 {
            return Err(ViewportError::ZeroWidth);
        }
        if height == 0 {
            return Err(ViewportError::ZeroHeight);
        }
        Ok(Self { width, height })
    }

    pub(crate) const fn width(self) -> u16 {
        self.width
    }

    pub(crate) const fn height(self) -> u16 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewportError {
    ZeroWidth,
    ZeroHeight,
}

impl fmt::Display for ViewportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroWidth => "viewport width must be positive",
            Self::ZeroHeight => "viewport height must be positive",
        })
    }
}

impl Error for ViewportError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationState {
    SlashPanel,
    ConfigObjectCreate,
    ConfigCenter {
        section: Option<ConfigSection>,
        selected: usize,
        pending_query: Option<&'static str>,
    },
    ConfigEditor,
    ProviderWizard,
    ModelSelector {
        group: ModelSelectorGroup,
        selected: usize,
        detail: bool,
    },
    Stats {
        group: StatsGroup,
        selected: usize,
        detail: bool,
    },
    AgentCenter {
        selected: usize,
        detail: bool,
    },
}

struct ActiveConfigObjectCreate {
    kind: ConfigObjectKind,
    id: String,
}

struct ActiveConfigEditor {
    object: Option<ConfigObjectRef>,
    session: ConfigEditorSession,
    view: ConfigEditorView,
    dirty: bool,
    validated: bool,
    connection: ProviderConnectionTestStatus,
}

pub(crate) struct PresentationController {
    state: PresentationState,
    slash_query: String,
    model_query: String,
    selected: usize,
    object_create: Option<ActiveConfigObjectCreate>,
    editor: Option<ActiveConfigEditor>,
}

impl Default for PresentationController {
    fn default() -> Self {
        Self::new()
    }
}

// Mutation commands are contract-tested before a terminal adapter owns the event loop.
#[allow(dead_code)]
impl PresentationController {
    pub(crate) fn new() -> Self {
        Self {
            state: PresentationState::SlashPanel,
            slash_query: "/".to_owned(),
            model_query: String::new(),
            selected: 0,
            object_create: None,
            editor: None,
        }
    }

    pub(crate) const fn is_slash_panel(&self) -> bool {
        matches!(self.state, PresentationState::SlashPanel)
    }

    pub(crate) const fn is_provider_wizard(&self) -> bool {
        matches!(self.state, PresentationState::ProviderWizard)
    }

    pub(crate) const fn is_model_selector(&self) -> bool {
        matches!(self.state, PresentationState::ModelSelector { .. })
    }

    pub(crate) const fn is_stats(&self) -> bool {
        matches!(self.state, PresentationState::Stats { .. })
    }

    pub(crate) const fn is_agent_center(&self) -> bool {
        matches!(self.state, PresentationState::AgentCenter { .. })
    }

    pub(crate) const fn is_config_object_selector(&self) -> bool {
        matches!(
            self.state,
            PresentationState::ConfigCenter {
                pending_query: Some(_),
                ..
            }
        )
    }

    pub(crate) const fn is_config_object_create(&self) -> bool {
        matches!(self.state, PresentationState::ConfigObjectCreate)
    }

    pub(crate) fn config_object_id(&self) -> Option<&str> {
        self.object_create.as_ref().map(|create| create.id.as_str())
    }

    pub(crate) fn config_editor_field(&self) -> Option<&ConfigFieldView> {
        self.editor.as_ref().map(|editor| &editor.view.field)
    }

    pub(crate) fn has_unsaved_config_draft(&self) -> bool {
        self.editor.as_ref().is_some_and(|editor| editor.dirty)
    }

    pub(crate) fn is_config_object_delete(&self) -> bool {
        self.state == PresentationState::ConfigEditor
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.session.operation() == ConfigEditorOperation::Delete)
    }

    pub(crate) fn set_slash_query(
        &mut self,
        query: &str,
    ) -> Result<(), PresentationControllerError> {
        self.require_discardable_editor()?;
        let panel = SlashPanelView::build(query, 0)?;
        self.slash_query = panel.query;
        self.selected = panel.selected.unwrap_or(0);
        self.state = PresentationState::SlashPanel;
        self.object_create = None;
        self.editor = None;
        Ok(())
    }

    pub(crate) fn move_selection(
        &mut self,
        offset: isize,
    ) -> Result<(), PresentationControllerError> {
        if self.state != PresentationState::SlashPanel {
            return Err(PresentationControllerError::NotSlashPanel);
        }
        let panel = SlashPanelView::build(&self.slash_query, self.selected)?;
        if panel.entries.is_empty() {
            self.selected = 0;
            return Ok(());
        }
        self.selected = self
            .selected
            .saturating_add_signed(offset)
            .min(panel.entries.len() - 1);
        Ok(())
    }

    pub(crate) fn set_model_query(&mut self, query: &str) -> Result<(), PresentationError> {
        ModelSelectorView::build(&[], &[], query)?;
        self.model_query = query.to_owned();
        if let PresentationState::ModelSelector {
            selected, detail, ..
        } = &mut self.state
        {
            *selected = 0;
            *detail = false;
        }
        Ok(())
    }

    pub(crate) fn edit_model_query(
        &mut self,
        character: Option<char>,
    ) -> Result<(), PresentationError> {
        let mut query = self.model_query.clone();
        if let Some(character) = character {
            query.push(character);
        } else if let Some((index, _)) =
            UnicodeSegmentation::grapheme_indices(query.as_str(), true).next_back()
        {
            query.truncate(index);
        }
        self.set_model_query(&query)
    }

    pub(crate) fn clear_model_query(&mut self) -> Result<(), PresentationError> {
        self.set_model_query("")
    }

    pub(crate) fn move_model_group(&mut self, offset: isize) {
        if let PresentationState::ModelSelector {
            group,
            selected,
            detail,
        } = &mut self.state
        {
            *group = group.moved(offset);
            *selected = 0;
            *detail = false;
        }
    }

    pub(crate) fn move_model_selection(&mut self, models: &ModelSelectorView, offset: isize) {
        let PresentationState::ModelSelector {
            group,
            selected,
            detail,
        } = &mut self.state
        else {
            return;
        };
        let filtered = models.filtered(&self.model_query);
        let Some(entries) = filtered.group_entries(*group) else {
            *selected = 0;
            *detail = false;
            return;
        };
        if entries.is_empty() {
            *selected = 0;
        } else {
            *selected = selected
                .saturating_add_signed(offset)
                .min(entries.len() - 1);
        }
        *detail = false;
    }

    pub(crate) fn toggle_model_detail(&mut self, models: &ModelSelectorView) {
        let PresentationState::ModelSelector {
            group,
            selected,
            detail,
        } = &mut self.state
        else {
            return;
        };
        let filtered = models.filtered(&self.model_query);
        if filtered
            .group_entries(*group)
            .is_some_and(|entries| entries.get(*selected).is_some())
        {
            *detail = !*detail;
        }
    }

    pub(crate) fn move_stats_selection(
        &mut self,
        stats: &Availability<RuntimeUsageSnapshot>,
        offset: isize,
    ) {
        let PresentationState::Stats {
            group,
            selected,
            detail,
        } = &mut self.state
        else {
            return;
        };
        let Availability::Known(stats) = stats else {
            *selected = 0;
            *detail = false;
            return;
        };
        let entry_count = group.entry_count(stats);
        if entry_count == 0 {
            *selected = 0;
        } else {
            *selected = selected.saturating_add_signed(offset).min(entry_count - 1);
        }
        *detail = false;
    }

    pub(crate) fn move_stats_group(&mut self, offset: isize) {
        if let PresentationState::Stats {
            group,
            selected,
            detail,
        } = &mut self.state
        {
            *group = group.moved(offset);
            *selected = 0;
            *detail = false;
        }
    }

    pub(crate) fn toggle_stats_detail(&mut self, stats: &Availability<RuntimeUsageSnapshot>) {
        let PresentationState::Stats {
            group,
            selected,
            detail,
        } = &mut self.state
        else {
            return;
        };
        if matches!(stats, Availability::Known(stats) if *selected < group.entry_count(stats)) {
            *detail = !*detail;
        }
    }

    pub(crate) fn move_agent_selection(
        &mut self,
        agents: &Availability<AgentCenterView>,
        offset: isize,
    ) {
        let PresentationState::AgentCenter { selected, detail } = &mut self.state else {
            return;
        };
        let Availability::Known(agents) = agents else {
            *selected = 0;
            *detail = false;
            return;
        };
        if agents.agents.is_empty() {
            *selected = 0;
        } else {
            *selected = selected
                .saturating_add_signed(offset)
                .min(agents.agents.len() - 1);
        }
        *detail = false;
    }

    pub(crate) fn toggle_agent_detail(&mut self, agents: &Availability<AgentCenterView>) {
        let PresentationState::AgentCenter { selected, detail } = &mut self.state else {
            return;
        };
        if matches!(
            agents,
            Availability::Known(agents) if agents.agents.get(*selected).is_some()
        ) {
            *detail = !*detail;
        }
    }

    pub(crate) fn activate(
        &mut self,
        runtime: &mut ConfigRuntime,
        scope: ConfigScope,
        object: Option<&ConfigObjectRef>,
    ) -> Result<(), PresentationControllerError> {
        if self.state != PresentationState::SlashPanel {
            return Err(PresentationControllerError::NotSlashPanel);
        }
        let panel = SlashPanelView::build(&self.slash_query, self.selected)?;
        let entry = panel
            .entries
            .get(self.selected)
            .ok_or(PresentationControllerError::NoCommandSelection)?;
        let target = entry.target;
        match target {
            CommandTarget::ConfigCenter => {
                self.state = PresentationState::ConfigCenter {
                    section: None,
                    selected: 0,
                    pending_query: None,
                };
            }
            CommandTarget::ConfigSection { section } => {
                self.state = PresentationState::ConfigCenter {
                    section: Some(section),
                    selected: 0,
                    pending_query: None,
                };
            }
            CommandTarget::ConfigObjectCreate { kind } => {
                let Some(object) = object else {
                    if !matches!(
                        kind,
                        ConfigObjectKind::ProviderProfile
                            | ConfigObjectKind::ModelPreset
                            | ConfigObjectKind::PriceSchedule
                            | ConfigObjectKind::UsageWindow
                    ) {
                        return Err(PresentationControllerError::ConfigObjectRouteUnavailable);
                    }
                    self.object_create = Some(ActiveConfigObjectCreate {
                        kind,
                        id: String::new(),
                    });
                    self.state = PresentationState::ConfigObjectCreate;
                    return Ok(());
                };
                if object.kind() != kind {
                    return Err(ConfigEditorError::ConfigObjectMismatch.into());
                }
                let session = ConfigEditorSession::create_object(runtime, scope, object.clone())?;
                let view = session.current_view(runtime)?;
                self.editor = Some(ActiveConfigEditor {
                    object: Some(object.clone()),
                    session,
                    view,
                    dirty: false,
                    validated: false,
                    connection: ProviderConnectionTestStatus::Untested,
                });
                self.state = if kind == ConfigObjectKind::ProviderProfile {
                    PresentationState::ProviderWizard
                } else {
                    PresentationState::ConfigEditor
                };
                self.object_create = None;
            }
            CommandTarget::ConfigObjectDelete { kind } => {
                let Some(object) = object else {
                    self.state = PresentationState::ConfigCenter {
                        section: Some(object_section(kind)),
                        selected: 0,
                        pending_query: Some(entry.canonical),
                    };
                    return Ok(());
                };
                if object.kind() != kind {
                    return Err(ConfigEditorError::ConfigObjectMismatch.into());
                }
                let session = ConfigEditorSession::delete_object(runtime, scope, object.clone())?;
                let view = session.current_view(runtime)?;
                self.editor = Some(ActiveConfigEditor {
                    object: Some(object.clone()),
                    session,
                    view,
                    dirty: true,
                    validated: false,
                    connection: ProviderConnectionTestStatus::Untested,
                });
                self.state = PresentationState::ConfigEditor;
            }
            CommandTarget::ConfigEditor { path_pattern, .. }
                if object.is_none() && path_pattern.contains("<id>") =>
            {
                let section = config_section_for_path_pattern(path_pattern)
                    .ok_or(ConfigEditorError::ConfigObjectRequired)?;
                self.state = PresentationState::ConfigCenter {
                    section: Some(section),
                    selected: 0,
                    pending_query: Some(entry.canonical),
                };
            }
            CommandTarget::ConfigEditor { .. } => {
                let session = ConfigEditorSession::open_from_query(
                    runtime,
                    scope,
                    &self.slash_query,
                    self.selected,
                    object,
                )?;
                let view = session.preview(runtime)?;
                self.editor = Some(ActiveConfigEditor {
                    object: object.cloned(),
                    session,
                    view,
                    dirty: false,
                    validated: true,
                    connection: ProviderConnectionTestStatus::Untested,
                });
                self.state = if object
                    .is_some_and(|object| object.kind() == ConfigObjectKind::ProviderProfile)
                {
                    PresentationState::ProviderWizard
                } else {
                    PresentationState::ConfigEditor
                };
            }
            CommandTarget::ModelSelector => {
                self.model_query.clear();
                self.state = PresentationState::ModelSelector {
                    group: ModelSelectorGroup::All,
                    selected: 0,
                    detail: false,
                };
            }
            CommandTarget::Stats => {
                self.state = PresentationState::Stats {
                    group: StatsGroup::Attempts,
                    selected: 0,
                    detail: false,
                };
            }
            CommandTarget::AgentCenter => {
                self.state = PresentationState::AgentCenter {
                    selected: 0,
                    detail: false,
                };
            }
        }
        Ok(())
    }

    pub(crate) fn set_config_object_id(
        &mut self,
        id: &str,
    ) -> Result<(), PresentationControllerError> {
        let create = self
            .object_create
            .as_mut()
            .filter(|_| self.state == PresentationState::ConfigObjectCreate)
            .ok_or(PresentationControllerError::NotConfigObjectCreate)?;
        create.id.clear();
        create.id.push_str(id);
        Ok(())
    }

    pub(crate) fn submit_config_object_id(
        &mut self,
        runtime: &mut ConfigRuntime,
        scope: ConfigScope,
    ) -> Result<(), PresentationControllerError> {
        let create = self
            .object_create
            .as_ref()
            .filter(|_| self.state == PresentationState::ConfigObjectCreate)
            .ok_or(PresentationControllerError::NotConfigObjectCreate)?;
        let object = ConfigObjectRef::new(create.kind, create.id.clone());
        let session = ConfigEditorSession::create_object(runtime, scope, object.clone())?;
        let view = session.current_view(runtime)?;
        self.editor = Some(ActiveConfigEditor {
            object: Some(object),
            session,
            view,
            dirty: false,
            validated: false,
            connection: ProviderConnectionTestStatus::Untested,
        });
        self.state = if create.kind == ConfigObjectKind::ProviderProfile {
            PresentationState::ProviderWizard
        } else {
            PresentationState::ConfigEditor
        };
        self.object_create = None;
        Ok(())
    }

    pub(crate) fn move_config_object_selection(
        &mut self,
        runtime: &ConfigRuntime,
        offset: isize,
    ) -> Result<(), PresentationControllerError> {
        let PresentationState::ConfigCenter {
            section, selected, ..
        } = &mut self.state
        else {
            return Err(PresentationControllerError::NotConfigCenter);
        };
        let objects = config_center_objects(runtime, *section)?;
        if objects.is_empty() {
            *selected = 0;
        } else {
            *selected = selected
                .saturating_add_signed(offset)
                .min(objects.len() - 1);
        }
        Ok(())
    }

    pub(crate) fn activate_config_object(
        &mut self,
        runtime: &mut ConfigRuntime,
        scope: ConfigScope,
    ) -> Result<(), PresentationControllerError> {
        let PresentationState::ConfigCenter {
            section,
            selected,
            pending_query,
        } = &self.state
        else {
            return Err(PresentationControllerError::NotConfigCenter);
        };
        let object = config_center_objects(runtime, *section)?
            .get(*selected)
            .cloned()
            .ok_or(PresentationControllerError::NoConfigObjectSelection)?;
        let query =
            (*pending_query).ok_or(PresentationControllerError::ConfigObjectRouteUnavailable)?;
        self.set_slash_query(query)?;
        self.activate(runtime, scope, Some(&object))
    }

    pub(crate) fn back(&mut self) -> Result<(), PresentationControllerError> {
        if let PresentationState::ModelSelector { detail, .. }
        | PresentationState::Stats { detail, .. }
        | PresentationState::AgentCenter { detail, .. } = &mut self.state
            && *detail
        {
            *detail = false;
            return Ok(());
        }
        self.require_discardable_editor()?;
        self.object_create = None;
        self.editor = None;
        self.state = PresentationState::SlashPanel;
        Ok(())
    }

    pub(crate) fn discard_config(&mut self) -> Result<(), PresentationControllerError> {
        if !matches!(
            self.state,
            PresentationState::ConfigEditor | PresentationState::ProviderWizard
        ) {
            return Err(PresentationControllerError::NotConfigEditor);
        }
        self.editor = None;
        self.state = PresentationState::SlashPanel;
        Ok(())
    }

    pub(crate) fn stage_config(
        &mut self,
        runtime: &ConfigRuntime,
        raw: &str,
    ) -> Result<(), PresentationControllerError> {
        let editor = self.active_editor_mut()?;
        editor.session.stage_raw(raw)?;
        editor.view.field = editor.session.field(runtime)?;
        editor.view.changes.clear();
        editor.dirty = true;
        editor.validated = false;
        editor.connection = ProviderConnectionTestStatus::Untested;
        Ok(())
    }

    pub(crate) fn stage_provider_credential_reference(
        &mut self,
        runtime: &ConfigRuntime,
        reference: &str,
    ) -> Result<(), PresentationControllerError> {
        if self.state != PresentationState::ProviderWizard {
            return Err(PresentationControllerError::NotProviderWizard);
        }
        let editor = self.active_editor_mut()?;
        editor.session.stage_credential_reference(reference)?;
        editor.view.field = editor.session.field(runtime)?;
        editor.view.changes.clear();
        editor.dirty = true;
        editor.validated = false;
        editor.connection = ProviderConnectionTestStatus::Untested;
        Ok(())
    }

    pub(crate) fn reset_provider_credential_reference(
        &mut self,
        runtime: &ConfigRuntime,
    ) -> Result<(), PresentationControllerError> {
        if self.state != PresentationState::ProviderWizard {
            return Err(PresentationControllerError::NotProviderWizard);
        }
        let editor = self.active_editor_mut()?;
        editor.session.reset_credential_reference()?;
        editor.view.field = editor.session.field(runtime)?;
        editor.view.changes.clear();
        editor.dirty = true;
        editor.validated = false;
        editor.connection = ProviderConnectionTestStatus::Untested;
        Ok(())
    }

    pub(crate) fn reset_config(
        &mut self,
        runtime: &ConfigRuntime,
    ) -> Result<(), PresentationControllerError> {
        let editor = self.active_editor_mut()?;
        editor.session.reset()?;
        editor.view.field = editor.session.field(runtime)?;
        editor.view.changes.clear();
        editor.dirty = true;
        editor.validated = false;
        editor.connection = ProviderConnectionTestStatus::Untested;
        Ok(())
    }

    pub(crate) fn focus_config_field(
        &mut self,
        runtime: &ConfigRuntime,
        query: &str,
        selected: usize,
    ) -> Result<(), PresentationControllerError> {
        let editor = self.active_editor_mut()?;
        editor.session.focus_from_query(runtime, query, selected)?;
        editor.view.field = editor.session.field(runtime)?;
        Ok(())
    }

    pub(crate) fn move_config_field(
        &mut self,
        runtime: &ConfigRuntime,
        offset: isize,
    ) -> Result<(), PresentationControllerError> {
        let editor = self.active_editor_mut()?;
        editor.session.move_field(runtime, offset)?;
        editor.view.field = editor.session.field(runtime)?;
        Ok(())
    }

    pub(crate) fn preview_config(
        &mut self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigEditorView, PresentationControllerError> {
        let preview = self.active_editor()?.session.preview(runtime)?;
        let editor = self.active_editor_mut()?;
        editor.dirty = !preview.changes.is_empty();
        editor.validated = true;
        editor.view = preview.clone();
        Ok(preview)
    }

    pub(crate) fn test_provider_connection<T: ProviderConnectionTester + ?Sized>(
        &mut self,
        runtime: &ConfigRuntime,
        tester: &mut T,
    ) -> Result<ProviderConnectionTestStatus, PresentationControllerError> {
        if self.state != PresentationState::ProviderWizard {
            return Err(PresentationControllerError::NotProviderWizard);
        }
        let profile = self.active_editor()?.session.provider_profile(runtime)?;
        let status = tester.test(&profile);
        self.active_editor_mut()?.connection = status.clone();
        Ok(status)
    }

    pub(crate) fn commit_config(
        &mut self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigCommit, PresentationControllerError> {
        let commit = if self.state == PresentationState::ProviderWizard {
            self.active_editor()?
                .session
                .try_commit_provider_profile(runtime)?
        } else {
            self.active_editor()?.session.try_commit(runtime)?
        };
        self.editor = None;
        self.state = PresentationState::SlashPanel;
        Ok(commit)
    }

    pub(crate) fn screen(
        &self,
        runtime: Option<&ConfigRuntime>,
    ) -> Result<PresentationScreenView, PresentationControllerError> {
        match self.state {
            PresentationState::SlashPanel => Ok(PresentationScreenView::SlashPanel(
                SlashPanelView::build(&self.slash_query, self.selected)?,
            )),
            PresentationState::ConfigObjectCreate => {
                let create = self
                    .object_create
                    .as_ref()
                    .ok_or(PresentationControllerError::NotConfigObjectCreate)?;
                Ok(PresentationScreenView::ConfigObjectCreate {
                    kind: create.kind,
                    id: create.id.clone(),
                })
            }
            PresentationState::ConfigCenter {
                section,
                selected,
                pending_query,
            } => {
                let objects = config_center_objects(
                    runtime.ok_or(PresentationControllerError::ConfigRuntimeRequired)?,
                    section,
                )?;
                let selected = (pending_query.is_some() && !objects.is_empty())
                    .then(|| selected.min(objects.len() - 1));
                Ok(PresentationScreenView::ConfigCenter {
                    section,
                    objects,
                    selected,
                })
            }
            PresentationState::ConfigEditor => {
                let editor = self.active_editor()?;
                Ok(PresentationScreenView::ConfigEditor {
                    object: editor.object.clone(),
                    operation: editor.session.operation(),
                    editor: editor.view.clone(),
                    dirty: editor.dirty,
                    validated: editor.validated,
                })
            }
            PresentationState::ProviderWizard => {
                let editor = self.active_editor()?;
                Ok(PresentationScreenView::ProviderWizard {
                    object: editor
                        .object
                        .clone()
                        .ok_or(PresentationControllerError::NotProviderWizard)?,
                    operation: editor.session.operation(),
                    editor: editor.view.clone(),
                    dirty: editor.dirty,
                    validated: editor.validated,
                    connection: editor.connection.clone(),
                })
            }
            PresentationState::ModelSelector {
                group,
                selected,
                detail,
            } => Ok(PresentationScreenView::ModelSelector {
                query: self.model_query.clone(),
                group,
                selected,
                detail,
            }),
            PresentationState::Stats {
                group,
                selected,
                detail,
            } => Ok(PresentationScreenView::Stats {
                group,
                selected,
                detail,
            }),
            PresentationState::AgentCenter { selected, detail } => {
                Ok(PresentationScreenView::AgentCenter { selected, detail })
            }
        }
    }

    pub(crate) fn layout(
        &self,
        runtime: Option<&ConfigRuntime>,
        view: &TuiViewModel,
        viewport: Viewport,
    ) -> Result<PresentationLayoutView, PresentationControllerError> {
        let screen = self.screen(runtime)?;
        let statusline =
            StatuslineLayoutView::build(&view.statusline, viewport.width, viewport.height);
        let body_capacity = usize::from(viewport.height).saturating_sub(statusline.rows.len());
        let mut body = screen_rows(&screen, view);
        if matches!(
            screen,
            PresentationScreenView::ConfigCenter { .. }
                | PresentationScreenView::ModelSelector { detail: false, .. }
                | PresentationScreenView::Stats { detail: false, .. }
                | PresentationScreenView::AgentCenter { detail: false, .. }
        ) {
            truncate_rows_keeping_selection(&mut body, body_capacity);
        } else {
            body.truncate(body_capacity);
        }
        for row in &mut body {
            row.text = fit_text(&row.text, usize::from(viewport.width));
        }
        Ok(PresentationLayoutView {
            viewport,
            screen,
            body,
            statusline,
        })
    }

    fn active_editor(&self) -> Result<&ActiveConfigEditor, PresentationControllerError> {
        if !matches!(
            self.state,
            PresentationState::ConfigEditor | PresentationState::ProviderWizard
        ) {
            return Err(PresentationControllerError::NotConfigEditor);
        }
        self.editor
            .as_ref()
            .ok_or(PresentationControllerError::NotConfigEditor)
    }

    fn active_editor_mut(
        &mut self,
    ) -> Result<&mut ActiveConfigEditor, PresentationControllerError> {
        if !matches!(
            self.state,
            PresentationState::ConfigEditor | PresentationState::ProviderWizard
        ) {
            return Err(PresentationControllerError::NotConfigEditor);
        }
        self.editor
            .as_mut()
            .ok_or(PresentationControllerError::NotConfigEditor)
    }

    fn require_discardable_editor(&self) -> Result<(), PresentationControllerError> {
        if self.editor.as_ref().is_some_and(|editor| editor.dirty) {
            Err(PresentationControllerError::UnsavedConfigDraft)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "screen", rename_all = "snake_case")]
pub(crate) enum PresentationScreenView {
    SlashPanel(SlashPanelView),
    ConfigObjectCreate {
        kind: ConfigObjectKind,
        id: String,
    },
    ConfigCenter {
        section: Option<ConfigSection>,
        objects: Vec<ConfigObjectRef>,
        selected: Option<usize>,
    },
    ConfigEditor {
        object: Option<ConfigObjectRef>,
        operation: ConfigEditorOperation,
        editor: ConfigEditorView,
        dirty: bool,
        validated: bool,
    },
    ProviderWizard {
        object: ConfigObjectRef,
        operation: ConfigEditorOperation,
        editor: ConfigEditorView,
        dirty: bool,
        validated: bool,
        connection: ProviderConnectionTestStatus,
    },
    ModelSelector {
        query: String,
        group: ModelSelectorGroup,
        selected: usize,
        detail: bool,
    },
    Stats {
        group: StatsGroup,
        selected: usize,
        detail: bool,
    },
    AgentCenter {
        selected: usize,
        detail: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct LayoutRowView {
    text: String,
    selected: bool,
}

impl LayoutRowView {
    fn new(text: impl Into<String>, selected: bool) -> Self {
        Self {
            text: text.into(),
            selected,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) const fn is_selected(&self) -> bool {
        self.selected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusSegmentKind {
    Recovery,
    Blockers,
    Model,
    Context,
    Usage,
    Cost,
    Agents,
    Provider,
    Config,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StatuslineLayoutView {
    rows: Vec<LayoutRowView>,
    hidden: Vec<StatusSegmentKind>,
}

impl StatuslineLayoutView {
    fn build(status: &StatuslineView, width: u16, height: u16) -> Self {
        let segments = status_segments(status);
        let (compact, hidden) = pack_status_segments(&segments, usize::from(width));
        let mut rows = vec![LayoutRowView::new(compact, false)];
        if width >= 120 && height >= 2 {
            let detail = format!(
                "thread {} | items {} | tail {}B",
                status
                    .thread
                    .map_or_else(|| "?".to_owned(), |thread| thread.to_string()),
                status.item_count,
                status.recovered_tail_bytes
            );
            rows.push(LayoutRowView::new(
                fit_text(&detail, usize::from(width)),
                false,
            ));
        }
        Self { rows, hidden }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PresentationLayoutView {
    viewport: Viewport,
    screen: PresentationScreenView,
    body: Vec<LayoutRowView>,
    statusline: StatuslineLayoutView,
}

impl PresentationLayoutView {
    pub(crate) const fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub(crate) fn body(&self) -> &[LayoutRowView] {
        &self.body
    }

    pub(crate) fn statusline_rows(&self) -> &[LayoutRowView] {
        &self.statusline.rows
    }

    pub(crate) fn show_pending_config_text(&mut self, value: &str) {
        let Some(selected) = self.body.iter().position(|row| row.selected) else {
            return;
        };
        self.body[selected].text = format!("> target {value}");
        if let Some(draft) = self.body.get_mut(selected + 1) {
            draft.text = "draft pending".to_owned();
        }
    }
}

fn truncate_rows_keeping_selection(rows: &mut Vec<LayoutRowView>, capacity: usize) {
    if rows.len() <= capacity {
        return;
    }
    if capacity == 0 {
        rows.clear();
        return;
    }
    let Some(selected) = rows.iter().position(LayoutRowView::is_selected) else {
        rows.truncate(capacity);
        return;
    };
    if selected < capacity {
        rows.truncate(capacity);
        return;
    }
    if capacity == 1 {
        let selected = rows[selected].clone();
        rows.clear();
        rows.push(selected);
        return;
    }

    let tail_capacity = capacity - 1;
    let start = selected + 1 - tail_capacity;
    let mut visible = Vec::with_capacity(capacity);
    visible.push(rows[0].clone());
    visible.extend_from_slice(&rows[start..=selected]);
    *rows = visible;
}

struct StatusSegment {
    kind: StatusSegmentKind,
    text: String,
    preferred_width: usize,
}

fn status_segments(status: &StatuslineView) -> Vec<StatusSegment> {
    vec![
        StatusSegment {
            kind: StatusSegmentKind::Recovery,
            text: recovery_label(&status.recovery),
            preferred_width: 24,
        },
        StatusSegment {
            kind: StatusSegmentKind::Blockers,
            text: availability_count("blockers", &status.blocker_count),
            preferred_width: 14,
        },
        StatusSegment {
            kind: StatusSegmentKind::Model,
            text: availability_text("model", &status.model),
            preferred_width: 30,
        },
        StatusSegment {
            kind: StatusSegmentKind::Context,
            text: match &status.context_pressure {
                Availability::Known(pressure) => {
                    match (pressure.occupancy_percent(), pressure.accuracy()) {
                        (Some(percent), Some(ContextPressureAccuracy::Estimated)) => {
                            format!("ctx ~{percent}%")
                        }
                        (Some(percent), Some(ContextPressureAccuracy::Exact)) => {
                            format!("ctx {percent}%")
                        }
                        _ => "ctx ?".to_owned(),
                    }
                }
                Availability::Unknown => "ctx ?".to_owned(),
            },
            preferred_width: 10,
        },
        StatusSegment {
            kind: StatusSegmentKind::Usage,
            text: usage_label(&status.one_hour_usage),
            preferred_width: 18,
        },
        StatusSegment {
            kind: StatusSegmentKind::Cost,
            text: cost_label(&status.one_hour_usage),
            preferred_width: 24,
        },
        StatusSegment {
            kind: StatusSegmentKind::Agents,
            text: availability_count("agents", &status.active_agents),
            preferred_width: 12,
        },
        StatusSegment {
            kind: StatusSegmentKind::Provider,
            text: availability_text("provider", &status.provider_profile),
            preferred_width: 26,
        },
        StatusSegment {
            kind: StatusSegmentKind::Config,
            text: if status.config_ready {
                "config ok".to_owned()
            } else {
                "config repair".to_owned()
            },
            preferred_width: 14,
        },
    ]
}

fn pack_status_segments(
    segments: &[StatusSegment],
    width: usize,
) -> (String, Vec<StatusSegmentKind>) {
    let mut row = String::new();
    let mut hidden = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let separator = usize::from(!row.is_empty()) * 3;
        let used = display_width(&row);
        let available = width.saturating_sub(used + separator);
        if available == 0 {
            hidden.extend(segments[index..].iter().map(|segment| segment.kind));
            break;
        }
        let preferred = fit_text(&segment.text, segment.preferred_width.min(available));
        if !row.is_empty() {
            row.push_str(" | ");
        }
        row.push_str(&preferred);
        if display_width(&segment.text) > available {
            hidden.extend(segments[index + 1..].iter().map(|segment| segment.kind));
            break;
        }
    }
    (row, hidden)
}

fn recovery_label(recovery: &RecoveryBadge) -> String {
    match recovery {
        RecoveryBadge::Ready => "ready".to_owned(),
        RecoveryBadge::ResumeRequired { turn } => format!("resume t{turn}"),
        RecoveryBadge::ReconciliationRequired { turn, delivery } => {
            format!("reconcile t{turn}/d{delivery}")
        }
        RecoveryBadge::Blocked { turn, .. } => format!("blocked t{turn}"),
    }
}

fn availability_text(label: &str, value: &Availability<String>) -> String {
    match value {
        Availability::Known(value) => format!("{label} {value}"),
        Availability::Unknown => format!("{label} ?"),
    }
}

fn availability_count(label: &str, value: &Availability<usize>) -> String {
    match value {
        Availability::Known(value) => format!("{label} {value}"),
        Availability::Unknown => format!("{label} ?"),
    }
}

fn usage_label(usage: &Availability<UsageSummaryView>) -> String {
    let Availability::Known(usage) = usage else {
        return "1h ?".to_owned();
    };
    match (
        usage.total_tokens.exact,
        usage.total_tokens.estimated,
        usage.total_tokens.unknown_records,
    ) {
        (Some(exact), Some(estimated), _) => format!("1h {exact}+~{estimated}t"),
        (Some(exact), None, _) => format!("1h {exact}t"),
        (None, Some(estimated), _) => format!("1h ~{estimated}t"),
        (None, None, 0) => "1h 0t".to_owned(),
        (None, None, _) => "1h ?".to_owned(),
    }
}

fn cost_label(usage: &Availability<UsageSummaryView>) -> String {
    let Availability::Known(usage) = usage else {
        return "cost ?".to_owned();
    };
    let mut costs = usage.payg_cost_estimates.iter();
    let Some((currency, quantity)) = costs.next() else {
        return if usage.cost_unknown_attempts == 0 {
            "cost 0".to_owned()
        } else {
            "cost ?".to_owned()
        };
    };
    if costs.next().is_some() {
        return if usage.cost_unknown_attempts == 0 {
            "cost mixed".to_owned()
        } else {
            "cost mixed+?".to_owned()
        };
    }
    let amount = match (
        quantity.exact_pico_units,
        quantity.estimated_pico_units,
        quantity.overflowed,
    ) {
        (_, _, true) => "overflow".to_owned(),
        (Some(0), Some(estimated), false) if estimated > 0 => {
            format!("~{}", format_cost(estimated, quantity.scale_decimal_places))
        }
        (Some(exact), Some(estimated), false) if estimated > 0 => {
            format!(
                "{}+~{}",
                format_cost(exact, quantity.scale_decimal_places),
                format_cost(estimated, quantity.scale_decimal_places)
            )
        }
        (Some(exact), _, false) => format_cost(exact, quantity.scale_decimal_places),
        (None, Some(estimated), false) => {
            format!("~{}", format_cost(estimated, quantity.scale_decimal_places))
        }
        (None, None, false) => "?".to_owned(),
    };
    let unknown = if usage.cost_unknown_attempts == 0 {
        ""
    } else {
        "+?"
    };
    format!("{currency} {amount}{unknown}")
}

fn format_cost(units: u64, decimal_places: u8) -> String {
    if decimal_places == 0 {
        return units.to_string();
    }
    let scale = 10_u64.pow(u32::from(decimal_places));
    format!(
        "{}.{:0width$}",
        units / scale,
        units % scale,
        width = usize::from(decimal_places)
    )
}

fn screen_rows(screen: &PresentationScreenView, view: &TuiViewModel) -> Vec<LayoutRowView> {
    match screen {
        PresentationScreenView::SlashPanel(panel) => {
            let mut rows = vec![LayoutRowView::new("Commands", false)];
            rows.extend(panel.entries.iter().enumerate().map(|(index, entry)| {
                let selected = panel.selected == Some(index);
                LayoutRowView::new(
                    format!("{} {}", if selected { '>' } else { ' ' }, entry.canonical),
                    selected,
                )
            }));
            rows
        }
        PresentationScreenView::ConfigObjectCreate { kind, id } => vec![
            LayoutRowView::new(format!("Create {}", config_object_label(*kind)), false),
            LayoutRowView::new(
                format!("> ID {}", if id.is_empty() { "<new>" } else { id }),
                true,
            ),
        ],
        PresentationScreenView::ConfigCenter {
            section,
            objects,
            selected,
        } => {
            let title = section.map_or_else(
                || "Config".to_owned(),
                |section| format!("Config / {}", config_section_label(section)),
            );
            let mut rows = vec![LayoutRowView::new(title, false)];
            rows.extend(objects.iter().enumerate().map(|(index, object)| {
                let is_selected = *selected == Some(index);
                LayoutRowView::new(
                    format!(
                        "{} {} {}",
                        if is_selected { '>' } else { ' ' },
                        config_object_label(object.kind()),
                        object.id()
                    ),
                    is_selected,
                )
            }));
            rows
        }
        PresentationScreenView::ConfigEditor {
            object,
            operation,
            editor,
            dirty,
            validated,
        } if *operation == ConfigEditorOperation::Delete => {
            config_delete_rows(object.as_ref(), *dirty, *validated)
        }
        PresentationScreenView::ConfigEditor {
            operation,
            editor,
            dirty,
            validated,
            ..
        } => config_editor_rows("Config", *operation, editor, *dirty, *validated, None),
        PresentationScreenView::ProviderWizard {
            operation,
            editor,
            dirty,
            validated,
            connection,
            ..
        } => config_editor_rows(
            "Provider",
            *operation,
            editor,
            *dirty,
            *validated,
            Some(connection),
        ),
        PresentationScreenView::ModelSelector {
            query,
            group,
            selected,
            detail,
        } => model_selector_rows(&view.models, query, *group, *selected, *detail),
        PresentationScreenView::Stats {
            group,
            selected,
            detail,
        } => stats_rows(&view.stats, *group, *selected, *detail),
        PresentationScreenView::AgentCenter { selected, detail } => {
            agent_center_rows(&view.agents, *selected, *detail)
        }
    }
}

fn agent_center_rows(
    agents: &Availability<AgentCenterView>,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let Availability::Known(agents) = agents else {
        return vec![
            LayoutRowView::new("Agents", false),
            LayoutRowView::new("Agent Team unavailable", false),
        ];
    };
    if detail && !agents.agents.is_empty() {
        return agent_detail_rows(
            agents,
            &agents.agents[selected.min(agents.agents.len() - 1)],
        );
    }

    let mut rows = vec![
        LayoutRowView::new("Agents", false),
        LayoutRowView::new(
            format!(
                "revision {} | transactions {} | sequence {}",
                agents.revision, agents.ledger_transaction, agents.ledger_sequence
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "{} agents | {} messages | {} operations awaiting acknowledgement",
                agents.agents.len(),
                agents.message_count,
                agents.operations_awaiting_acknowledgement
            ),
            false,
        ),
    ];
    if agents.recovered_tail_bytes > 0 {
        rows.push(LayoutRowView::new(
            format!(
                "recovery required | incomplete tail {} bytes",
                agents.recovered_tail_bytes
            ),
            false,
        ));
    }
    if agents.agents.is_empty() {
        rows.push(LayoutRowView::new("No agents", false));
        return rows;
    }
    let selected = selected.min(agents.agents.len() - 1);
    rows.extend(agents.agents.iter().enumerate().map(|(index, agent)| {
        LayoutRowView::new(
            format!(
                "{} agent {} | {} | task {} | {}",
                if index == selected { '>' } else { ' ' },
                agent.id,
                agent.status,
                agent.task,
                agent.task_status
            ),
            index == selected,
        )
    }));
    rows
}

fn agent_detail_rows(agents: &AgentCenterView, agent: &AgentCenterEntryView) -> Vec<LayoutRowView> {
    vec![
        LayoutRowView::new("Agents / Agent", false),
        LayoutRowView::new(format!("agent {}", agent.id), false),
        LayoutRowView::new(
            format!(
                "parent {} | status {}",
                agent
                    .parent
                    .map_or_else(|| "root".to_owned(), |parent| parent.to_string()),
                agent.status
            ),
            false,
        ),
        LayoutRowView::new(format!("task {}", agent.task), false),
        LayoutRowView::new(
            format!(
                "task status {} | {} dependencies",
                agent.task_status, agent.dependency_count
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "budget {} tokens | {} tool calls",
                agent.token_budget, agent.tool_budget
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "reserved {} tokens | {} tool calls",
                agent.reserved_tokens, agent.reserved_tools
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "{} capabilities | {} scope labels",
                agent.capability_count, agent.scope_count
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "revision {} | {} messages | {} operations awaiting acknowledgement",
                agents.revision, agents.message_count, agents.operations_awaiting_acknowledgement
            ),
            false,
        ),
        LayoutRowView::new(
            if agents.recovered_tail_bytes == 0 {
                "ledger complete".to_owned()
            } else {
                format!(
                    "recovery required | incomplete tail {} bytes",
                    agents.recovered_tail_bytes
                )
            },
            false,
        ),
    ]
}

const fn agent_status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Dormant => "dormant",
        AgentStatus::Active => "active",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Succeeded => "succeeded",
        AgentStatus::Failed => "failed",
        AgentStatus::Cancelled => "cancelled",
    }
}

const fn task_status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Ready => "ready",
        TaskStatus::Running => "running",
        TaskStatus::Blocked { .. } => "blocked",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed { .. } => "failed",
        TaskStatus::Cancelled { .. } => "cancelled",
    }
}

fn model_selector_rows(
    models: &ModelSelectorView,
    query: &str,
    group: ModelSelectorGroup,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let filtered = models.filtered(query);
    let mut rows = vec![
        LayoutRowView::new(format!("Models / {}", group.label()), false),
        LayoutRowView::new(format!("query {query}"), false),
    ];
    let Some(entries) = filtered.group_entries(group) else {
        rows.push(LayoutRowView::new(
            format!("{} unknown", group.label()),
            false,
        ));
        return rows;
    };
    if entries.is_empty() {
        rows.push(LayoutRowView::new("No models", false));
        return rows;
    }
    let selected = selected.min(entries.len() - 1);
    if detail {
        rows.extend(model_detail_rows(&entries[selected]));
        return rows;
    }
    rows.push(LayoutRowView::new(
        format!(
            "Favorites {} | Recent ? | Compatible {} | All {}",
            filtered.favorites.len(),
            filtered
                .group_entries(ModelSelectorGroup::Compatible)
                .map_or_else(|| "?".to_owned(), |entries| entries.len().to_string()),
            filtered.all.len()
        ),
        false,
    ));
    rows.extend(entries.iter().enumerate().map(|(index, entry)| {
        let source = match &entry.choice {
            ModelSelectorChoiceView::ConfiguredPreset { .. } => "configured",
            ModelSelectorChoiceView::ReleaseCatalog { .. } => "release",
        };
        LayoutRowView::new(
            format!(
                "{} [{}] {} / {} / {}",
                if index == selected { '>' } else { ' ' },
                source,
                entry.id(),
                entry.provider(),
                entry.model()
            ),
            index == selected,
        )
    }));
    rows
}

fn model_detail_rows(entry: &ModelSelectorEntryView) -> Vec<LayoutRowView> {
    let mut rows = vec![LayoutRowView::new(format!("model {}", entry.id()), false)];
    match &entry.choice {
        ModelSelectorChoiceView::ConfiguredPreset { preset } => {
            rows.extend([
                LayoutRowView::new("source configured", false),
                LayoutRowView::new(format!("provider {}", preset.provider), false),
                LayoutRowView::new(format!("model {}", preset.model), false),
                LayoutRowView::new(format!("dialect {}", preset.dialect.as_str()), false),
                LayoutRowView::new(
                    format!(
                        "reasoning {}",
                        preset.reasoning_effort.map_or("?", |value| value.as_str())
                    ),
                    false,
                ),
                LayoutRowView::new(
                    format!(
                        "service tier {}",
                        preset.service_tier.map_or("?", |value| value.as_str())
                    ),
                    false,
                ),
                LayoutRowView::new(
                    format!(
                        "max output tokens {}",
                        preset
                            .max_output_tokens
                            .map_or_else(|| "?".to_owned(), |value| value.to_string())
                    ),
                    false,
                ),
                LayoutRowView::new(
                    format!("context {}", preset.context_mode.as_deref().unwrap_or("?")),
                    false,
                ),
                LayoutRowView::new(format!("favorite {}", preset.favorite), false),
                LayoutRowView::new(
                    format!(
                        "fallback {}",
                        if preset.fallback.is_empty() {
                            "none".to_owned()
                        } else {
                            preset.fallback.join(", ")
                        }
                    ),
                    false,
                ),
            ]);
        }
        ModelSelectorChoiceView::ReleaseCatalog { model } => {
            let record = model.record();
            let capabilities = record.capabilities().value().map_or_else(
                || "?".to_owned(),
                |capabilities| {
                    capabilities
                        .iter()
                        .map(|capability| match capability {
                            ModelCapability::ImageInput => "image_input",
                            ModelCapability::Reasoning => "reasoning",
                            ModelCapability::ToolCalling => "tool_calling",
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            );
            rows.extend([
                LayoutRowView::new("source release", false),
                LayoutRowView::new(format!("display {}", record.display_name().value()), false),
                LayoutRowView::new(format!("provider {}", model.provider()), false),
                LayoutRowView::new(format!("model {}", record.model_id().value()), false),
                LayoutRowView::new(format!("template {}", record.provider_template()), false),
                LayoutRowView::new(
                    format!("dialect {}", record.primary_dialect().value().as_str()),
                    false,
                ),
                LayoutRowView::new(
                    format!(
                        "compatibility {}",
                        availability_bool_label(&entry.compatibility)
                    ),
                    false,
                ),
                LayoutRowView::new(
                    format!(
                        "availability {}",
                        availability_bool_label(&entry.availability)
                    ),
                    false,
                ),
                LayoutRowView::new(
                    format!(
                        "context tokens {}",
                        record
                            .context_window_tokens()
                            .value()
                            .map_or_else(|| "?".to_owned(), |value| value.to_string())
                    ),
                    false,
                ),
                LayoutRowView::new(format!("capabilities {capabilities}"), false),
                LayoutRowView::new(
                    format!(
                        "price schedule {}",
                        record.price_schedule_ref().value().unwrap_or("?")
                    ),
                    false,
                ),
                LayoutRowView::new(format!("seed {}", record.seed_revision()), false),
                LayoutRowView::new(format!("observed {}", record.observed_at()), false),
            ]);
        }
    }
    rows
}

fn availability_bool_label(value: &Availability<bool>) -> &'static str {
    match value {
        Availability::Known(true) => "yes",
        Availability::Known(false) => "no",
        Availability::Unknown => "?",
    }
}

fn stats_rows(
    stats: &Availability<RuntimeUsageSnapshot>,
    group: StatsGroup,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let Availability::Known(stats) = stats else {
        return vec![
            LayoutRowView::new("Stats", false),
            LayoutRowView::new("Usage unavailable", false),
        ];
    };
    if group == StatsGroup::Thread {
        return stats_thread_rows(stats, detail);
    }
    if group == StatsGroup::Agent {
        return stats_agent_rows(stats, selected, detail);
    }
    if group == StatsGroup::Team {
        return stats_team_rows(stats, detail);
    }
    if group == StatsGroup::NamedWindow {
        return stats_named_window_rows(stats, selected, detail);
    }
    if group == StatsGroup::TokenCache {
        return stats_token_cache_rows(stats, selected, detail);
    }
    let attempts = stats.attempts();
    if detail && !attempts.is_empty() {
        return stats_attempt_detail_rows(&attempts[selected.min(attempts.len() - 1)]);
    }

    let mut rows = vec![
        LayoutRowView::new("Stats", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
        LayoutRowView::new(stats_rollup_label("1h", stats.rolling().one_hour()), false),
        LayoutRowView::new(stats_rollup_label("1d", stats.rolling().one_day()), false),
        LayoutRowView::new(
            stats_rollup_label("7d", stats.rolling().seven_days()),
            false,
        ),
        LayoutRowView::new(format!("Attempts {}", attempts.len()), false),
    ];
    if attempts.is_empty() {
        rows.push(LayoutRowView::new("No attempts", false));
        return rows;
    }

    let selected = selected.min(attempts.len() - 1);
    rows.extend(attempts.iter().enumerate().map(|(index, attempt)| {
        LayoutRowView::new(
            format!(
                "{} attempt {} | turn {} | {} | {}/{}",
                if index == selected { '>' } else { ' ' },
                attempt.attempt(),
                attempt.turn(),
                usage_attempt_outcome_label(attempt.outcome()),
                attempt.provider_profile(),
                attempt.requested_model()
            ),
            index == selected,
        )
    }));
    rows
}

fn stats_token_cache_rows(
    stats: &RuntimeUsageSnapshot,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let rollups = [
        ("1h", stats.rolling().one_hour()),
        ("1d", stats.rolling().one_day()),
        ("7d", stats.rolling().seven_days()),
    ];
    let selected = selected.min(rollups.len() - 1);
    let (period, rollup) = rollups[selected];
    if detail {
        return vec![
            LayoutRowView::new("Stats / Token & Cache", false),
            LayoutRowView::new(format!("period {period}"), false),
            LayoutRowView::new(
                format!(
                    "input {}",
                    usage_quantity_label(&rollup.input_tokens().into())
                ),
                false,
            ),
            LayoutRowView::new(
                format!(
                    "cached input {}",
                    usage_quantity_label(&rollup.cached_input_tokens().into())
                ),
                false,
            ),
            LayoutRowView::new(
                format!(
                    "cache write input {}",
                    usage_quantity_label(&rollup.cache_write_input_tokens().into())
                ),
                false,
            ),
            LayoutRowView::new(
                format!(
                    "output {}",
                    usage_quantity_label(&rollup.output_tokens().into())
                ),
                false,
            ),
            LayoutRowView::new(
                format!(
                    "reasoning output {}",
                    usage_quantity_label(&rollup.reasoning_output_tokens().into())
                ),
                false,
            ),
            LayoutRowView::new(
                format!(
                    "total {}",
                    usage_quantity_label(&rollup.total_tokens().into())
                ),
                false,
            ),
        ];
    }

    let mut rows = vec![
        LayoutRowView::new("Stats / Token & Cache", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
    ];
    rows.extend(
        rollups
            .into_iter()
            .enumerate()
            .map(|(index, (period, rollup))| {
                let is_selected = index == selected;
                LayoutRowView::new(
                    format!(
                        "{} {period} input {} | cached {} | write {} | total {}",
                        if is_selected { '>' } else { ' ' },
                        usage_quantity_label(&rollup.input_tokens().into()),
                        usage_quantity_label(&rollup.cached_input_tokens().into()),
                        usage_quantity_label(&rollup.cache_write_input_tokens().into()),
                        usage_quantity_label(&rollup.total_tokens().into())
                    ),
                    is_selected,
                )
            }),
    );
    rows
}

fn stats_named_window_rows(
    stats: &RuntimeUsageSnapshot,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let windows = stats.named_windows();
    if windows.is_empty() {
        return vec![
            LayoutRowView::new("Stats / Named Window", false),
            LayoutRowView::new("No named-window usage", false),
        ];
    }
    let selected = selected.min(windows.len() - 1);
    if detail {
        let window = &windows[selected];
        return stats_rollup_detail_rows(
            "Stats / Named Window",
            format!("window {}", window.window().id()),
            window.usage(),
        );
    }
    let mut rows = vec![
        LayoutRowView::new("Stats / Named Window", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
    ];
    rows.extend(windows.iter().enumerate().map(|(index, window)| {
        let is_selected = index == selected;
        LayoutRowView::new(
            format!(
                "{} {}",
                if is_selected { '>' } else { ' ' },
                stats_rollup_label(&format!("window {}", window.window().id()), window.usage())
            ),
            is_selected,
        )
    }));
    rows
}

fn stats_team_rows(stats: &RuntimeUsageSnapshot, detail: bool) -> Vec<LayoutRowView> {
    let Some(team) = stats.team() else {
        return vec![
            LayoutRowView::new("Stats / Team", false),
            LayoutRowView::new("No Team usage", false),
        ];
    };
    if detail {
        return stats_rollup_detail_rows("Stats / Team", "team usage".to_owned(), team);
    }
    vec![
        LayoutRowView::new("Stats / Team", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
        LayoutRowView::new(format!("> {}", stats_rollup_label("team", team)), true),
    ]
}

fn stats_agent_rows(
    stats: &RuntimeUsageSnapshot,
    selected: usize,
    detail: bool,
) -> Vec<LayoutRowView> {
    let agents = stats.agents();
    if agents.is_empty() {
        return vec![
            LayoutRowView::new("Stats / Agent", false),
            LayoutRowView::new("No Agent usage", false),
        ];
    }
    let selected = selected.min(agents.len() - 1);
    if detail {
        let agent = &agents[selected];
        return stats_rollup_detail_rows(
            "Stats / Agent",
            format!("agent {}", agent.id()),
            agent.usage(),
        );
    }
    let mut rows = vec![
        LayoutRowView::new("Stats / Agent", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
    ];
    rows.extend(agents.iter().enumerate().map(|(index, agent)| {
        let is_selected = index == selected;
        LayoutRowView::new(
            format!(
                "{} {}",
                if is_selected { '>' } else { ' ' },
                stats_rollup_label(&format!("agent {}", agent.id()), agent.usage())
            ),
            is_selected,
        )
    }));
    rows
}

fn stats_thread_rows(stats: &RuntimeUsageSnapshot, detail: bool) -> Vec<LayoutRowView> {
    let Some(thread) = stats.thread() else {
        return vec![
            LayoutRowView::new("Stats / Thread", false),
            LayoutRowView::new("No current thread", false),
        ];
    };
    if detail {
        return stats_rollup_detail_rows(
            "Stats / Thread",
            format!("thread {}", thread.id()),
            thread.usage(),
        );
    }
    vec![
        LayoutRowView::new("Stats / Thread", false),
        LayoutRowView::new(format!("as of {} ms", stats.as_of().unix_millis()), false),
        LayoutRowView::new(
            format!(
                "> {}",
                stats_rollup_label(&format!("thread {}", thread.id()), thread.usage())
            ),
            true,
        ),
    ]
}

fn stats_rollup_detail_rows(
    title: &'static str,
    identity: String,
    rollup: &UsageRollup,
) -> Vec<LayoutRowView> {
    let summary = UsageSummaryView::from(rollup);
    vec![
        LayoutRowView::new(title, false),
        LayoutRowView::new(identity, false),
        LayoutRowView::new(
            format!(
                "attempts {} | succeeded {} | failed {} | interrupted {}",
                rollup.attempts(),
                rollup.succeeded(),
                rollup.failed(),
                rollup.interrupted()
            ),
            false,
        ),
        LayoutRowView::new(format!("usage records {}", rollup.usage_records()), false),
        LayoutRowView::new(
            format!(
                "tokens input {} | output {} | total {}",
                usage_quantity_label(&rollup.input_tokens().into()),
                usage_quantity_label(&rollup.output_tokens().into()),
                usage_quantity_label(&summary.total_tokens)
            ),
            false,
        ),
        LayoutRowView::new(cost_label(&Availability::Known(summary)), false),
    ]
}

fn stats_rollup_label(label: &str, rollup: &UsageRollup) -> String {
    let summary = UsageSummaryView::from(rollup);
    let cost = cost_label(&Availability::Known(summary.clone()));
    format!(
        "{label} {} attempts | {} tokens | {}",
        summary.attempts,
        usage_quantity_label(&summary.total_tokens),
        cost
    )
}

fn usage_quantity_label(quantity: &UsageQuantityView) -> String {
    if quantity.overflowed {
        return "overflow".to_owned();
    }
    let amount = match (quantity.exact, quantity.estimated) {
        (Some(exact), Some(estimated)) if estimated > 0 => format!("{exact}+~{estimated}"),
        (Some(exact), _) => exact.to_string(),
        (None, Some(estimated)) => format!("~{estimated}"),
        (None, None) if quantity.unknown_records == 0 => "0".to_owned(),
        (None, None) => "?".to_owned(),
    };
    if quantity.unknown_records == 0 || amount == "?" {
        amount
    } else {
        format!("{amount}+?")
    }
}

fn stats_attempt_detail_rows(attempt: &UsageAttempt) -> Vec<LayoutRowView> {
    let usage = attempt.usage();
    let cost = attempt.cost_estimate();
    vec![
        LayoutRowView::new("Stats / Attempt", false),
        LayoutRowView::new(format!("attempt {}", attempt.attempt()), false),
        LayoutRowView::new(
            format!(
                "thread {} | turn {} | agent {}",
                attempt.thread(),
                attempt.turn(),
                attempt
                    .agent()
                    .map_or_else(|| "?".to_owned(), |value| value.to_string())
            ),
            false,
        ),
        LayoutRowView::new(
            format!("outcome {}", usage_attempt_outcome_label(attempt.outcome())),
            false,
        ),
        LayoutRowView::new(format!("provider {}", attempt.provider_profile()), false),
        LayoutRowView::new(
            format!("requested model {}", attempt.requested_model()),
            false,
        ),
        LayoutRowView::new(
            format!("observed model {}", attempt.observed_model().unwrap_or("?")),
            false,
        ),
        LayoutRowView::new(
            format!(
                "dialect {}",
                attempt.dialect().map_or("?", |value| value.as_str())
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "reasoning requested {} | observed {}",
                attempt.requested_reasoning_effort().unwrap_or("?"),
                attempt.observed_reasoning_effort().unwrap_or("?")
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "service tier requested {} | observed {}",
                attempt.requested_service_tier().unwrap_or("?"),
                attempt.observed_service_tier().unwrap_or("?")
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "tokens input {} | output {} | total {}",
                usage
                    .and_then(|record| record.input_tokens())
                    .map_or_else(|| "?".to_owned(), |value| value.to_string()),
                usage
                    .and_then(|record| record.output_tokens())
                    .map_or_else(|| "?".to_owned(), |value| value.to_string()),
                usage
                    .and_then(|record| record.total_tokens())
                    .map_or_else(|| "?".to_owned(), |value| value.to_string())
            ),
            false,
        ),
        LayoutRowView::new(
            cost.map_or_else(
                || "cost ?".to_owned(),
                |cost| {
                    format!(
                        "cost {} {}",
                        cost.currency(),
                        format_cost(cost.amount_pico_units(), cost.scale_decimal_places())
                    )
                },
            ),
            false,
        ),
        LayoutRowView::new(
            format!(
                "started {} | completed {}",
                attempt
                    .started_at()
                    .map_or_else(|| "?".to_owned(), |value| value.unix_millis().to_string()),
                attempt
                    .completed_at()
                    .map_or_else(|| "?".to_owned(), |value| value.unix_millis().to_string())
            ),
            false,
        ),
    ]
}

const fn usage_attempt_outcome_label(outcome: UsageAttemptOutcome) -> &'static str {
    match outcome {
        UsageAttemptOutcome::Succeeded => "succeeded",
        UsageAttemptOutcome::Failed => "failed",
        UsageAttemptOutcome::Interrupted => "interrupted",
    }
}

fn config_delete_rows(
    object: Option<&ConfigObjectRef>,
    dirty: bool,
    validated: bool,
) -> Vec<LayoutRowView> {
    let target = object.map_or_else(
        || "Config Object".to_owned(),
        |object| format!("{} {}", config_object_label(object.kind()), object.id()),
    );
    vec![
        LayoutRowView::new(format!("Delete {target}"), false),
        LayoutRowView::new("> Confirm deletion", true),
        LayoutRowView::new(
            match (dirty, validated) {
                (false, _) => "draft clean",
                (true, true) => "draft validated",
                (true, false) => "draft pending",
            },
            false,
        ),
    ]
}

fn config_editor_rows(
    title: &str,
    operation: ConfigEditorOperation,
    editor: &ConfigEditorView,
    dirty: bool,
    validated: bool,
    connection: Option<&ProviderConnectionTestStatus>,
) -> Vec<LayoutRowView> {
    let mut rows = vec![LayoutRowView::new(
        format!(
            "{title} {} / {}",
            config_editor_operation_label(operation),
            editor.field.path
        ),
        false,
    )];
    match &editor.field.contents {
        ConfigFieldContents::Value {
            effective, target, ..
        } => {
            let interaction = editor.field.interaction;
            rows.push(LayoutRowView::new(
                format!("effective {}", config_value_label(effective.as_ref())),
                false,
            ));
            let text_input_selected = matches!(interaction, ConfigFieldInteraction::Text { .. });
            rows.push(LayoutRowView::new(
                format!(
                    "{}target {}",
                    if text_input_selected { "> " } else { "" },
                    config_value_label(target.as_ref())
                ),
                text_input_selected,
            ));
            if let ConfigFieldInteraction::Choice { choices } = interaction {
                let selected = target.as_ref().or(effective.as_ref()).and_then(|value| {
                    if let ConfigValue::String(value) = value {
                        Some(value.as_str())
                    } else {
                        None
                    }
                });
                rows.extend(choices.iter().copied().map(|choice| {
                    let is_selected = selected == Some(choice);
                    LayoutRowView::new(
                        format!("{} {choice}", if is_selected { '>' } else { ' ' }),
                        is_selected,
                    )
                }));
            }
        }
        ConfigFieldContents::CredentialBinding {
            effective_bound,
            target_bound,
            ..
        } => {
            rows.push(LayoutRowView::new(
                format!(
                    "credential {}",
                    if *effective_bound { "bound" } else { "missing" }
                ),
                false,
            ));
            let input_selected = matches!(
                editor.field.interaction,
                ConfigFieldInteraction::CredentialReference { .. }
            );
            rows.push(LayoutRowView::new(
                format!(
                    "{}target {}",
                    if input_selected { "> " } else { "" },
                    if *target_bound { "bound" } else { "missing" }
                ),
                input_selected,
            ));
        }
    }
    rows.push(LayoutRowView::new(
        match (dirty, validated) {
            (false, _) => "draft clean",
            (true, true) => "draft validated",
            (true, false) => "draft pending",
        },
        false,
    ));
    if let Some(connection) = connection {
        rows.push(LayoutRowView::new(
            match connection {
                ProviderConnectionTestStatus::Untested => "connection untested",
                ProviderConnectionTestStatus::Succeeded { .. } => "connection succeeded",
                ProviderConnectionTestStatus::Failed {
                    retryable: true, ..
                } => "connection failed (retryable)",
                ProviderConnectionTestStatus::Failed {
                    retryable: false, ..
                } => "connection failed",
            },
            false,
        ));
    }
    rows
}

fn config_value_label(value: Option<&ConfigValue>) -> String {
    match value {
        Some(ConfigValue::String(value)) => value.clone(),
        Some(ConfigValue::PositiveInteger(value)) => value.to_string(),
        Some(ConfigValue::NonNegativeInteger(value)) => value.to_string(),
        Some(ConfigValue::Boolean(value)) => value.to_string(),
        Some(ConfigValue::StringList(value)) => value.join(", "),
        None => "inherited".to_owned(),
    }
}

fn config_editor_operation_label(operation: ConfigEditorOperation) -> &'static str {
    match operation {
        ConfigEditorOperation::Edit => "edit",
        ConfigEditorOperation::Create => "create",
        ConfigEditorOperation::Delete => "delete",
    }
}

fn config_section_label(section: ConfigSection) -> &'static str {
    match section {
        ConfigSection::Provider => "provider",
        ConfigSection::Model => "model",
        ConfigSection::Pricing => "pricing",
        ConfigSection::Statusline => "statusline",
        ConfigSection::StatsWindow => "stats-window",
        ConfigSection::Agent => "agent",
        ConfigSection::Skills => "skills",
        ConfigSection::Mcp => "mcp",
        ConfigSection::Security => "security",
    }
}

fn config_object_label(kind: ConfigObjectKind) -> &'static str {
    match kind {
        ConfigObjectKind::ProviderProfile => "provider",
        ConfigObjectKind::ModelPreset => "model",
        ConfigObjectKind::PriceSchedule => "pricing",
        ConfigObjectKind::UsageWindow => "stats-window",
    }
}

fn object_section(kind: ConfigObjectKind) -> ConfigSection {
    match kind {
        ConfigObjectKind::ProviderProfile => ConfigSection::Provider,
        ConfigObjectKind::ModelPreset => ConfigSection::Model,
        ConfigObjectKind::PriceSchedule => ConfigSection::Pricing,
        ConfigObjectKind::UsageWindow => ConfigSection::StatsWindow,
    }
}

fn config_center_objects(
    runtime: &ConfigRuntime,
    section: Option<ConfigSection>,
) -> Result<Vec<ConfigObjectRef>, ConfigRuntimeError> {
    let mut objects = runtime.addressable_objects()?;
    if let Some(section) = section {
        objects.retain(|object| object_section(object.kind()) == section);
    }
    Ok(objects)
}

fn config_section_for_path_pattern(path_pattern: &str) -> Option<ConfigSection> {
    if path_pattern.starts_with("providers.<id>.") {
        Some(ConfigSection::Provider)
    } else if path_pattern.starts_with("model_presets.<id>.") {
        Some(ConfigSection::Model)
    } else if path_pattern.starts_with("price_schedules.<id>.") {
        Some(ConfigSection::Pricing)
    } else if path_pattern.starts_with("stats.windows.<id>.") {
        Some(ConfigSection::StatsWindow)
    } else {
        None
    }
}

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(sanitize_controls(text).as_ref())
}

pub(crate) fn fit_text(text: &str, max_width: usize) -> String {
    let text = sanitize_controls(text);
    if UnicodeWidthStr::width(text.as_ref()) <= max_width {
        return text.into_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let content_width = max_width - 1;
    let mut fitted = String::new();
    for grapheme in UnicodeSegmentation::graphemes(text.as_ref(), true) {
        if display_width(&fitted) + display_width(grapheme) > content_width {
            break;
        }
        fitted.push_str(grapheme);
    }
    let fitted = fitted.trim_end();
    format!("{fitted}…")
}

fn sanitize_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|character| {
                if character.is_control() {
                    '?'
                } else {
                    character
                }
            })
            .collect(),
    )
}

#[derive(Debug)]
pub(crate) enum PresentationControllerError {
    Command(CommandQueryError),
    ConfigEditor(ConfigEditorError),
    Config(ConfigRuntimeError),
    NoCommandSelection,
    NotSlashPanel,
    NotConfigCenter,
    NotConfigObjectCreate,
    NotConfigEditor,
    NotProviderWizard,
    ConfigRuntimeRequired,
    UnsavedConfigDraft,
    NoConfigObjectSelection,
    ConfigObjectRouteUnavailable,
}

impl fmt::Display for PresentationControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(source) => write!(formatter, "{source}"),
            Self::ConfigEditor(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::NoCommandSelection => formatter.write_str("no command is selected"),
            Self::NotSlashPanel => formatter.write_str("command requires the Slash Panel"),
            Self::NotConfigCenter => formatter.write_str("command requires the Config Center"),
            Self::NotConfigObjectCreate => {
                formatter.write_str("command requires a Config Object name prompt")
            }
            Self::NotConfigEditor => formatter.write_str("command requires a Config editor"),
            Self::NotProviderWizard => {
                formatter.write_str("command requires a Provider Profile wizard")
            }
            Self::ConfigRuntimeRequired => {
                formatter.write_str("Config Center requires the Config Runtime")
            }
            Self::UnsavedConfigDraft => {
                formatter.write_str("Config draft must be committed or discarded")
            }
            Self::NoConfigObjectSelection => formatter.write_str("no Config Object is selected"),
            Self::ConfigObjectRouteUnavailable => {
                formatter.write_str("Config Object has no rendered editor route")
            }
        }
    }
}

impl Error for PresentationControllerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::ConfigEditor(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::NoCommandSelection
            | Self::NotSlashPanel
            | Self::NotConfigCenter
            | Self::NotConfigObjectCreate
            | Self::NotConfigEditor
            | Self::NotProviderWizard
            | Self::ConfigRuntimeRequired
            | Self::UnsavedConfigDraft
            | Self::NoConfigObjectSelection
            | Self::ConfigObjectRouteUnavailable => None,
        }
    }
}

impl From<CommandQueryError> for PresentationControllerError {
    fn from(source: CommandQueryError) -> Self {
        Self::Command(source)
    }
}

impl From<ConfigEditorError> for PresentationControllerError {
    fn from(source: ConfigEditorError) -> Self {
        Self::ConfigEditor(source)
    }
}

impl From<ConfigRuntimeError> for PresentationControllerError {
    fn from(source: ConfigRuntimeError) -> Self {
        Self::Config(source)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PresentationSmokeView {
    #[serde(flatten)]
    view: TuiViewModel,
    layouts: Vec<PresentationLayoutView>,
}

#[cfg(test)]
impl PresentationSmokeView {
    pub(crate) const fn view(&self) -> &TuiViewModel {
        &self.view
    }

    pub(crate) fn layouts(&self) -> &[PresentationLayoutView] {
        &self.layouts
    }
}

pub(crate) fn build_smoke_view(
    query: &str,
) -> Result<PresentationSmokeView, PresentationSmokeError> {
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
    let view = TuiViewModel::build(
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
            context_pressure: None,
            model_presets: &[],
            catalog_models: &[],
        },
    )?;
    let mut controller = PresentationController::new();
    controller.set_slash_query(query)?;
    let layouts = [(40, 12), (80, 24), (160, 50)]
        .into_iter()
        .map(|(width, height)| -> Result<_, PresentationSmokeError> {
            let viewport = Viewport::new(width, height)?;
            Ok(controller.layout(None, &view, viewport)?)
        })
        .collect::<Result<Vec<_>, PresentationSmokeError>>()?;
    Ok(PresentationSmokeView { view, layouts })
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
    InvalidModelQuery,
}

impl fmt::Display for PresentationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(source) => write!(formatter, "{source}"),
            Self::InvalidModelQuery => formatter.write_str("model query is invalid"),
        }
    }
}

impl Error for PresentationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::InvalidModelQuery => None,
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
    Controller(PresentationControllerError),
    Viewport(ViewportError),
}

impl fmt::Display for PresentationSmokeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Presentation(source) => write!(formatter, "{source}"),
            Self::Controller(source) => write!(formatter, "{source}"),
            Self::Viewport(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for PresentationSmokeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Presentation(source) => Some(source),
            Self::Controller(source) => Some(source),
            Self::Viewport(source) => Some(source),
        }
    }
}

impl From<PresentationError> for PresentationSmokeError {
    fn from(source: PresentationError) -> Self {
        Self::Presentation(source)
    }
}

impl From<PresentationControllerError> for PresentationSmokeError {
    fn from(source: PresentationControllerError) -> Self {
        Self::Controller(source)
    }
}

impl From<ViewportError> for PresentationSmokeError {
    fn from(source: ViewportError) -> Self {
        Self::Viewport(source)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use greentyper_core::agent_team::{
        EventSeq, TaskId, TaskScope, TaskStatus, TaskView, TeamOperationId, TeamOperationRecord,
        TeamOperationStatus, TeamSnapshot, TransactionId,
    };
    use greentyper_core::config::{
        CommandMatchKind, CommandTarget, ConfigDocument, ConfigEditorError, ConfigEditorOperation,
        ConfigEditorSession, ConfigEditorView, ConfigErrorCategory, ConfigFieldContents,
        ConfigFieldView, ConfigObjectKind, ConfigObjectRef, ConfigPaths, ConfigRepairIssue,
        ConfigRuntime, ConfigRuntimeError, ConfigRuntimeStatus, ConfigScope, ConfigSection,
        ConfigValue, ModelPresetView,
    };
    use greentyper_core::context::{
        ContextPressure, ContextPressureAccuracy, ContextPressureInput, ContextPressurePolicy,
        ContextPressureState,
    };
    use greentyper_core::ledger::LedgerHead;
    use greentyper_core::model::{DeliveryId, ThreadId, TurnId};
    use greentyper_core::provider::ProviderDialect;
    use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};

    use crate::provider_connection::{ProviderConnectionTestStatus, ProviderConnectionTester};

    use super::{
        Availability, BlockerView, CostQuantityView, ModelSelectorGroup, ModelSelectorView,
        PresentationController, PresentationControllerError, PresentationScreenView,
        PresentationSources, RecoveryBadge, SlashPanelView, StatusSegmentKind, TuiViewModel,
        UsageQuantityView, UsageSummaryView, Viewport, cost_label, display_width, fit_text,
        model_detail_rows, model_selector_rows, status_segments,
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
        let mut loser = PresentationController::new();
        loser
            .set_slash_query("/config provider url")
            .expect("set loser editor route");
        loser
            .activate(&mut loser_runtime, ConfigScope::Project, Some(&object))
            .expect("open loser editor");
        winner
            .stage_raw("https://winner.example.com/v1")
            .expect("stage winner");
        loser
            .stage_config(&loser_runtime, "https://loser.example.com/v1")
            .expect("stage loser");
        winner.commit(&mut winner_runtime).expect("commit winner");
        loser_runtime.reload().expect("observe winner revision");

        let mut tester = SuccessfulConnectionTester { calls: Vec::new() };
        assert!(matches!(
            loser.test_provider_connection(&loser_runtime, &mut tester),
            Err(PresentationControllerError::ConfigEditor(
                ConfigEditorError::Config(ConfigRuntimeError::RevisionConflict { .. })
            ))
        ));
        assert!(
            tester.calls.is_empty(),
            "stale draft must not reach network"
        );

        assert!(matches!(
            loser.commit_config(&mut loser_runtime),
            Err(PresentationControllerError::ConfigEditor(
                ConfigEditorError::Config(ConfigRuntimeError::RevisionConflict { .. })
            ))
        ));
        assert!(matches!(
            loser
                .screen(Some(&loser_runtime))
                .expect("stale editor remains visible"),
            PresentationScreenView::ProviderWizard { dirty: true, .. }
        ));
        loser
            .stage_config(&loser_runtime, "https://still-live.example.com/v1")
            .expect("stale editor session remains live after conflict");
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
                context_pressure: None,
                model_presets: &[],
                catalog_models: &[],
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
                context_pressure: None,
                model_presets: &[],
                catalog_models: &[],
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
    fn statusline_marks_estimated_context_pressure_without_inventing_exactness() {
        let runtime = runtime(RecoveryStatus::Ready);
        let config = ConfigRuntimeStatus {
            ready: true,
            issues: Vec::new(),
        };
        let pressure = ContextPressure::project(
            ContextPressureInput::known(
                200_000,
                100_000,
                30_000,
                ContextPressureAccuracy::Estimated,
            ),
            ContextPressurePolicy::default(),
        )
        .expect("estimated Context Pressure");
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
                context_pressure: Some(&pressure),
                model_presets: &[],
                catalog_models: &[],
            },
        )
        .expect("view model");

        assert_eq!(
            view.statusline.context_pressure_percent,
            Availability::Known(65)
        );
        assert!(matches!(
            &view.statusline.context_pressure,
            Availability::Known(snapshot)
                if snapshot.accuracy() == Some(ContextPressureAccuracy::Estimated)
                    && snapshot.state() == ContextPressureState::Soft
        ));
        assert_eq!(
            status_segments(&view.statusline)
                .into_iter()
                .find(|segment| segment.kind == StatusSegmentKind::Context)
                .map(|segment| segment.text),
            Some("ctx ~65%".to_owned())
        );
    }

    #[test]
    fn statusline_formats_known_payg_cost_without_hiding_unknown_attempts() {
        let summary = UsageSummaryView {
            attempts: 2,
            total_tokens: UsageQuantityView {
                exact: Some(120),
                estimated: Some(0),
                unknown_records: 0,
                overflowed: false,
            },
            payg_cost_estimates: BTreeMap::from([(
                "USD".to_owned(),
                CostQuantityView {
                    scale_decimal_places: 12,
                    exact_pico_units: Some(202),
                    estimated_pico_units: Some(0),
                    records: 1,
                    overflowed: false,
                },
            )]),
            cost_unknown_attempts: 1,
        };
        assert_eq!(
            cost_label(&Availability::Known(summary.clone())),
            "USD 0.000000000202+?"
        );

        let mut estimated = summary.clone();
        estimated.cost_unknown_attempts = 0;
        estimated
            .payg_cost_estimates
            .get_mut("USD")
            .expect("USD estimate")
            .exact_pico_units = Some(0);
        estimated
            .payg_cost_estimates
            .get_mut("USD")
            .expect("USD estimate")
            .estimated_pico_units = Some(303);
        assert_eq!(
            cost_label(&Availability::Known(estimated)),
            "USD ~0.000000000303"
        );

        let mut overflowed = summary;
        overflowed.cost_unknown_attempts = 0;
        overflowed
            .payg_cost_estimates
            .get_mut("USD")
            .expect("USD estimate")
            .overflowed = true;
        assert_eq!(cost_label(&Availability::Known(overflowed)), "USD overflow");
    }

    #[test]
    fn model_selector_filters_configured_presets_without_inventing_compatibility() {
        let presets = vec![
            ModelPresetView {
                id: "fast".into(),
                provider: "edge".into(),
                model: "fixture-model".into(),
                dialect: ProviderDialect::Responses,
                reasoning_effort: Some(greentyper_core::config::ReasoningEffort::Medium),
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
                context_pressure: None,
                model_presets: &presets,
                catalog_models: &[],
            },
        )
        .expect("model selector")
        .models;
        assert_eq!(selector.all.len(), 1);
        assert_eq!(selector.all[0].id(), "fast");
        assert_eq!(selector.favorites.len(), 1);
        assert_eq!(selector.recent, Availability::Unknown);
        assert_eq!(selector.compatible, Availability::Unknown);
        assert_eq!(selector.all[0].compatibility, Availability::Unknown);
        assert_eq!(
            model_selector_rows(&selector, "", ModelSelectorGroup::Recent, 0, false)
                .into_iter()
                .map(|row| row.text)
                .collect::<Vec<_>>(),
            vec![
                "Models / Recent".to_owned(),
                "query ".to_owned(),
                "Recent unknown".to_owned(),
            ]
        );
    }

    #[test]
    fn model_selector_projects_release_catalog_compatibility_without_live_availability() {
        let temp = TempTree::new("catalog-model-selector");
        let config = ConfigDocument::parse(
            r#"
schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-openai-credential-reference"

[providers.deepseek-main]
template = "deepseek"
credential = "synthetic-deepseek-credential-reference"
"#,
        )
        .expect("parse official provider profile");
        let runtime = ConfigRuntime::open(temp.paths(), config).expect("open Config Runtime");
        let catalog_models = runtime.catalog_models().expect("catalog model candidates");

        let selector = ModelSelectorView::build(&[], &catalog_models, "terra")
            .expect("catalog-backed selector");
        assert!(selector.favorites.is_empty());
        assert_eq!(selector.recent, Availability::Unknown);
        assert_eq!(selector.all.len(), 1);
        let entry = &selector.all[0];
        assert!(entry.is_release_catalog());
        assert_eq!(entry.id(), "openai/gpt-5.6-terra");
        assert_eq!(entry.provider(), "openai-main");
        assert_eq!(entry.model(), "gpt-5.6-terra");
        assert_eq!(entry.compatibility, Availability::Known(true));
        assert_eq!(entry.availability, Availability::Unknown);
        assert_eq!(
            selector.compatible,
            Availability::Known(selector.all.clone())
        );

        let encoded = serde_json::to_string(&selector).expect("serialize model selector");
        assert!(encoded.contains("release_catalog"));
        assert!(encoded.contains("2026-08-10.2"));
        assert!(!encoded.contains("synthetic-openai-credential-reference"));
        let detail = model_detail_rows(entry)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(detail.contains("source release"));
        assert!(detail.contains("availability ?"));
        assert!(detail.contains("seed 2026-08-10.2"));
        assert!(!detail.contains("synthetic-openai-credential-reference"));

        let deepseek = ModelSelectorView::build(&[], &catalog_models, "deepseek-v4-pro")
            .expect("catalog model with DeepSeek Chat adapter");
        assert_eq!(deepseek.all.len(), 1);
        assert_eq!(deepseek.all[0].compatibility, Availability::Known(true));
        assert_eq!(
            deepseek.compatible,
            Availability::Known(deepseek.all.clone())
        );
        let encoded = serde_json::to_string(&deepseek).expect("serialize DeepSeek selector");
        assert!(!encoded.contains("synthetic-deepseek-credential-reference"));

        let deepseek_responses =
            ModelSelectorView::build(&[], &catalog_models, "deepseek-v4-flash")
                .expect("catalog model with DeepSeek Responses adapter");
        assert_eq!(deepseek_responses.all.len(), 1);
        assert_eq!(
            deepseek_responses.all[0].compatibility,
            Availability::Known(true)
        );
        assert_eq!(
            deepseek_responses.compatible,
            Availability::Known(deepseek_responses.all.clone())
        );
    }

    #[test]
    fn controller_creates_and_deletes_a_config_object_through_typed_routes() {
        let temp = TempTree::new("controller-object-lifecycle");
        let mut runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
            .expect("open Config Runtime");
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut controller = PresentationController::new();

        controller
            .set_slash_query("/config provider add")
            .expect("set create route");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("activate create route");
        assert!(matches!(
            controller.screen(Some(&runtime)).expect("create screen"),
            PresentationScreenView::ProviderWizard {
                operation: ConfigEditorOperation::Create,
                dirty: false,
                validated: false,
                ..
            }
        ));
        controller
            .stage_config(&runtime, "openai-compatible")
            .expect("stage provider template");
        controller
            .preview_config(&mut runtime)
            .expect("preview creation");
        controller
            .commit_config(&mut runtime)
            .expect("commit creation");
        assert!(runtime.addressable_objects().unwrap().contains(&object));

        controller
            .set_slash_query("/config provider remove")
            .expect("set delete route");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("activate delete route");
        assert!(matches!(
            controller.screen(Some(&runtime)).expect("delete screen"),
            PresentationScreenView::ConfigEditor {
                operation: ConfigEditorOperation::Delete,
                dirty: true,
                validated: false,
                ..
            }
        ));
        controller
            .preview_config(&mut runtime)
            .expect("preview deletion");
        controller
            .commit_config(&mut runtime)
            .expect("commit deletion");
        assert!(!runtime.addressable_objects().unwrap().contains(&object));
    }

    #[test]
    fn controller_prompts_for_every_rendered_config_object_creation_form() {
        let temp = TempTree::new("controller-rendered-create-boundary");
        let mut runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
            .expect("open Config Runtime");
        let mut controller = PresentationController::new();

        controller
            .set_slash_query("/config provider add")
            .expect("set Provider create route");
        controller
            .activate(&mut runtime, ConfigScope::User, None)
            .expect("open rendered Provider ID prompt");
        assert!(matches!(
            controller.screen(Some(&runtime)).expect("Provider prompt"),
            PresentationScreenView::ConfigObjectCreate {
                kind: ConfigObjectKind::ProviderProfile,
                ref id,
            } if id.is_empty()
        ));

        controller.back().expect("close Provider prompt");
        controller
            .set_slash_query("/config model add")
            .expect("set Model create route");
        controller
            .activate(&mut runtime, ConfigScope::User, None)
            .expect("open rendered Model Preset ID prompt");
        assert!(matches!(
            controller.screen(Some(&runtime)).expect("Model prompt"),
            PresentationScreenView::ConfigObjectCreate {
                kind: ConfigObjectKind::ModelPreset,
                ref id,
            } if id.is_empty()
        ));

        controller.back().expect("close Model prompt");
        controller
            .set_slash_query("/config stats-window add")
            .expect("set Usage Window create route");
        controller
            .activate(&mut runtime, ConfigScope::User, None)
            .expect("open rendered Usage Window ID prompt");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("Usage Window prompt"),
            PresentationScreenView::ConfigObjectCreate {
                kind: ConfigObjectKind::UsageWindow,
                ref id,
            } if id.is_empty()
        ));

        controller.back().expect("close Usage Window prompt");
        controller
            .set_slash_query("/config pricing add")
            .expect("set Price Schedule create route");
        controller
            .activate(&mut runtime, ConfigScope::User, None)
            .expect("open rendered Price Schedule ID prompt");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("Price Schedule prompt"),
            PresentationScreenView::ConfigObjectCreate {
                kind: ConfigObjectKind::PriceSchedule,
                ref id,
            } if id.is_empty()
        ));
    }

    struct SuccessfulConnectionTester {
        calls: Vec<(String, u64)>,
    }

    impl ProviderConnectionTester for SuccessfulConnectionTester {
        fn test(
            &mut self,
            profile: &greentyper_core::provider::ProviderProfileSnapshot,
        ) -> ProviderConnectionTestStatus {
            self.calls
                .push((profile.profile().to_owned(), profile.fingerprint()));
            ProviderConnectionTestStatus::Succeeded {
                profile: profile.profile().to_owned(),
                fingerprint: profile.fingerprint(),
                models: Vec::new(),
            }
        }
    }

    #[test]
    fn provider_wizard_tests_the_current_draft_without_committing_or_exposing_credentials() {
        let temp = TempTree::new("provider-wizard");
        let paths = temp.paths();
        let mut runtime = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
            .expect("open Config Runtime");
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut controller = PresentationController::new();
        controller
            .set_slash_query("/config provider add")
            .expect("set Provider wizard route");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("activate Provider wizard");
        assert!(matches!(
            controller.screen(Some(&runtime)).expect("wizard screen"),
            PresentationScreenView::ProviderWizard {
                connection: ProviderConnectionTestStatus::Untested,
                dirty: false,
                validated: false,
                ..
            }
        ));

        controller
            .stage_config(&runtime, "openai-compatible")
            .expect("stage template");
        controller
            .focus_config_field(&runtime, "/config provider credential", 0)
            .expect("focus credential");
        controller
            .stage_provider_credential_reference(&runtime, "synthetic-edge-credential-reference")
            .expect("stage credential reference");
        for (query, value) in [
            ("/config provider url", "https://wizard.example.com/v1"),
            ("/config provider route responses", "/responses"),
            ("/config provider route models", "/models"),
            ("/config provider dialects", "[\"responses\"]"),
            ("/config provider pricing", "unknown"),
        ] {
            controller
                .focus_config_field(&runtime, query, 0)
                .expect("focus Provider field");
            controller
                .stage_config(&runtime, value)
                .expect("stage Provider field");
        }
        controller
            .preview_config(&mut runtime)
            .expect("validate Provider draft");
        let mut tester = SuccessfulConnectionTester { calls: Vec::new() };
        let status = controller
            .test_provider_connection(&runtime, &mut tester)
            .expect("test Provider connection");
        assert!(matches!(
            status,
            ProviderConnectionTestStatus::Succeeded { .. }
        ));
        assert_eq!(tester.calls.len(), 1);
        assert_eq!(tester.calls[0].0, "edge");
        assert!(
            !paths.project().exists(),
            "connection test must not commit Config"
        );

        let encoded = serde_json::to_string(
            &controller
                .screen(Some(&runtime))
                .expect("tested wizard screen"),
        )
        .expect("serialize Provider wizard");
        assert!(encoded.contains("\"screen\":\"provider_wizard\""));
        assert!(encoded.contains("\"state\":\"succeeded\""));
        assert!(!encoded.contains("synthetic-edge-credential-reference"));

        controller
            .focus_config_field(&runtime, "/config provider template", 0)
            .expect("refocus template");
        controller
            .stage_config(&runtime, "openai-compatible")
            .expect("restage template");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("invalidated wizard screen"),
            PresentationScreenView::ProviderWizard {
                connection: ProviderConnectionTestStatus::Untested,
                ..
            }
        ));
    }

    #[test]
    fn controller_keeps_one_draft_while_focusing_each_new_object_field() {
        let temp = TempTree::new("controller-multi-field-create");
        let mut runtime = temp.open_provider_runtime();
        let object = ConfigObjectRef::new(ConfigObjectKind::ModelPreset, "fast");
        let mut controller = PresentationController::new();
        controller
            .set_slash_query("/config model add")
            .expect("set model create route");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("activate model create route");

        controller
            .stage_config(&runtime, "edge")
            .expect("stage provider");
        controller
            .focus_config_field(&runtime, "/config model model", 0)
            .expect("focus model");
        controller
            .stage_config(&runtime, "fixture-model")
            .expect("stage model");
        controller
            .focus_config_field(&runtime, "/config model dialect", 0)
            .expect("focus dialect");
        controller
            .stage_config(&runtime, "responses")
            .expect("stage dialect");

        assert!(matches!(
            controller.screen(Some(&runtime)).expect("dialect screen"),
            PresentationScreenView::ConfigEditor {
                editor: ConfigEditorView {
                    field: ConfigFieldView {
                        ref path,
                        contents: ConfigFieldContents::Value {
                            target: Some(ConfigValue::String(ref value)),
                            ..
                        },
                        ..
                    },
                    ..
                },
                ..
            } if path == "model_presets.fast.dialect" && value == "responses"
        ));
        assert_eq!(
            controller
                .preview_config(&mut runtime)
                .expect("preview model preset")
                .changes
                .len(),
            3
        );
        controller
            .commit_config(&mut runtime)
            .expect("commit model preset");
        assert_eq!(runtime.model_presets().unwrap()[0].id, "fast");
    }

    #[test]
    fn controller_navigates_config_and_keeps_a_failed_draft_live() {
        let temp = TempTree::new("controller-config");
        let mut runtime = temp.open_provider_runtime();
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut controller = PresentationController::new();

        controller
            .set_slash_query("/config")
            .expect("set Config Center query");
        controller
            .activate(&mut runtime, ConfigScope::Project, None)
            .expect("open browse-only Config Center");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("browse-only Config Center screen"),
            PresentationScreenView::ConfigCenter {
                section: None,
                ref objects,
                selected: None,
            } if objects == std::slice::from_ref(&object)
        ));
        assert!(matches!(
            controller.activate_config_object(&mut runtime, ConfigScope::Project),
            Err(PresentationControllerError::ConfigObjectRouteUnavailable)
        ));
        controller.back().expect("leave browse-only Config Center");

        controller
            .set_slash_query("/config pro url")
            .expect("set focused editor query");
        controller
            .activate(&mut runtime, ConfigScope::Project, None)
            .expect("open provider selector");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("provider selector screen"),
            PresentationScreenView::ConfigCenter {
                section: Some(ConfigSection::Provider),
                ref objects,
                selected: Some(0),
            }
                if objects == std::slice::from_ref(&object)
        ));

        controller
            .activate_config_object(&mut runtime, ConfigScope::Project)
            .expect("open provider URL editor");
        controller
            .stage_config(&runtime, "http://provider.invalid/v1")
            .expect("stage typed but invalid URL");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("staged editor screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                validated: false,
                editor: ConfigEditorView {
                    field: ConfigFieldView {
                        contents: ConfigFieldContents::Value {
                            target: Some(ConfigValue::String(ref target)),
                            ..
                        },
                        ..
                    },
                    ..
                },
                ..
            } if target == "http://provider.invalid/v1"
        ));
        assert!(matches!(
            controller.preview_config(&mut runtime),
            Err(PresentationControllerError::ConfigEditor(
                ConfigEditorError::Config(ConfigRuntimeError::InvalidValue { ref path, .. })
            )) if path == "providers.edge.base_url"
        ));
        assert!(matches!(
            controller.back(),
            Err(PresentationControllerError::UnsavedConfigDraft)
        ));
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("editor remains visible"),
            PresentationScreenView::ProviderWizard { dirty: true, .. }
        ));

        controller
            .stage_config(&runtime, "https://controller.example.com/v1")
            .expect("correct staged URL");
        let preview = controller
            .preview_config(&mut runtime)
            .expect("preview corrected URL");
        assert_eq!(preview.changes.len(), 1);
        controller
            .reset_config(&runtime)
            .expect("reset staged URL through controller");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("reset editor screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                validated: false,
                editor: ConfigEditorView {
                    ref changes,
                    field: ConfigFieldView {
                        contents: ConfigFieldContents::Value { target: None, .. },
                        ..
                    },
                    ..
                },
                ..
            } if changes.is_empty()
        ));
        controller
            .stage_config(&runtime, "https://controller.example.com/v1")
            .expect("restage corrected URL");
        controller
            .preview_config(&mut runtime)
            .expect("preview restaged URL");
        let commit = controller
            .commit_config(&mut runtime)
            .expect("commit corrected URL");
        assert!(commit.written);
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("post-commit screen"),
            PresentationScreenView::SlashPanel(_)
        ));
    }

    #[test]
    fn controller_edits_credential_references_without_readback() {
        let temp = TempTree::new("controller-credential");
        let mut runtime = temp.open_provider_runtime();
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut controller = PresentationController::new();
        controller
            .set_slash_query("/config provider credential")
            .expect("set credential route");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("open credential status");

        let encoded = serde_json::to_string(
            &controller
                .screen(Some(&runtime))
                .expect("credential status screen"),
        )
        .expect("serialize credential status screen");
        assert!(!encoded.contains("synthetic-edge-credential-reference"));
        assert!(encoded.contains("credential_binding"));
        assert!(matches!(
            controller.stage_config(&runtime, "synthetic-replacement-reference"),
            Err(PresentationControllerError::ConfigEditor(
                ConfigEditorError::CredentialOperationRequired
            ))
        ));
        controller
            .stage_provider_credential_reference(&runtime, "synthetic-replacement-reference")
            .expect("stage opaque credential reference");
        controller
            .preview_config(&mut runtime)
            .expect("preview credential reference");
        let staged = serde_json::to_string(
            &controller
                .screen(Some(&runtime))
                .expect("staged credential status screen"),
        )
        .expect("serialize staged credential status screen");
        assert!(!staged.contains("synthetic-edge-credential-reference"));
        assert!(!staged.contains("synthetic-replacement-reference"));
        assert!(staged.contains("target_bound"));
        controller
            .commit_config(&mut runtime)
            .expect("commit credential reference");
        assert_eq!(
            runtime
                .provider_profile("edge")
                .expect("resolve Provider")
                .expect("Provider exists")
                .credential_reference(),
            Some("synthetic-replacement-reference")
        );
    }

    #[test]
    fn terminal_neutral_layout_is_deterministic_and_fits_three_viewports() {
        let temp = TempTree::new("layout-runtime");
        let config_runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
            .expect("open layout Config Runtime");
        let runtime = runtime(RecoveryStatus::Ready);
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
                provider_profile: Some("fixture-provider"),
                model: Some("deterministic-v1"),
                context_pressure: None,
                model_presets: &[],
                catalog_models: &[],
            },
        )
        .expect("view model");
        let controller = PresentationController::new();

        let mut layouts = Vec::new();
        for (width, height) in [(40, 12), (80, 24), (160, 50)] {
            let viewport = Viewport::new(width, height).expect("valid viewport");
            let layout = controller
                .layout(Some(&config_runtime), &view, viewport)
                .expect("terminal-neutral layout");
            assert_eq!(layout.viewport, viewport);
            assert!(
                layout
                    .body
                    .iter()
                    .chain(layout.statusline.rows.iter())
                    .all(|row| display_width(&row.text) <= usize::from(width))
            );
            assert!(layout.body.len() + layout.statusline.rows.len() <= usize::from(height));
            assert_eq!(
                serde_json::to_string(&layout).expect("serialize layout"),
                serde_json::to_string(
                    &controller
                        .layout(Some(&config_runtime), &view, viewport)
                        .expect("repeat layout")
                )
                .expect("serialize repeated layout")
            );
            layouts.push(layout);
        }

        assert_eq!(layouts[0].body[0].text, "Commands");
        assert_eq!(layouts[0].body[1].text, "> /config");
        assert_eq!(layouts[0].statusline.rows.len(), 1);
        assert_eq!(
            layouts[0].statusline.rows[0].text,
            "ready | blockers ? | model deterministi…"
        );
        assert_eq!(
            layouts[0].statusline.hidden,
            vec![
                super::StatusSegmentKind::Context,
                super::StatusSegmentKind::Usage,
                super::StatusSegmentKind::Cost,
                super::StatusSegmentKind::Agents,
                super::StatusSegmentKind::Provider,
                super::StatusSegmentKind::Config,
            ]
        );
        assert_eq!(
            layouts[1].statusline.rows[0].text,
            "ready | blockers ? | model deterministic-v1 | ctx ? | 1h ? | cost ? | agents ?"
        );
        assert_eq!(
            layouts[1].statusline.hidden,
            vec![
                super::StatusSegmentKind::Provider,
                super::StatusSegmentKind::Config,
            ]
        );
        assert_eq!(layouts[2].statusline.rows.len(), 2);
        assert_eq!(
            layouts[2].statusline.rows[0].text,
            "ready | blockers ? | model deterministic-v1 | ctx ? | 1h ? | cost ? | agents ? | provider fixture-provider | config ok"
        );
        assert_eq!(
            layouts[2].statusline.rows[1].text,
            "thread 7 | items 0 | tail 0B"
        );
        assert!(layouts[2].statusline.hidden.is_empty());

        let one_row = controller
            .layout(
                Some(&config_runtime),
                &view,
                Viewport::new(160, 1).expect("one-row viewport"),
            )
            .expect("one-row layout");
        assert!(one_row.body.is_empty());
        assert_eq!(one_row.statusline.rows.len(), 1);

        let one_cell = controller
            .layout(
                Some(&config_runtime),
                &view,
                Viewport::new(1, 1).expect("one-cell viewport"),
            )
            .expect("one-cell layout");
        assert_eq!(one_cell.statusline.rows[0].text, "…");
        assert_eq!(display_width(&one_cell.statusline.rows[0].text), 1);
    }

    #[test]
    fn config_center_keeps_the_selected_object_visible_in_a_short_viewport() {
        let temp = TempTree::new("config-center-short-viewport");
        fs::write(
            temp.paths().project(),
            r#"schema_version = 1

[providers.alpha]
template = "openai"

[providers.bravo]
template = "openai"

[providers.charlie]
template = "openai"
"#,
        )
        .expect("write provider choices");
        let mut runtime = ConfigRuntime::open(temp.paths(), ConfigDocument::empty())
            .expect("open Config Runtime");
        let mut controller = PresentationController::new();
        controller
            .set_slash_query("/config provider remove")
            .expect("set delete query");
        controller
            .activate(&mut runtime, ConfigScope::Project, None)
            .expect("open provider selector");
        controller
            .move_config_object_selection(&runtime, 2)
            .expect("select last provider");

        let smoke = super::build_smoke_view("/").expect("smoke view");
        let layout = controller
            .layout(
                Some(&runtime),
                smoke.view(),
                Viewport::new(80, 3).expect("short viewport"),
            )
            .expect("short Config Center layout");

        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.is_selected() && row.text() == "> provider charlie"),
            "selected object must remain visible before destructive activation"
        );
    }

    #[test]
    fn text_fit_preserves_unicode_clusters_and_never_exceeds_width() {
        assert_eq!(display_width("A双B"), 4);
        assert_eq!(fit_text("A双B", 3), "A…");
        assert_eq!(fit_text("Cafe\u{301} noir", 6), "Cafe\u{301}…");
        assert_eq!(fit_text("go 👩‍💻 now", 7), "go 👩‍💻…");
        assert_eq!(fit_text("safe\u{1b}[31m\n", 32), "safe?[31m?");
        for width in 0..12 {
            let fitted = fit_text("双语 👩‍💻 Cafe\u{301}", width);
            assert!(display_width(&fitted) <= width);
            assert!(std::str::from_utf8(fitted.as_bytes()).is_ok());
            assert!(!fitted.chars().any(char::is_control));
        }
    }
}
