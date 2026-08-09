//! Terminal-neutral product presentation model.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use greentyper_core::agent_team::{TaskStatus, TeamOperationStatus};
use greentyper_core::config::{
    CommandMatchKind, CommandQueryError, CommandTarget, ConfigCommit, ConfigEditorError,
    ConfigEditorSession, ConfigEditorView, ConfigErrorCategory, ConfigFieldContents,
    ConfigObjectKind, ConfigObjectRef, ConfigRuntime, ConfigRuntimeError, ConfigRuntimeStatus,
    ConfigScope, ConfigSection, ConfigValue, ModelPresetView, match_command_paths,
};
use greentyper_core::ledger::LedgerHead;
use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};
use greentyper_core::tool_runtime::{ToolCallStatus, ToolSnapshot};
use greentyper_core::usage::{RuntimeUsageSnapshot, UsageQuantity, UsageRollup};
use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    ConfigCenter { section: Option<ConfigSection> },
    ConfigEditor,
    ModelSelector,
    Stats,
    AgentCenter,
}

struct ActiveConfigEditor {
    object: Option<ConfigObjectRef>,
    session: ConfigEditorSession,
    view: ConfigEditorView,
    dirty: bool,
    validated: bool,
}

pub(crate) struct PresentationController {
    state: PresentationState,
    slash_query: String,
    model_query: String,
    selected: usize,
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
            editor: None,
        }
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
        ModelSelectorView::build(&[], query)?;
        self.model_query = query.to_owned();
        Ok(())
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
        let target = panel
            .entries
            .get(self.selected)
            .ok_or(PresentationControllerError::NoCommandSelection)?
            .target;
        match target {
            CommandTarget::ConfigCenter => {
                self.state = PresentationState::ConfigCenter { section: None };
            }
            CommandTarget::ConfigSection { section } => {
                self.state = PresentationState::ConfigCenter {
                    section: Some(section),
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
                });
                self.state = PresentationState::ConfigEditor;
            }
            CommandTarget::ModelSelector => {
                self.state = PresentationState::ModelSelector;
            }
            CommandTarget::Stats => {
                self.state = PresentationState::Stats;
            }
            CommandTarget::AgentCenter => {
                self.state = PresentationState::AgentCenter;
            }
        }
        Ok(())
    }

    pub(crate) fn back(&mut self) -> Result<(), PresentationControllerError> {
        self.require_discardable_editor()?;
        self.editor = None;
        self.state = PresentationState::SlashPanel;
        Ok(())
    }

    pub(crate) fn discard_config(&mut self) -> Result<(), PresentationControllerError> {
        if self.state != PresentationState::ConfigEditor {
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

    pub(crate) fn commit_config(
        &mut self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigCommit, PresentationControllerError> {
        let commit = self.active_editor()?.session.try_commit(runtime)?;
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
            PresentationState::ConfigCenter { section } => {
                let mut objects = runtime
                    .ok_or(PresentationControllerError::ConfigRuntimeRequired)?
                    .addressable_objects()?;
                if let Some(section) = section {
                    objects.retain(|object| object_section(object.kind()) == section);
                }
                Ok(PresentationScreenView::ConfigCenter { section, objects })
            }
            PresentationState::ConfigEditor => {
                let editor = self.active_editor()?;
                Ok(PresentationScreenView::ConfigEditor {
                    object: editor.object.clone(),
                    editor: editor.view.clone(),
                    dirty: editor.dirty,
                    validated: editor.validated,
                })
            }
            PresentationState::ModelSelector => Ok(PresentationScreenView::ModelSelector {
                query: self.model_query.clone(),
            }),
            PresentationState::Stats => Ok(PresentationScreenView::Stats),
            PresentationState::AgentCenter => Ok(PresentationScreenView::AgentCenter),
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
        body.truncate(body_capacity);
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
        if self.state != PresentationState::ConfigEditor {
            return Err(PresentationControllerError::NotConfigEditor);
        }
        self.editor
            .as_ref()
            .ok_or(PresentationControllerError::NotConfigEditor)
    }

    fn active_editor_mut(
        &mut self,
    ) -> Result<&mut ActiveConfigEditor, PresentationControllerError> {
        if self.state != PresentationState::ConfigEditor {
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
    ConfigCenter {
        section: Option<ConfigSection>,
        objects: Vec<ConfigObjectRef>,
    },
    ConfigEditor {
        object: Option<ConfigObjectRef>,
        editor: ConfigEditorView,
        dirty: bool,
        validated: bool,
    },
    ModelSelector {
        query: String,
    },
    Stats,
    AgentCenter,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusSegmentKind {
    Recovery,
    Blockers,
    Model,
    Context,
    Usage,
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
            text: match status.context_pressure_percent {
                Availability::Known(percent) => format!("ctx {percent}%"),
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
        PresentationScreenView::ConfigCenter { section, objects } => {
            let title = section.map_or_else(
                || "Config".to_owned(),
                |section| format!("Config / {}", config_section_label(section)),
            );
            let mut rows = vec![LayoutRowView::new(title, false)];
            rows.extend(objects.iter().map(|object| {
                LayoutRowView::new(
                    format!("{} {}", config_object_label(object.kind()), object.id()),
                    false,
                )
            }));
            rows
        }
        PresentationScreenView::ConfigEditor {
            editor,
            dirty,
            validated,
            ..
        } => {
            let mut rows = vec![LayoutRowView::new(
                format!("Config / {}", editor.field.path),
                false,
            )];
            match &editor.field.contents {
                ConfigFieldContents::Value {
                    effective, target, ..
                } => {
                    rows.push(LayoutRowView::new(
                        format!("effective {}", config_value_label(effective.as_ref())),
                        false,
                    ));
                    rows.push(LayoutRowView::new(
                        format!("target {}", config_value_label(target.as_ref())),
                        false,
                    ));
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
                    rows.push(LayoutRowView::new(
                        format!("target {}", if *target_bound { "bound" } else { "missing" }),
                        false,
                    ));
                }
            }
            rows.push(LayoutRowView::new(
                match (*dirty, *validated) {
                    (false, _) => "draft clean",
                    (true, true) => "draft validated",
                    (true, false) => "draft pending",
                },
                false,
            ));
            rows
        }
        PresentationScreenView::ModelSelector { .. } => {
            let mut rows = vec![LayoutRowView::new("Models", false)];
            rows.extend(view.models.all.iter().enumerate().map(|(index, entry)| {
                LayoutRowView::new(
                    format!(
                        "{} {} / {} / {}",
                        if index == 0 { '>' } else { ' ' },
                        entry.preset.id,
                        entry.preset.provider,
                        entry.preset.model
                    ),
                    index == 0,
                )
            }));
            rows
        }
        PresentationScreenView::Stats => vec![LayoutRowView::new("Stats", false)],
        PresentationScreenView::AgentCenter => vec![LayoutRowView::new("Agents", false)],
    }
}

fn config_value_label(value: Option<&ConfigValue>) -> String {
    match value {
        Some(ConfigValue::String(value)) => value.clone(),
        Some(ConfigValue::PositiveInteger(value)) => value.to_string(),
        Some(ConfigValue::Boolean(value)) => value.to_string(),
        Some(ConfigValue::StringList(value)) => value.join(", "),
        None => "inherited".to_owned(),
    }
}

fn config_section_label(section: ConfigSection) -> &'static str {
    match section {
        ConfigSection::Provider => "provider",
        ConfigSection::Model => "model",
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
        ConfigObjectKind::UsageWindow => "stats-window",
    }
}

fn object_section(kind: ConfigObjectKind) -> ConfigSection {
    match kind {
        ConfigObjectKind::ProviderProfile => ConfigSection::Provider,
        ConfigObjectKind::ModelPreset => ConfigSection::Model,
        ConfigObjectKind::UsageWindow => ConfigSection::StatsWindow,
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
    NotConfigEditor,
    ConfigRuntimeRequired,
    UnsavedConfigDraft,
}

impl fmt::Display for PresentationControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(source) => write!(formatter, "{source}"),
            Self::ConfigEditor(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::NoCommandSelection => formatter.write_str("no command is selected"),
            Self::NotSlashPanel => formatter.write_str("command requires the Slash Panel"),
            Self::NotConfigEditor => formatter.write_str("command requires a Config editor"),
            Self::ConfigRuntimeRequired => {
                formatter.write_str("Config Center requires the Config Runtime")
            }
            Self::UnsavedConfigDraft => {
                formatter.write_str("Config draft must be committed or discarded")
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
            | Self::NotConfigEditor
            | Self::ConfigRuntimeRequired
            | Self::UnsavedConfigDraft => None,
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
            context_pressure_percent: None,
            model_presets: &[],
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
        ConfigEditorView, ConfigErrorCategory, ConfigFieldContents, ConfigFieldView,
        ConfigObjectKind, ConfigObjectRef, ConfigPaths, ConfigRepairIssue, ConfigRuntime,
        ConfigRuntimeError, ConfigRuntimeStatus, ConfigScope, ConfigValue, ModelPresetView,
    };
    use greentyper_core::ledger::LedgerHead;
    use greentyper_core::model::{DeliveryId, ThreadId, TurnId};
    use greentyper_core::provider::ProviderDialect;
    use greentyper_core::runtime::{KernelTeamSnapshot, RecoveryStatus, RuntimeSnapshot};

    use super::{
        Availability, BlockerView, PresentationController, PresentationControllerError,
        PresentationScreenView, PresentationSources, RecoveryBadge, SlashPanelView, TuiViewModel,
        Viewport, display_width, fit_text,
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
            PresentationScreenView::ConfigEditor { dirty: true, .. }
        ));
        loser
            .stage_config(&loser_runtime, "https://still-live.example.com/v1")
            .expect("stale editor session remains live after conflict");
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
            .expect("open Config Center");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("Config Center screen"),
            PresentationScreenView::ConfigCenter { section: None, ref objects }
                if objects == std::slice::from_ref(&object)
        ));

        controller.back().expect("return to Slash Panel");
        controller
            .set_slash_query("/config pro url")
            .expect("set focused editor query");
        controller
            .activate(&mut runtime, ConfigScope::Project, Some(&object))
            .expect("open provider URL editor");
        controller
            .stage_config(&runtime, "http://provider.invalid/v1")
            .expect("stage typed but invalid URL");
        assert!(matches!(
            controller
                .screen(Some(&runtime))
                .expect("staged editor screen"),
            PresentationScreenView::ConfigEditor {
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
            PresentationScreenView::ConfigEditor { dirty: true, .. }
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
            PresentationScreenView::ConfigEditor {
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
    fn controller_keeps_credential_fields_status_only() {
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
        assert!(matches!(
            controller.commit_config(&mut runtime),
            Err(PresentationControllerError::ConfigEditor(
                ConfigEditorError::CredentialOperationRequired
            ))
        ));
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
                context_pressure_percent: None,
                model_presets: &[],
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
                super::StatusSegmentKind::Agents,
                super::StatusSegmentKind::Provider,
                super::StatusSegmentKind::Config,
            ]
        );
        assert_eq!(
            layouts[1].statusline.rows[0].text,
            "ready | blockers ? | model deterministic-v1 | ctx ? | 1h ? | agents ? | provide…"
        );
        assert_eq!(
            layouts[1].statusline.hidden,
            vec![super::StatusSegmentKind::Config]
        );
        assert_eq!(layouts[2].statusline.rows.len(), 2);
        assert_eq!(
            layouts[2].statusline.rows[0].text,
            "ready | blockers ? | model deterministic-v1 | ctx ? | 1h ? | agents ? | provider fixture-provider | config ok"
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
