//! Product terminal adapter.

use std::error::Error;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal as crossterm_terminal;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use greentyper_core::agent_team::TeamOperationRecord;
use greentyper_core::config::{
    ConfigEditorError, ConfigError, ConfigFieldContents, ConfigFieldInteraction, ConfigRuntime,
    ConfigRuntimeError, ConfigScope, ConfigValue, ConfigValueKind, MAX_COMMAND_QUERY_BYTES,
    MAX_CONFIG_ID_BYTES, ModelPresetView,
};
use greentyper_core::model::DeliveryId;
use greentyper_core::provider::ProviderError;
use greentyper_core::provider_discovery::{ProviderDiscoveryError, ProviderDiscoveryState};
use greentyper_core::runtime::{
    KernelTeamSnapshot, ProviderToolApproval, RuntimeError, RuntimeKernel, RuntimeSnapshot,
};
use greentyper_core::tool_runtime::{ToolCallStatus, ToolSnapshot};
use greentyper_core::usage::{RuntimeUsageSnapshot, UsageError, UsageTimestamp};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, MAX_SECRET_BYTES, PlatformCredentialVault,
    ProviderCredentialScope, SecretValue,
};
use crate::local_process::{LocalProcessError, LocalProcessExecutor};
use crate::presentation::{
    AgentLifecycleFlowView, BlockerEntryAction, CredentialFlowView, DiscoveredModelAcceptance,
    ModelEntryAction, PresentationController, PresentationControllerError, PresentationError,
    PresentationLayoutView, PresentationSources, ProductToolApprovalView, ToolApprovalAction,
    ToolApprovalEntryAction, TuiViewModel, Viewport, ViewportError,
};
use crate::product_driver::{
    ProductDriver, ProductDriverError, ProductInteraction, ProductToolDecision,
    ProductToolDecisionOutcome, acknowledge_product_team_operation,
    apply_model_preset_to_next_turn, cancel_product_agent, freeze_model_selection,
    inspect_product_team, inspect_product_tools, stage_product_model_selection,
};
use crate::provider_connection::{ModelsHttpConnectionTester, ProviderConnectionTester};
use crate::provider_discovery_catalog::{
    commit_provider_discovery_status, provider_discovery_catalogs, refresh_provider_discovery,
};
use crate::provider_discovery_task::{
    OnDemandProviderDiscoveryTask, ProviderDiscoveryTask, ProviderDiscoveryTaskEvent,
    ProviderDiscoveryTrigger,
};
use crate::provider_http::ConfiguredProvider;

enum TerminalToolResolution {
    Prepared { delivery: u64, text: String },
    Denied,
}

trait TerminalProductActions {
    fn load_tool_approval(&mut self, call: u64) -> Result<ProductToolApprovalView, TerminalError>;

    fn resolve_tool_approval(
        &mut self,
        call: u64,
        decision: ProductToolDecision,
    ) -> Result<TerminalToolResolution, TerminalError>;

    fn cancel_tool_approval(&mut self);

    fn acknowledge_output(&mut self, delivery: u64) -> Result<(), TerminalError>;

    fn cancel_agent(&mut self, agent: u64) -> Result<u64, TerminalError>;

    fn acknowledge_team_operation(&mut self, operation: u64) -> Result<(), TerminalError>;
}

struct LedgerTerminalProductActions<'a> {
    ledger: &'a Path,
    pending: Option<LedgerPendingToolApproval>,
}

struct LedgerPendingToolApproval {
    call: u64,
    driver: ProductDriver<LocalProcessExecutor>,
    provider: ConfiguredProvider<PlatformCredentialVault>,
    approval: Box<ProviderToolApproval>,
}

struct TerminalApprovalInteraction;

impl ProductInteraction for TerminalApprovalInteraction {
    fn present_team_operation(&mut self, _record: TeamOperationRecord) -> io::Result<()> {
        Err(io::Error::other(
            "pending Team operation must be acknowledged separately",
        ))
    }

    fn decide_tool(&mut self, _approval: &ProviderToolApproval) -> io::Result<ProductToolDecision> {
        Err(io::Error::other(
            "Tool approval requires the rendered terminal decision",
        ))
    }
}

impl TerminalProductActions for LedgerTerminalProductActions<'_> {
    fn load_tool_approval(&mut self, call: u64) -> Result<ProductToolApprovalView, TerminalError> {
        self.pending = None;
        let tools = inspect_product_tools(self.ledger)?;
        let agent = tools
            .calls
            .iter()
            .find(|record| {
                record.call.get() == call && record.status == ToolCallStatus::AwaitingApproval
            })
            .map(|record| record.agent.get())
            .ok_or(TerminalError::ToolApprovalUnavailable)?;
        let mut interaction = TerminalApprovalInteraction;
        let executor = LocalProcessExecutor::current()?;
        let mut driver =
            ProductDriver::open_with_executor(self.ledger, executor, &mut interaction)?;
        let mut provider = ConfiguredProvider::from_epoch(
            driver
                .pending_provider_epoch()
                .ok_or(TerminalError::PendingProviderEpochRequired)?,
            PlatformCredentialVault,
        )?;
        provider.enable_local_echo();
        let approval = driver.recover_pending_tool_approval(call, &mut provider)?;
        let view = ProductToolApprovalView {
            call,
            agent,
            tool: approval.tool().to_owned(),
            identity: approval.identity().to_owned(),
            arguments: approval.arguments().canonical_json().to_owned(),
            filesystem_reads: approval
                .resources()
                .filesystem_reads()
                .map(str::to_owned)
                .collect(),
            filesystem_writes: approval
                .resources()
                .filesystem_writes()
                .map(str::to_owned)
                .collect(),
            process: approval.resources().process().map(str::to_owned),
            network_targets: approval
                .resources()
                .network_targets()
                .map(str::to_owned)
                .collect(),
        };
        self.pending = Some(LedgerPendingToolApproval {
            call,
            driver,
            provider,
            approval,
        });
        Ok(view)
    }

    fn resolve_tool_approval(
        &mut self,
        call: u64,
        decision: ProductToolDecision,
    ) -> Result<TerminalToolResolution, TerminalError> {
        let pending = self
            .pending
            .take()
            .filter(|pending| pending.call == call)
            .ok_or(TerminalError::ToolApprovalUnavailable)?;
        let LedgerPendingToolApproval {
            mut driver,
            mut provider,
            approval,
            ..
        } = pending;
        match driver.resolve_recovered_tool_approval(approval, decision, &mut provider)? {
            ProductToolDecisionOutcome::Prepared(output) => Ok(TerminalToolResolution::Prepared {
                delivery: output.delivery().get(),
                text: output.text().to_owned(),
            }),
            ProductToolDecisionOutcome::Denied => Ok(TerminalToolResolution::Denied),
        }
    }

    fn cancel_tool_approval(&mut self) {
        self.pending = None;
    }

    fn acknowledge_output(&mut self, delivery: u64) -> Result<(), TerminalError> {
        self.pending = None;
        let mut interaction = TerminalApprovalInteraction;
        let executor = LocalProcessExecutor::current()?;
        let mut driver =
            ProductDriver::open_with_executor(self.ledger, executor, &mut interaction)?;
        let delivery = DeliveryId::new(delivery).map_err(RuntimeError::Model)?;
        driver.acknowledge(delivery)?;
        Ok(())
    }

    fn cancel_agent(&mut self, agent: u64) -> Result<u64, TerminalError> {
        let operation = cancel_product_agent(
            self.ledger,
            Some(agent),
            "cancelled from Agent Center".to_owned(),
        )?;
        Ok(operation.operation.get())
    }

    fn acknowledge_team_operation(&mut self, operation: u64) -> Result<(), TerminalError> {
        acknowledge_product_team_operation(self.ledger, operation)?;
        Ok(())
    }
}

pub(crate) fn require_interactive() -> Result<(), TerminalError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TerminalError::NonInteractive);
    }
    Ok(())
}

pub(crate) fn run(ledger: &Path, config: &mut ConfigRuntime) -> Result<(), TerminalError> {
    let view = build_terminal_view(ledger, config, "/")?;
    let (width, height) = crossterm_terminal::size()?;
    let viewport = Viewport::new(width, height)?;
    let stdout = io::stdout();
    let _writer = run_terminal_loop_with_discovery_task(
        stdout.lock(),
        CrosstermTerminalMode,
        config,
        &view,
        ledger,
        viewport,
        event::read,
    )?;
    Ok(())
}

fn build_terminal_view(
    ledger: &Path,
    config: &ConfigRuntime,
    query: &str,
) -> Result<TuiViewModel, TerminalError> {
    let runtime = RuntimeKernel::inspect(ledger)?;
    let usage = RuntimeKernel::inspect_usage(ledger, UsageTimestamp::now()?)?;
    let team = inspect_product_team(ledger)?;
    let tools = inspect_product_tools(ledger)?;
    let discovery = inspect_terminal_discovery(ledger);
    build_terminal_view_from_sources(
        config,
        query,
        &runtime,
        &usage,
        team.as_ref(),
        &tools,
        discovery.as_ref(),
    )
}

fn refresh_terminal_view(
    ledger: &Path,
    config: &ConfigRuntime,
    query: &str,
) -> Result<RefreshedTerminalSnapshot, TerminalError> {
    let runtime = RuntimeKernel::inspect(ledger)?;
    let usage = RuntimeKernel::inspect_usage(ledger, UsageTimestamp::now()?)?;
    let team = inspect_product_team(ledger)?;
    let tools = inspect_product_tools(ledger)?;
    let config = config.reload_candidate()?;
    let discovery = inspect_terminal_discovery(ledger);
    let view = build_terminal_view_from_sources(
        &config,
        query,
        &runtime,
        &usage,
        team.as_ref(),
        &tools,
        discovery.as_ref(),
    )?;
    Ok(RefreshedTerminalSnapshot { config, view })
}

struct RefreshedTerminalSnapshot {
    config: ConfigRuntime,
    view: TuiViewModel,
}

fn stage_terminal_model_selection(
    ledger: &Path,
    config: &ConfigRuntime,
    displayed: &ModelPresetView,
) -> Result<(), TerminalError> {
    let preset = config.model_preset(&displayed.id)?;
    if preset != *displayed {
        return Err(RuntimeError::InvalidModelSelection("displayed Preset is stale").into());
    }
    let mut layers = config.config_layers()?.clone();
    apply_model_preset_to_next_turn(&mut layers, &preset);
    let usage_windows = config.resolved_usage_windows()?;
    let price_schedules = config.resolved_price_schedules()?;
    let selection = freeze_model_selection(&layers, &usage_windows, &price_schedules, &preset)?;
    stage_product_model_selection(ledger, selection)?;
    Ok(())
}

fn build_terminal_view_from_sources(
    config: &ConfigRuntime,
    query: &str,
    runtime: &RuntimeSnapshot,
    usage: &RuntimeUsageSnapshot,
    team: Option<&KernelTeamSnapshot>,
    tools: &ToolSnapshot,
    discovery: Option<&ProviderDiscoveryState>,
) -> Result<TuiViewModel, TerminalError> {
    let status = config.status();
    let resolved = config.config_layers()?.resolve()?;
    let model_presets = config.model_presets()?;
    let catalog_models = config.catalog_models()?;
    let discovery_catalogs = discovery
        .map(|state| provider_discovery_catalogs(config, state))
        .transpose()?;
    TuiViewModel::build(
        query,
        "",
        0,
        PresentationSources {
            runtime,
            usage: Some(usage),
            team,
            tools: Some(tools),
            config: &status,
            provider_profile: Some(resolved.provider_profile().value()),
            model: Some(resolved.provider_model().value()),
            context_pressure: None,
            model_presets: &model_presets,
            catalog_models: &catalog_models,
            discovery_catalogs: discovery_catalogs.as_deref(),
        },
    )
    .map_err(TerminalError::PresentationModel)
}

fn terminal_discovery_path(ledger: &Path) -> PathBuf {
    ledger.with_file_name("provider-discovery.json")
}

fn inspect_terminal_discovery(ledger: &Path) -> Option<ProviderDiscoveryState> {
    ProviderDiscoveryState::inspect(&terminal_discovery_path(ledger)).ok()
}

fn can_refresh_terminal_discovery_on_open(config: &ConfigRuntime) -> bool {
    eligible_terminal_discovery_profile(config).is_some()
}

fn eligible_terminal_discovery_profile(
    config: &ConfigRuntime,
) -> Option<greentyper_core::provider::ProviderProfileSnapshot> {
    let profile = config.selected_provider_profile().ok().flatten()?;
    (matches!(
        config.provider_catalog_mode(profile.profile()),
        Ok(mode) if mode.includes_discovery()
    ) && profile.models_endpoint().is_some())
    .then_some(profile)
}

fn refresh_terminal_discovery<T: ProviderConnectionTester + ?Sized>(
    ledger: &Path,
    config: &ConfigRuntime,
    tester: &mut T,
) -> Result<crate::provider_connection::ProviderConnectionTestStatus, TerminalError> {
    let profile =
        config
            .selected_provider_profile()?
            .ok_or(ProviderError::InvalidConfiguration(
                "selected simulator profile has no Provider discovery endpoint",
            ))?;
    if !config
        .provider_catalog_mode(profile.profile())?
        .includes_discovery()
    {
        return Err(ProviderError::InvalidConfiguration(
            "selected Provider Profile catalog mode does not allow discovery",
        )
        .into());
    }
    refresh_provider_discovery(
        &profile,
        &terminal_discovery_path(ledger),
        UsageTimestamp::now()?.unix_millis(),
        tester,
    )
    .map_err(TerminalError::from)
}

fn validate_terminal_discovery_acceptance(
    ledger: &Path,
    config: &ConfigRuntime,
    acceptance: &DiscoveredModelAcceptance,
) -> Result<(), TerminalError> {
    let profile = config
        .provider_profile(&acceptance.profile)?
        .ok_or(ProviderDiscoveryError::StaleObservation)?;
    if profile.template() != acceptance.template
        || profile.fingerprint() != acceptance.profile_fingerprint
        || !config
            .provider_catalog_mode(&acceptance.profile)?
            .includes_discovery()
    {
        return Err(ProviderDiscoveryError::StaleObservation.into());
    }
    let state = ProviderDiscoveryState::inspect(&terminal_discovery_path(ledger))?;
    let observation = state
        .profiles()
        .iter()
        .find(|candidate| candidate.profile() == acceptance.profile)
        .ok_or(ProviderDiscoveryError::MissingObservation)?;
    if observation.template() != acceptance.template
        || observation.fingerprint() != acceptance.profile_fingerprint
        || observation.observed_at_unix_ms() != acceptance.observed_at_unix_ms
    {
        return Err(ProviderDiscoveryError::StaleObservation.into());
    }
    if !observation
        .models()
        .iter()
        .any(|model| model.id() == acceptance.model)
    {
        return Err(ProviderDiscoveryError::UnknownModel.into());
    }
    Ok(())
}

const ENTER_TERMINAL: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H";
const LEAVE_TERMINAL: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1049l";
const MAX_TERMINAL_COLUMNS: u16 = 512;
const MAX_TERMINAL_ROWS: u16 = 256;
const MAX_TERMINAL_CELLS: usize = 128 * 1024;

trait TerminalMode {
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

struct CrosstermTerminalMode;

impl TerminalMode for CrosstermTerminalMode {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        if !crossterm::ansi_support::supports_ansi() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "terminal does not support VT output",
            ));
        }
        crossterm_terminal::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm_terminal::disable_raw_mode()
    }
}

struct TerminalSurface<W: Write, M: TerminalMode> {
    writer: Option<W>,
    mode: M,
    active: bool,
}

impl<W: Write, M: TerminalMode> TerminalSurface<W, M> {
    fn enter(mut writer: W, mut mode: M) -> io::Result<Self> {
        mode.enable_raw_mode()?;
        if let Err(source) = writer
            .write_all(ENTER_TERMINAL)
            .and_then(|()| writer.flush())
        {
            let _ = writer.write_all(LEAVE_TERMINAL);
            let _ = writer.flush();
            let _ = mode.disable_raw_mode();
            return Err(source);
        }
        Ok(Self {
            writer: Some(writer),
            mode,
            active: true,
        })
    }

    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let writer = self.writer.as_mut().expect("terminal writer is present");
        writer.write_all(bytes)?;
        writer.flush()
    }

    fn finish(mut self) -> io::Result<W> {
        self.restore()?;
        Ok(self.writer.take().expect("terminal writer is present"))
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let output = self
            .writer
            .as_mut()
            .expect("terminal writer is present")
            .write_all(LEAVE_TERMINAL)
            .and_then(|()| {
                self.writer
                    .as_mut()
                    .expect("terminal writer is present")
                    .flush()
            });
        let mode = self.mode.disable_raw_mode();
        output.and(mode)
    }
}

impl<W: Write, M: TerminalMode> Drop for TerminalSurface<W, M> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalStyle {
    Plain,
    Header,
    Accent,
    Dim,
    Warning,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalCell {
    symbol: String,
    continuation: bool,
    style: TerminalStyle,
}

impl TerminalCell {
    fn blank() -> Self {
        Self {
            symbol: " ".to_owned(),
            continuation: false,
            style: TerminalStyle::Plain,
        }
    }

    fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.symbol.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalFrame {
    width: u16,
    height: u16,
    cells: Vec<TerminalCell>,
}

impl TerminalFrame {
    fn blank(width: u16, height: u16) -> Result<Self, TerminalError> {
        let cell_count = usize::from(width)
            .checked_mul(usize::from(height))
            .filter(|cell_count| *cell_count <= MAX_TERMINAL_CELLS)
            .ok_or(TerminalError::InvalidDimensions)?;
        if width == 0 || height == 0 || width > MAX_TERMINAL_COLUMNS || height > MAX_TERMINAL_ROWS {
            return Err(TerminalError::InvalidDimensions);
        }
        Ok(Self {
            width,
            height,
            cells: vec![TerminalCell::blank(); cell_count],
        })
    }

    #[cfg(test)]
    fn from_rows(width: u16, height: u16, rows: &[&str]) -> Result<Self, TerminalError> {
        let mut frame = Self::blank(width, height)?;
        for (row, text) in rows.iter().take(usize::from(height)).enumerate() {
            frame.set_row(row, text, TerminalStyle::Plain)?;
        }
        Ok(frame)
    }

    #[cfg(test)]
    fn from_layout(layout: &PresentationLayoutView) -> Result<Self, TerminalError> {
        Self::from_layout_with_notice(layout, None)
    }

    fn from_layout_with_notice(
        layout: &PresentationLayoutView,
        notice: Option<&str>,
    ) -> Result<Self, TerminalError> {
        let viewport = layout.viewport();
        let mut frame = Self::blank(viewport.width(), viewport.height())?;
        let status_start = usize::from(viewport.height()) - layout.statusline_rows().len();
        let notice_row = notice.map(|_| status_start.saturating_sub(1));
        let body_limit = notice_row.unwrap_or(status_start);
        for (row_index, row) in layout.body().iter().take(body_limit).enumerate() {
            let style = if row.is_selected() {
                TerminalStyle::Accent
            } else if row_index == 0 {
                TerminalStyle::Header
            } else {
                TerminalStyle::Plain
            };
            frame.set_row(row_index, row.text(), style)?;
        }
        for (offset, row) in layout.statusline_rows().iter().enumerate() {
            frame.set_row(status_start + offset, row.text(), TerminalStyle::Dim)?;
        }
        if let (Some(notice), Some(notice_row)) = (notice, notice_row) {
            let sanitized: String = notice
                .chars()
                .map(|character| {
                    if character.is_control() {
                        '?'
                    } else {
                        character
                    }
                })
                .collect();
            frame.set_row(notice_row, &sanitized, TerminalStyle::Warning)?;
        }
        Ok(frame)
    }

    fn set_row(
        &mut self,
        row: usize,
        text: &str,
        style: TerminalStyle,
    ) -> Result<(), TerminalError> {
        let width = usize::from(self.width);
        let mut column = 0;
        for grapheme in UnicodeSegmentation::graphemes(text, true) {
            let display_width = UnicodeWidthStr::width(grapheme);
            if display_width == 0 || display_width > 2 {
                return Err(TerminalError::UnsupportedCellWidth);
            }
            if column + display_width > width {
                break;
            }
            self.cells[row * width + column] = TerminalCell {
                symbol: grapheme.to_owned(),
                continuation: false,
                style,
            };
            for continued in 1..display_width {
                self.cells[row * width + column + continued] = TerminalCell {
                    symbol: String::new(),
                    continuation: true,
                    style,
                };
            }
            column += display_width;
        }
        Ok(())
    }

    fn cell(&self, column: u16, row: u16) -> &TerminalCell {
        &self.cells[usize::from(row) * usize::from(self.width) + usize::from(column)]
    }
}

struct DirectVtRenderer {
    previous: TerminalFrame,
    has_frame: bool,
}

impl DirectVtRenderer {
    fn new(width: u16, height: u16) -> Result<Self, TerminalError> {
        Ok(Self {
            previous: TerminalFrame::blank(width, height)?,
            has_frame: false,
        })
    }

    fn resize(&mut self, width: u16, height: u16) -> Result<Vec<u8>, TerminalError> {
        self.previous = TerminalFrame::blank(width, height)?;
        self.has_frame = false;
        Ok(b"\x1b[2J\x1b[H".to_vec())
    }

    fn draw(&mut self, frame: &TerminalFrame) -> Result<Vec<u8>, TerminalError> {
        if self.has_frame && self.previous == *frame {
            return Ok(Vec::new());
        }
        if self.previous.width != frame.width || self.previous.height != frame.height {
            return Err(TerminalError::DimensionMismatch);
        }

        let mut output = Vec::new();
        for row in 0..frame.height {
            let mut changed = vec![false; usize::from(frame.width)];
            for column in 0..frame.width {
                let previous = self.previous.cell(column, row);
                let current = frame.cell(column, row);
                if previous == current {
                    continue;
                }
                changed[usize::from(column)] = true;
                if (previous.continuation || current.continuation) && column > 0 {
                    changed[usize::from(column - 1)] = true;
                }
                if (previous.display_width() == 2 || current.display_width() == 2)
                    && column + 1 < frame.width
                {
                    changed[usize::from(column + 1)] = true;
                }
            }
            let mut column = 0;
            while column < frame.width {
                if !changed[usize::from(column)] {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < frame.width && changed[usize::from(column)] {
                    column += 1;
                }
                write!(output, "\x1b[{};{}H", row + 1, start + 1)?;
                let mut active_style = None;
                for changed in start..column {
                    let cell = frame.cell(changed, row);
                    if !cell.continuation {
                        if active_style != Some(cell.style) {
                            output.extend_from_slice(terminal_style(cell.style));
                            active_style = Some(cell.style);
                        }
                        write!(output, "{}", cell.symbol)?;
                    }
                }
                output.extend_from_slice(b"\x1b[0m");
            }
        }
        self.previous = frame.clone();
        self.has_frame = true;
        Ok(output)
    }
}

const fn terminal_style(style: TerminalStyle) -> &'static [u8] {
    match style {
        TerminalStyle::Plain => b"\x1b[0m",
        TerminalStyle::Header => b"\x1b[0;1;38;5;6m",
        TerminalStyle::Accent => b"\x1b[0;38;5;2m",
        TerminalStyle::Dim => b"\x1b[0;2;38;5;8m",
        TerminalStyle::Warning => b"\x1b[0;38;5;3m",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalInputEvent {
    Character(char),
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Enter,
    Escape,
    TestProviderConnection,
    CredentialActions,
    RefreshSnapshot,
    Resize(u16, u16),
    Quit,
    Ignore,
}

fn map_crossterm_event(event: Event) -> TerminalInputEvent {
    match event {
        Event::Resize(width, height) => TerminalInputEvent::Resize(width, height),
        Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char(character)
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(character, 'c' | 'q') =>
            {
                TerminalInputEvent::Quit
            }
            KeyCode::Char('r') if key.modifiers == KeyModifiers::CONTROL => {
                TerminalInputEvent::RefreshSnapshot
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                TerminalInputEvent::Character(character)
            }
            KeyCode::Backspace => TerminalInputEvent::Backspace,
            KeyCode::Delete => TerminalInputEvent::Delete,
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                TerminalInputEvent::BackTab
            }
            KeyCode::Tab => TerminalInputEvent::Tab,
            KeyCode::BackTab => TerminalInputEvent::BackTab,
            KeyCode::Up => TerminalInputEvent::Up,
            KeyCode::Down => TerminalInputEvent::Down,
            KeyCode::Enter => TerminalInputEvent::Enter,
            KeyCode::Esc => TerminalInputEvent::Escape,
            KeyCode::F(5) if key.modifiers.is_empty() => TerminalInputEvent::TestProviderConnection,
            KeyCode::F(6) if key.modifiers.is_empty() => TerminalInputEvent::RefreshSnapshot,
            KeyCode::F(7) if key.modifiers.is_empty() => TerminalInputEvent::CredentialActions,
            _ => TerminalInputEvent::Ignore,
        },
        _ => TerminalInputEvent::Ignore,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TerminalIntent {
    SetSlashQuery(String),
    MoveSelection(isize),
    Activate,
    EditModelQuery(char),
    BackspaceModelQuery,
    ClearModelQuery,
    MoveModelGroup(isize),
    MoveModelSelection(isize),
    ToggleModelDetail,
    RefreshProviderDiscovery,
    MoveDiscoveredModelDialect(isize),
    ConfirmDiscoveredModelDialect,
    MoveStatsGroup(isize),
    MoveStatsSelection(isize),
    ToggleStatsDetail,
    MoveAgentSelection(isize),
    ToggleAgentDetail,
    OpenAgentActions,
    MoveAgentAction(isize),
    ActivateAgentAction,
    ConfirmAgentAction,
    CancelAgentFlow,
    MoveBlockerSelection(isize),
    ActivateBlocker,
    MoveToolApprovalSelection(isize),
    ActivateToolApproval,
    CancelToolApproval,
    MoveProductOutputSelection(isize),
    AcknowledgeProductOutput,
    RefreshSnapshot,
    EditConfigObjectId(char),
    BackspaceConfigObjectId,
    ClearConfigObjectId,
    SubmitConfigObjectId,
    MoveConfigObjectSelection(isize),
    ActivateConfigObject,
    MoveConfigField(isize),
    MoveConfigChoice(isize),
    TestProviderConnection,
    OpenCredentialActions,
    MoveCredentialAction(isize),
    ActivateCredentialAction,
    EditCredentialSecret(char),
    BackspaceCredentialSecret,
    ClearCredentialSecret,
    SubmitCredentialSecret,
    ConfirmCredentialAction,
    CancelCredentialFlow,
    ConfirmConfigDelete,
    PreviewConfig,
    CommitConfig,
    DiscardConfig,
    EditConfigText(char),
    BackspaceConfigText,
    ClearConfigText,
    SubmitConfigText,
    CancelDiscard,
    Back,
    Resize(u16, u16),
    Quit,
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalInputContext {
    SlashPanel,
    ModelSelector,
    ModelDiscoveryDialect,
    Stats,
    AgentCenter,
    AgentActions,
    AgentConfirmation,
    BlockerCenter,
    ToolApproval,
    ProductOutput,
    ConfigObjectId,
    ConfigObject,
    ConfigChoice,
    ConfigText,
    ConfigCredentialReference,
    CredentialActions,
    CredentialSecret,
    CredentialConfirmation,
    ConfigDeleteConfirmation,
    DiscardConfirmation,
    Other,
}

struct TerminalInputState {
    slash_query: String,
}

impl TerminalInputState {
    fn new(slash_query: &str) -> Result<Self, TerminalError> {
        if !slash_query.starts_with('/')
            || slash_query.len() > MAX_COMMAND_QUERY_BYTES
            || slash_query.chars().any(char::is_control)
        {
            return Err(TerminalError::InvalidQuery);
        }
        Ok(Self {
            slash_query: slash_query.to_owned(),
        })
    }

    fn apply(
        &mut self,
        event: TerminalInputEvent,
        context: TerminalInputContext,
    ) -> TerminalIntent {
        match event {
            TerminalInputEvent::RefreshSnapshot
                if matches!(
                    context,
                    TerminalInputContext::SlashPanel
                        | TerminalInputContext::ModelSelector
                        | TerminalInputContext::Stats
                        | TerminalInputContext::AgentCenter
                        | TerminalInputContext::BlockerCenter
                ) =>
            {
                TerminalIntent::RefreshSnapshot
            }
            TerminalInputEvent::TestProviderConnection
                if context == TerminalInputContext::ModelSelector =>
            {
                TerminalIntent::RefreshProviderDiscovery
            }
            TerminalInputEvent::TestProviderConnection
                if !matches!(
                    context,
                    TerminalInputContext::CredentialActions
                        | TerminalInputContext::CredentialSecret
                        | TerminalInputContext::CredentialConfirmation
                ) =>
            {
                TerminalIntent::TestProviderConnection
            }
            TerminalInputEvent::CredentialActions
                if context == TerminalInputContext::ConfigCredentialReference =>
            {
                TerminalIntent::OpenCredentialActions
            }
            TerminalInputEvent::Up if context == TerminalInputContext::CredentialActions => {
                TerminalIntent::MoveCredentialAction(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::CredentialActions => {
                TerminalIntent::MoveCredentialAction(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::CredentialActions => {
                TerminalIntent::ActivateCredentialAction
            }
            TerminalInputEvent::Character(character)
                if context == TerminalInputContext::CredentialSecret && !character.is_control() =>
            {
                TerminalIntent::EditCredentialSecret(character)
            }
            TerminalInputEvent::Backspace if context == TerminalInputContext::CredentialSecret => {
                TerminalIntent::BackspaceCredentialSecret
            }
            TerminalInputEvent::Delete if context == TerminalInputContext::CredentialSecret => {
                TerminalIntent::ClearCredentialSecret
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::CredentialSecret => {
                TerminalIntent::SubmitCredentialSecret
            }
            TerminalInputEvent::Enter
                if context == TerminalInputContext::CredentialConfirmation =>
            {
                TerminalIntent::ConfirmCredentialAction
            }
            TerminalInputEvent::Escape
                if matches!(
                    context,
                    TerminalInputContext::CredentialActions
                        | TerminalInputContext::CredentialSecret
                        | TerminalInputContext::CredentialConfirmation
                ) =>
            {
                TerminalIntent::CancelCredentialFlow
            }
            TerminalInputEvent::Character(character)
                if context == TerminalInputContext::SlashPanel
                    && !character.is_control()
                    && self.slash_query.len().saturating_add(character.len_utf8())
                        <= MAX_COMMAND_QUERY_BYTES =>
            {
                self.slash_query.push(character);
                TerminalIntent::SetSlashQuery(self.slash_query.clone())
            }
            TerminalInputEvent::Backspace if context == TerminalInputContext::SlashPanel => {
                if self.slash_query != "/" {
                    let last =
                        UnicodeSegmentation::grapheme_indices(self.slash_query.as_str(), true)
                            .next_back()
                            .map_or(1, |(index, _)| index);
                    self.slash_query.truncate(last.max(1));
                }
                TerminalIntent::SetSlashQuery(self.slash_query.clone())
            }
            TerminalInputEvent::Up if context == TerminalInputContext::SlashPanel => {
                TerminalIntent::MoveSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::SlashPanel => {
                TerminalIntent::MoveSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::SlashPanel => {
                TerminalIntent::Activate
            }
            TerminalInputEvent::Character(character)
                if context == TerminalInputContext::ModelSelector && !character.is_control() =>
            {
                TerminalIntent::EditModelQuery(character)
            }
            TerminalInputEvent::Backspace if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::BackspaceModelQuery
            }
            TerminalInputEvent::Delete if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::ClearModelQuery
            }
            TerminalInputEvent::Tab if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::MoveModelGroup(1)
            }
            TerminalInputEvent::BackTab if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::MoveModelGroup(-1)
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::MoveModelSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::MoveModelSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ModelSelector => {
                TerminalIntent::ToggleModelDetail
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ModelDiscoveryDialect => {
                TerminalIntent::MoveDiscoveredModelDialect(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ModelDiscoveryDialect => {
                TerminalIntent::MoveDiscoveredModelDialect(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ModelDiscoveryDialect => {
                TerminalIntent::ConfirmDiscoveredModelDialect
            }
            TerminalInputEvent::Up if context == TerminalInputContext::Stats => {
                TerminalIntent::MoveStatsSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::Stats => {
                TerminalIntent::MoveStatsSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::Stats => {
                TerminalIntent::ToggleStatsDetail
            }
            TerminalInputEvent::Tab if context == TerminalInputContext::Stats => {
                TerminalIntent::MoveStatsGroup(1)
            }
            TerminalInputEvent::BackTab if context == TerminalInputContext::Stats => {
                TerminalIntent::MoveStatsGroup(-1)
            }
            TerminalInputEvent::Up if context == TerminalInputContext::AgentCenter => {
                TerminalIntent::MoveAgentSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::AgentCenter => {
                TerminalIntent::MoveAgentSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::AgentCenter => {
                TerminalIntent::ToggleAgentDetail
            }
            TerminalInputEvent::Character('a') if context == TerminalInputContext::AgentCenter => {
                TerminalIntent::OpenAgentActions
            }
            TerminalInputEvent::Character('A') if context == TerminalInputContext::AgentCenter => {
                TerminalIntent::OpenAgentActions
            }
            TerminalInputEvent::Up if context == TerminalInputContext::AgentActions => {
                TerminalIntent::MoveAgentAction(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::AgentActions => {
                TerminalIntent::MoveAgentAction(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::AgentActions => {
                TerminalIntent::ActivateAgentAction
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::AgentConfirmation => {
                TerminalIntent::ConfirmAgentAction
            }
            TerminalInputEvent::Escape
                if matches!(
                    context,
                    TerminalInputContext::AgentActions | TerminalInputContext::AgentConfirmation
                ) =>
            {
                TerminalIntent::CancelAgentFlow
            }
            TerminalInputEvent::Up if context == TerminalInputContext::BlockerCenter => {
                TerminalIntent::MoveBlockerSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::BlockerCenter => {
                TerminalIntent::MoveBlockerSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::BlockerCenter => {
                TerminalIntent::ActivateBlocker
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ToolApproval => {
                TerminalIntent::MoveToolApprovalSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ToolApproval => {
                TerminalIntent::MoveToolApprovalSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ToolApproval => {
                TerminalIntent::ActivateToolApproval
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ProductOutput => {
                TerminalIntent::MoveProductOutputSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ProductOutput => {
                TerminalIntent::MoveProductOutputSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ProductOutput => {
                TerminalIntent::AcknowledgeProductOutput
            }
            TerminalInputEvent::Character(character)
                if context == TerminalInputContext::ConfigObjectId && !character.is_control() =>
            {
                TerminalIntent::EditConfigObjectId(character)
            }
            TerminalInputEvent::Backspace if context == TerminalInputContext::ConfigObjectId => {
                TerminalIntent::BackspaceConfigObjectId
            }
            TerminalInputEvent::Delete if context == TerminalInputContext::ConfigObjectId => {
                TerminalIntent::ClearConfigObjectId
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ConfigObjectId => {
                TerminalIntent::SubmitConfigObjectId
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ConfigObject => {
                TerminalIntent::MoveConfigObjectSelection(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ConfigObject => {
                TerminalIntent::MoveConfigObjectSelection(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ConfigObject => {
                TerminalIntent::ActivateConfigObject
            }
            TerminalInputEvent::Up if context == TerminalInputContext::ConfigChoice => {
                TerminalIntent::MoveConfigChoice(-1)
            }
            TerminalInputEvent::Down if context == TerminalInputContext::ConfigChoice => {
                TerminalIntent::MoveConfigChoice(1)
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::ConfigChoice => {
                TerminalIntent::PreviewConfig
            }
            TerminalInputEvent::Character('c') if context == TerminalInputContext::ConfigChoice => {
                TerminalIntent::CommitConfig
            }
            TerminalInputEvent::Character('d') if context == TerminalInputContext::ConfigChoice => {
                TerminalIntent::DiscardConfig
            }
            TerminalInputEvent::Tab
                if matches!(
                    context,
                    TerminalInputContext::ConfigChoice
                        | TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) =>
            {
                TerminalIntent::MoveConfigField(1)
            }
            TerminalInputEvent::BackTab
                if matches!(
                    context,
                    TerminalInputContext::ConfigChoice
                        | TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) =>
            {
                TerminalIntent::MoveConfigField(-1)
            }
            TerminalInputEvent::Character(character)
                if matches!(
                    context,
                    TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) && !character.is_control() =>
            {
                TerminalIntent::EditConfigText(character)
            }
            TerminalInputEvent::Backspace
                if matches!(
                    context,
                    TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) =>
            {
                TerminalIntent::BackspaceConfigText
            }
            TerminalInputEvent::Delete
                if matches!(
                    context,
                    TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) =>
            {
                TerminalIntent::ClearConfigText
            }
            TerminalInputEvent::Enter
                if matches!(
                    context,
                    TerminalInputContext::ConfigText
                        | TerminalInputContext::ConfigCredentialReference
                ) =>
            {
                TerminalIntent::SubmitConfigText
            }
            TerminalInputEvent::Enter if context == TerminalInputContext::DiscardConfirmation => {
                TerminalIntent::DiscardConfig
            }
            TerminalInputEvent::Escape if context == TerminalInputContext::DiscardConfirmation => {
                TerminalIntent::CancelDiscard
            }
            TerminalInputEvent::Enter
                if context == TerminalInputContext::ConfigDeleteConfirmation =>
            {
                TerminalIntent::ConfirmConfigDelete
            }
            TerminalInputEvent::Escape
                if context == TerminalInputContext::ConfigDeleteConfirmation =>
            {
                TerminalIntent::DiscardConfig
            }
            TerminalInputEvent::Escape if context == TerminalInputContext::ToolApproval => {
                TerminalIntent::CancelToolApproval
            }
            TerminalInputEvent::Escape if context == TerminalInputContext::SlashPanel => {
                TerminalIntent::Quit
            }
            TerminalInputEvent::Escape => TerminalIntent::Back,
            TerminalInputEvent::Resize(width, height) => TerminalIntent::Resize(width, height),
            TerminalInputEvent::Quit => TerminalIntent::Quit,
            TerminalInputEvent::Character(_)
            | TerminalInputEvent::Backspace
            | TerminalInputEvent::Delete
            | TerminalInputEvent::Tab
            | TerminalInputEvent::BackTab
            | TerminalInputEvent::Up
            | TerminalInputEvent::Down
            | TerminalInputEvent::Enter
            | TerminalInputEvent::TestProviderConnection
            | TerminalInputEvent::CredentialActions
            | TerminalInputEvent::RefreshSnapshot
            | TerminalInputEvent::Ignore => TerminalIntent::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalLoopOutcome {
    Redraw,
    Resize(u16, u16),
    RefreshSnapshot,
    RefreshProviderDiscovery,
    RefreshProviderDiscoveryOnOpen,
    BeginDiscoveryAcceptance,
    ConfirmDiscoveryAcceptance,
    ApplyModelSelection,
    CancelAgent(u64),
    AcknowledgeTeamOperation(u64),
    LoadToolApproval(u64),
    ResolveToolApproval,
    CancelToolApproval,
    AcknowledgeProductOutput(u64),
    ResolveCredential,
    Quit,
    Noop,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CredentialAction {
    Bind,
    Replace,
    Test,
    Forget,
}

impl CredentialAction {
    const ALL: [Self; 4] = [Self::Bind, Self::Replace, Self::Test, Self::Forget];
}

enum CredentialFlow {
    Actions {
        scope: ProviderCredentialScope,
        selected: usize,
    },
    Secret {
        scope: ProviderCredentialScope,
        action: CredentialAction,
        input: CredentialSecretInput,
    },
    ConfirmReplace {
        scope: ProviderCredentialScope,
        secret: SecretValue,
    },
    ConfirmForget {
        scope: ProviderCredentialScope,
    },
}

#[derive(Clone, Copy)]
enum AgentLifecycleFlow {
    Actions {
        agent: u64,
        cancellable: bool,
        pending_operation: Option<u64>,
        selected: usize,
    },
    ConfirmCancel {
        agent: u64,
    },
    ConfirmAcknowledgement {
        operation: u64,
    },
}

struct CredentialSecretInput {
    bytes: Vec<u8>,
}

impl Default for CredentialSecretInput {
    fn default() -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_SECRET_BYTES),
        }
    }
}

impl CredentialSecretInput {
    fn push(&mut self, character: char) -> bool {
        if self.bytes.len().saturating_add(character.len_utf8()) > MAX_SECRET_BYTES {
            return false;
        }
        let mut encoded = [0_u8; 4];
        self.bytes
            .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        true
    }

    fn backspace(&mut self) {
        let Ok(value) = std::str::from_utf8(&self.bytes) else {
            self.clear();
            return;
        };
        let new_len = value
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.bytes[new_len..].fill(0);
        self.bytes.truncate(new_len);
    }

    fn clear(&mut self) {
        self.bytes.fill(0);
        self.bytes.clear();
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn take_secret(&mut self) -> Result<SecretValue, CredentialVaultError> {
        let mut bytes = Vec::with_capacity(MAX_SECRET_BYTES);
        std::mem::swap(&mut bytes, &mut self.bytes);
        SecretValue::new(bytes)
    }
}

impl Drop for CredentialSecretInput {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

enum CredentialCommand {
    Bind {
        scope: ProviderCredentialScope,
        secret: SecretValue,
    },
    Replace {
        scope: ProviderCredentialScope,
        secret: SecretValue,
    },
    Test {
        scope: ProviderCredentialScope,
    },
    Forget {
        scope: ProviderCredentialScope,
    },
}

impl CredentialCommand {
    fn scope(&self) -> &ProviderCredentialScope {
        match self {
            Self::Bind { scope, .. }
            | Self::Replace { scope, .. }
            | Self::Test { scope }
            | Self::Forget { scope } => scope,
        }
    }
}

enum CredentialCommandOutcome {
    Completed,
    Available,
    Missing,
    Forgotten,
}

struct TerminalSession {
    controller: PresentationController,
    input: TerminalInputState,
    viewport: Viewport,
    validated_config_choice: Option<String>,
    config_text: Option<ConfigTextInput>,
    validated_config_text: Option<String>,
    confirming_discard: bool,
    pending_model_selection: Option<ModelPresetView>,
    pending_discovery_acceptance: Option<DiscoveredModelAcceptance>,
    pending_tool_action: Option<(u64, ProductToolDecision)>,
    credential_flow: Option<CredentialFlow>,
    pending_credential_command: Option<CredentialCommand>,
    agent_flow: Option<AgentLifecycleFlow>,
    notice: Option<String>,
}

struct ConfigTextInput {
    value: String,
    replace_on_edit: bool,
    pending: bool,
}

impl TerminalSession {
    fn new(query: &str, width: u16, height: u16) -> Result<Self, TerminalError> {
        let mut controller = PresentationController::new();
        controller.set_slash_query(query)?;
        Ok(Self {
            controller,
            input: TerminalInputState::new(query)?,
            viewport: Viewport::new(width, height)?,
            validated_config_choice: None,
            config_text: None,
            validated_config_text: None,
            confirming_discard: false,
            pending_model_selection: None,
            pending_discovery_acceptance: None,
            pending_tool_action: None,
            credential_flow: None,
            pending_credential_command: None,
            agent_flow: None,
            notice: None,
        })
    }

    #[cfg(test)]
    fn handle(
        &mut self,
        event: TerminalInputEvent,
        runtime: Option<&mut ConfigRuntime>,
    ) -> Result<TerminalLoopOutcome, TerminalError> {
        self.handle_with_connection_tester(event, runtime, None)
    }

    #[cfg(test)]
    fn handle_with_connection_tester(
        &mut self,
        event: TerminalInputEvent,
        runtime: Option<&mut ConfigRuntime>,
        tester: Option<&mut dyn ProviderConnectionTester>,
    ) -> Result<TerminalLoopOutcome, TerminalError> {
        self.handle_with_view_and_connection_tester(event, runtime, None, tester)
    }

    fn handle_with_view_and_connection_tester(
        &mut self,
        event: TerminalInputEvent,
        runtime: Option<&mut ConfigRuntime>,
        view: Option<&TuiViewModel>,
        tester: Option<&mut dyn ProviderConnectionTester>,
    ) -> Result<TerminalLoopOutcome, TerminalError> {
        let intent = self.input.apply(event, self.input_context());
        match intent {
            TerminalIntent::SetSlashQuery(query) => {
                self.controller.set_slash_query(&query)?;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveSelection(offset) => {
                self.controller.move_selection(offset)?;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Activate => {
                let was_model_selector = self.controller.is_model_selector();
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self.controller.activate(runtime, ConfigScope::User, None) {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.sync_config_text();
                        self.notice = None;
                        if !was_model_selector
                            && self.controller.is_model_selector()
                            && can_refresh_terminal_discovery_on_open(runtime)
                        {
                            return Ok(TerminalLoopOutcome::RefreshProviderDiscoveryOnOpen);
                        }
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::EditModelQuery(character) => {
                match self.controller.edit_model_query(Some(character)) {
                    Ok(()) => self.notice = None,
                    Err(_) => self.notice = Some("Model query exceeds its input limit".to_owned()),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::BackspaceModelQuery => {
                self.controller
                    .edit_model_query(None)
                    .map_err(TerminalError::PresentationModel)?;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ClearModelQuery => {
                self.controller
                    .clear_model_query()
                    .map_err(TerminalError::PresentationModel)?;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveModelGroup(offset) => {
                self.controller.move_model_group(offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveModelSelection(offset) => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller.move_model_selection(&view.models, offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ToggleModelDetail => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                match self.controller.activate_model_entry(&view.models) {
                    ModelEntryAction::DetailChanged => {
                        self.notice = None;
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    ModelEntryAction::ApplyConfigured(preset) => {
                        self.pending_model_selection = Some(preset);
                        Ok(TerminalLoopOutcome::ApplyModelSelection)
                    }
                    ModelEntryAction::UpdateConfiguredStarter(preset_id) => {
                        let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                        match self.controller.begin_model_starter_update(
                            runtime,
                            ConfigScope::User,
                            &preset_id,
                        ) {
                            Ok(()) => {
                                self.validated_config_choice = None;
                                self.sync_config_text();
                                self.notice = Some(
                                    "Release starter update staged; preview before commit"
                                        .to_owned(),
                                );
                            }
                            Err(source) => self.notice = Some(presentation_notice(&source)),
                        }
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    ModelEntryAction::AcceptRelease => {
                        self.notice =
                            Some("Enter a Preset ID to accept this release starter".to_owned());
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    ModelEntryAction::AcceptDiscovery(acceptance) => {
                        self.pending_discovery_acceptance = Some(acceptance);
                        Ok(TerminalLoopOutcome::BeginDiscoveryAcceptance)
                    }
                    ModelEntryAction::ReleaseUnavailable => {
                        self.notice = Some(
                            "Release model is incompatible with the selected Provider Profile"
                                .to_owned(),
                        );
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    ModelEntryAction::DiscoveryUnavailable => {
                        self.notice = Some(
                            "Discovered model requires an explicit trusted dialect".to_owned(),
                        );
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    ModelEntryAction::None => Ok(TerminalLoopOutcome::Noop),
                }
            }
            TerminalIntent::RefreshProviderDiscovery => {
                self.notice = None;
                Ok(TerminalLoopOutcome::RefreshProviderDiscovery)
            }
            TerminalIntent::MoveDiscoveredModelDialect(offset) => {
                self.controller.move_discovered_model_dialect(offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ConfirmDiscoveredModelDialect => {
                Ok(TerminalLoopOutcome::ConfirmDiscoveryAcceptance)
            }
            TerminalIntent::MoveStatsSelection(offset) => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller.move_stats_selection(&view.stats, offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveStatsGroup(offset) => {
                self.controller.move_stats_group(offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ToggleStatsDetail => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller.toggle_stats_detail(&view.stats);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveAgentSelection(offset) => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller.move_agent_selection(&view.agents, offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ToggleAgentDetail => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller.toggle_agent_detail(&view.agents);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::OpenAgentActions => {
                let target = view
                    .and_then(|view| self.controller.selected_agent_action_target(&view.agents));
                if let Some(target) = target {
                    self.agent_flow = Some(AgentLifecycleFlow::Actions {
                        agent: target.agent,
                        cancellable: target.cancellable,
                        pending_operation: target.pending_operation,
                        selected: 0,
                    });
                    self.notice = None;
                } else {
                    self.notice = Some("Agent actions unavailable".to_owned());
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveAgentAction(offset) => {
                if let Some(AgentLifecycleFlow::Actions {
                    cancellable,
                    pending_operation,
                    selected,
                    ..
                }) = self.agent_flow.as_mut()
                {
                    let count = usize::from(*cancellable)
                        .saturating_add(usize::from(pending_operation.is_some()))
                        .saturating_add(1);
                    *selected = selected.saturating_add_signed(offset).min(count - 1);
                }
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ActivateAgentAction => {
                let Some(AgentLifecycleFlow::Actions {
                    agent,
                    cancellable,
                    pending_operation,
                    selected,
                }) = self.agent_flow
                else {
                    return Ok(TerminalLoopOutcome::Noop);
                };
                let mut index = 0;
                if cancellable {
                    if selected == index {
                        self.agent_flow = Some(AgentLifecycleFlow::ConfirmCancel { agent });
                        return Ok(TerminalLoopOutcome::Redraw);
                    }
                    index += 1;
                }
                if let Some(operation) = pending_operation
                    && selected == index
                {
                    self.agent_flow =
                        Some(AgentLifecycleFlow::ConfirmAcknowledgement { operation });
                    return Ok(TerminalLoopOutcome::Redraw);
                }
                self.agent_flow = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ConfirmAgentAction => match self.agent_flow.take() {
                Some(AgentLifecycleFlow::ConfirmCancel { agent }) => {
                    Ok(TerminalLoopOutcome::CancelAgent(agent))
                }
                Some(AgentLifecycleFlow::ConfirmAcknowledgement { operation }) => {
                    Ok(TerminalLoopOutcome::AcknowledgeTeamOperation(operation))
                }
                _ => Ok(TerminalLoopOutcome::Noop),
            },
            TerminalIntent::CancelAgentFlow => {
                self.agent_flow = None;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveBlockerSelection(offset) => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                self.controller
                    .move_blocker_selection(&view.blockers, offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ActivateBlocker => {
                let view = view.ok_or(TerminalError::ViewModelRequired)?;
                match self.controller.activate_blocker(&view.blockers) {
                    BlockerEntryAction::DetailChanged => {
                        self.notice = None;
                        Ok(TerminalLoopOutcome::Redraw)
                    }
                    BlockerEntryAction::LoadToolApproval { call } => {
                        Ok(TerminalLoopOutcome::LoadToolApproval(call))
                    }
                    BlockerEntryAction::None => Ok(TerminalLoopOutcome::Noop),
                }
            }
            TerminalIntent::MoveToolApprovalSelection(offset) => {
                self.controller
                    .move_tool_approval_selection(offset, usize::from(self.viewport.width()));
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ActivateToolApproval => {
                match self
                    .controller
                    .activate_tool_approval(usize::from(self.viewport.width()))
                {
                    ToolApprovalEntryAction::Resolve { call, decision } => {
                        self.pending_tool_action = Some((
                            call,
                            match decision {
                                ToolApprovalAction::Approve => ProductToolDecision::Approve,
                                ToolApprovalAction::Deny => ProductToolDecision::Deny,
                            },
                        ));
                        Ok(TerminalLoopOutcome::ResolveToolApproval)
                    }
                    ToolApprovalEntryAction::None => Ok(TerminalLoopOutcome::Noop),
                }
            }
            TerminalIntent::CancelToolApproval => Ok(TerminalLoopOutcome::CancelToolApproval),
            TerminalIntent::MoveProductOutputSelection(offset) => {
                self.controller.move_product_output_selection(offset);
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::AcknowledgeProductOutput => self
                .controller
                .selected_product_delivery()
                .map_or(Ok(TerminalLoopOutcome::Noop), |delivery| {
                    Ok(TerminalLoopOutcome::AcknowledgeProductOutput(delivery))
                }),
            TerminalIntent::RefreshSnapshot => {
                self.notice = None;
                Ok(TerminalLoopOutcome::RefreshSnapshot)
            }
            TerminalIntent::EditConfigObjectId(character) => {
                self.edit_config_object_id(Some(character));
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::BackspaceConfigObjectId => {
                self.edit_config_object_id(None);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ClearConfigObjectId => {
                match self.controller.set_config_object_id("") {
                    Ok(()) => self.notice = None,
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::SubmitConfigObjectId => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self
                    .controller
                    .submit_config_object_id(runtime, ConfigScope::User)
                {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.sync_config_text();
                        self.notice = None;
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveConfigObjectSelection(offset) => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self
                    .controller
                    .move_config_object_selection(runtime, offset)
                {
                    Ok(()) => self.notice = None,
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ActivateConfigObject => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self
                    .controller
                    .activate_config_object(runtime, ConfigScope::User)
                {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.sync_config_text();
                        self.notice = None;
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveConfigField(offset) => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                if !self.flush_config_text(runtime) {
                    return Ok(TerminalLoopOutcome::Redraw);
                }
                match self.controller.move_config_field(runtime, offset) {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.sync_config_text();
                        self.notice = None;
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveConfigChoice(offset) => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self.move_config_choice(runtime, offset) {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.notice = None;
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::TestProviderConnection => {
                if !self.controller.is_provider_wizard() {
                    return Ok(TerminalLoopOutcome::Noop);
                }
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                let Some(tester) = tester else {
                    self.notice = Some("Provider connection test unavailable".to_owned());
                    return Ok(TerminalLoopOutcome::Redraw);
                };
                match self.controller.test_provider_connection(runtime, tester) {
                    Ok(_) => self.notice = None,
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::OpenCredentialActions => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                self.open_credential_actions(runtime);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::MoveCredentialAction(offset) => {
                self.move_credential_action(offset);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ActivateCredentialAction => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                if self.activate_credential_action(runtime) {
                    Ok(TerminalLoopOutcome::ResolveCredential)
                } else {
                    Ok(TerminalLoopOutcome::Redraw)
                }
            }
            TerminalIntent::EditCredentialSecret(character) => {
                self.edit_credential_secret(Some(character));
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::BackspaceCredentialSecret => {
                self.edit_credential_secret(None);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ClearCredentialSecret => {
                self.clear_credential_secret();
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::SubmitCredentialSecret => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                if self.submit_credential_secret(runtime) {
                    Ok(TerminalLoopOutcome::ResolveCredential)
                } else {
                    Ok(TerminalLoopOutcome::Redraw)
                }
            }
            TerminalIntent::ConfirmCredentialAction => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                if self.confirm_credential_action(runtime) {
                    Ok(TerminalLoopOutcome::ResolveCredential)
                } else {
                    Ok(TerminalLoopOutcome::Redraw)
                }
            }
            TerminalIntent::CancelCredentialFlow => {
                self.cancel_credential_flow();
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ConfirmConfigDelete => {
                if !self.controller.is_config_object_delete() {
                    return Ok(TerminalLoopOutcome::Noop);
                }
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self.controller.preview_config(runtime) {
                    Ok(_) => match self.controller.commit_config(runtime) {
                        Ok(_) => {
                            self.validated_config_choice = None;
                            self.clear_config_text();
                            self.notice = None;
                        }
                        Err(source) => self.notice = Some(presentation_notice(&source)),
                    },
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::PreviewConfig => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                let choice = self.config_choice().map(str::to_owned);
                match self.controller.preview_config(runtime) {
                    Ok(_) => {
                        self.validated_config_choice = choice;
                        self.notice = Some("Config draft validated".to_owned());
                    }
                    Err(source) => {
                        self.validated_config_choice = None;
                        self.notice = Some(presentation_notice(&source));
                    }
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::CommitConfig => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                if !self.controller.has_unsaved_config_draft() {
                    self.notice = Some("No Config changes to commit".to_owned());
                } else if self.config_choice().map(str::to_owned) != self.validated_config_choice {
                    self.notice = Some("Config draft must be previewed before commit".to_owned());
                } else {
                    match self.controller.commit_config(runtime) {
                        Ok(_) => {
                            self.validated_config_choice = None;
                            self.notice = None;
                        }
                        Err(source) => self.notice = Some(presentation_notice(&source)),
                    }
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::DiscardConfig => {
                self.controller.discard_config()?;
                self.validated_config_choice = None;
                self.clear_config_text();
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::EditConfigText(character) => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                self.edit_config_text(runtime, Some(character))?;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::BackspaceConfigText => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                self.edit_config_text(runtime, None)?;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::ClearConfigText => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                self.reset_config_text(runtime);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::SubmitConfigText => {
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                self.submit_config_text(runtime);
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::CancelDiscard => {
                self.confirming_discard = false;
                self.notice = None;
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Back => {
                if self.controller.is_product_output() {
                    self.notice = Some("Provider output must be acknowledged".to_owned());
                    return Ok(TerminalLoopOutcome::Redraw);
                }
                if self.config_text.is_some() && self.has_unsaved_config_input() {
                    self.confirming_discard = true;
                    self.notice = Some("Discard Config draft?".to_owned());
                    return Ok(TerminalLoopOutcome::Redraw);
                }
                match self.controller.back() {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.clear_config_text();
                        self.notice = None;
                    }
                    Err(PresentationControllerError::UnsavedConfigDraft) => {
                        self.notice =
                            Some("Config draft must be committed or discarded".to_owned());
                    }
                    Err(source) => return Err(source.into()),
                }
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Resize(width, height) => {
                let old_width = usize::from(self.viewport.width());
                self.viewport = Viewport::new(width, height)?;
                self.controller
                    .reflow_tool_approval_selection(old_width, usize::from(width));
                Ok(TerminalLoopOutcome::Resize(width, height))
            }
            TerminalIntent::Quit if self.controller.is_product_output() => {
                self.notice = Some("Provider output must be acknowledged".to_owned());
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Quit if self.has_unsaved_config_input() => {
                self.notice = Some("Config draft must be committed or discarded".to_owned());
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Quit => Ok(TerminalLoopOutcome::Quit),
            TerminalIntent::None => Ok(TerminalLoopOutcome::Noop),
        }
    }

    fn take_model_selection(&mut self) -> Option<ModelPresetView> {
        self.pending_model_selection.take()
    }

    fn take_discovery_acceptance(&mut self) -> Option<DiscoveredModelAcceptance> {
        self.pending_discovery_acceptance.take()
    }

    fn take_tool_action(&mut self) -> Option<(u64, ProductToolDecision)> {
        self.pending_tool_action.take()
    }

    fn take_credential_command(&mut self) -> Option<CredentialCommand> {
        self.pending_credential_command.take()
    }

    fn validate_credential_command_scope(
        &mut self,
        runtime: &ConfigRuntime,
        command: &CredentialCommand,
    ) -> bool {
        if matches!(
            self.current_credential_scope(runtime),
            Ok(scope) if scope == *command.scope()
        ) {
            return true;
        }
        self.credential_flow = None;
        self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
        false
    }

    fn show_product_output(&mut self, delivery: u64, text: String) {
        self.controller.show_product_output(delivery, text);
        self.notice = Some("Provider output prepared".to_owned());
    }

    fn open_credential_actions(&mut self, runtime: &ConfigRuntime) {
        if self.has_unsaved_config_input() {
            self.notice = Some("Commit the Provider Profile before credential actions".to_owned());
            return;
        }
        match self.current_credential_scope(runtime) {
            Ok(scope) => {
                self.credential_flow = Some(CredentialFlow::Actions { scope, selected: 0 });
                self.notice = None;
            }
            Err(()) => {
                self.notice = Some(
                    "Credential actions require a committed reference and Provider origin"
                        .to_owned(),
                );
            }
        }
    }

    fn current_credential_scope(
        &self,
        runtime: &ConfigRuntime,
    ) -> Result<ProviderCredentialScope, ()> {
        self.controller
            .current_provider_profile(runtime)
            .map_err(|_| ())
            .and_then(|profile| ProviderCredentialScope::from_profile(&profile).map_err(|_| ()))
    }

    fn move_credential_action(&mut self, offset: isize) {
        let Some(CredentialFlow::Actions { selected, .. }) = self.credential_flow.as_mut() else {
            return;
        };
        *selected = selected
            .saturating_add_signed(offset)
            .min(CredentialAction::ALL.len() - 1);
        self.notice = None;
    }

    fn activate_credential_action(&mut self, runtime: &ConfigRuntime) -> bool {
        let Ok(current_scope) = self.current_credential_scope(runtime) else {
            self.credential_flow = None;
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        };
        let Some(CredentialFlow::Actions { scope, selected }) = self.credential_flow.take() else {
            return false;
        };
        if scope != current_scope {
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        }
        let Some(action) = CredentialAction::ALL.get(selected).copied() else {
            self.credential_flow = Some(CredentialFlow::Actions { scope, selected: 0 });
            return false;
        };
        match action {
            CredentialAction::Bind | CredentialAction::Replace => {
                self.credential_flow = Some(CredentialFlow::Secret {
                    scope,
                    action,
                    input: CredentialSecretInput::default(),
                });
                self.notice = None;
                false
            }
            CredentialAction::Test => {
                self.pending_credential_command = Some(CredentialCommand::Test {
                    scope: scope.clone(),
                });
                self.credential_flow = Some(CredentialFlow::Actions { scope, selected });
                self.notice = None;
                true
            }
            CredentialAction::Forget => {
                self.credential_flow = Some(CredentialFlow::ConfirmForget { scope });
                self.notice = Some("Confirm credential removal".to_owned());
                false
            }
        }
    }

    fn edit_credential_secret(&mut self, character: Option<char>) {
        let Some(CredentialFlow::Secret { input, .. }) = self.credential_flow.as_mut() else {
            return;
        };
        match character {
            Some(character) if !input.push(character) => {
                self.notice = Some("Credential value exceeds its input limit".to_owned());
                return;
            }
            Some(_) => {}
            None => input.backspace(),
        }
        self.notice = None;
    }

    fn clear_credential_secret(&mut self) {
        let Some(CredentialFlow::Secret { input, .. }) = self.credential_flow.as_mut() else {
            return;
        };
        input.clear();
        self.notice = None;
    }

    fn submit_credential_secret(&mut self, runtime: &ConfigRuntime) -> bool {
        let Ok(current_scope) = self.current_credential_scope(runtime) else {
            self.credential_flow = None;
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        };
        let Some(CredentialFlow::Secret {
            scope,
            action,
            input,
        }) = self.credential_flow.as_mut()
        else {
            return false;
        };
        if *scope != current_scope {
            self.credential_flow = None;
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        }
        let secret = match input.take_secret() {
            Ok(secret) => secret,
            Err(_) => {
                self.notice = Some("Credential value is invalid".to_owned());
                return false;
            }
        };
        let scope = scope.clone();
        match action {
            CredentialAction::Bind => {
                self.pending_credential_command = Some(CredentialCommand::Bind { scope, secret });
            }
            CredentialAction::Replace => {
                self.credential_flow = Some(CredentialFlow::ConfirmReplace { scope, secret });
                self.notice = Some("Confirm credential replacement".to_owned());
                return false;
            }
            CredentialAction::Test | CredentialAction::Forget => {
                self.credential_flow = None;
                self.notice = Some("Credential action is unavailable".to_owned());
                return false;
            }
        }
        self.notice = None;
        true
    }

    fn confirm_credential_action(&mut self, runtime: &ConfigRuntime) -> bool {
        let Ok(current_scope) = self.current_credential_scope(runtime) else {
            self.credential_flow = None;
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        };
        let (scope, selected, command) = match self.credential_flow.take() {
            Some(CredentialFlow::ConfirmReplace { scope, secret }) => {
                let command = CredentialCommand::Replace {
                    scope: scope.clone(),
                    secret,
                };
                (scope, 1, command)
            }
            Some(CredentialFlow::ConfirmForget { scope }) => {
                let command = CredentialCommand::Forget {
                    scope: scope.clone(),
                };
                (scope, 3, command)
            }
            _ => return false,
        };
        if scope != current_scope {
            self.notice = Some("Credential scope changed; reopen credential actions".to_owned());
            return false;
        }
        self.pending_credential_command = Some(command);
        self.credential_flow = Some(CredentialFlow::Actions { scope, selected });
        self.notice = None;
        true
    }

    fn finish_credential_command(
        &mut self,
        action: CredentialAction,
        result: Result<CredentialCommandOutcome, CredentialVaultError>,
    ) {
        let scope = match self.credential_flow.take() {
            Some(CredentialFlow::Secret { scope, .. })
            | Some(CredentialFlow::ConfirmReplace { scope, .. })
            | Some(CredentialFlow::ConfirmForget { scope })
            | Some(CredentialFlow::Actions { scope, .. }) => scope,
            None => return,
        };
        let selected = CredentialAction::ALL
            .iter()
            .position(|candidate| *candidate == action)
            .unwrap_or(0);
        self.credential_flow = Some(CredentialFlow::Actions { scope, selected });
        self.notice = Some(match result {
            Ok(CredentialCommandOutcome::Completed) => match action {
                CredentialAction::Bind => "Credential bound".to_owned(),
                CredentialAction::Replace => "Credential replaced".to_owned(),
                CredentialAction::Test | CredentialAction::Forget => {
                    "Credential action completed".to_owned()
                }
            },
            Ok(CredentialCommandOutcome::Available) => "Credential available".to_owned(),
            Ok(CredentialCommandOutcome::Missing) => "Credential not found".to_owned(),
            Ok(CredentialCommandOutcome::Forgotten) => "Credential forgotten".to_owned(),
            Err(CredentialVaultError::AlreadyBound) => {
                "Credential is already bound; use Replace".to_owned()
            }
            Err(CredentialVaultError::NotFound) => "Credential was not found".to_owned(),
            Err(CredentialVaultError::Unavailable) => {
                "Platform credential vault is unavailable".to_owned()
            }
            Err(CredentialVaultError::InvalidScope(_)) => "Credential scope is invalid".to_owned(),
            Err(CredentialVaultError::InvalidSecret) => "Credential value is invalid".to_owned(),
        });
    }

    fn cancel_credential_flow(&mut self) {
        match self.credential_flow.take() {
            Some(CredentialFlow::Secret { scope, action, .. }) => {
                let selected = CredentialAction::ALL
                    .iter()
                    .position(|candidate| *candidate == action)
                    .unwrap_or(0);
                self.credential_flow = Some(CredentialFlow::Actions { scope, selected });
            }
            Some(CredentialFlow::ConfirmReplace { scope, .. }) => {
                self.credential_flow = Some(CredentialFlow::Actions { scope, selected: 1 });
            }
            Some(CredentialFlow::ConfirmForget { scope }) => {
                self.credential_flow = Some(CredentialFlow::Actions { scope, selected: 3 });
            }
            Some(CredentialFlow::Actions { .. }) | None => {}
        }
        self.pending_credential_command = None;
        self.notice = None;
    }

    fn input_context(&self) -> TerminalInputContext {
        if matches!(self.agent_flow, Some(AgentLifecycleFlow::Actions { .. })) {
            TerminalInputContext::AgentActions
        } else if matches!(
            self.agent_flow,
            Some(
                AgentLifecycleFlow::ConfirmCancel { .. }
                    | AgentLifecycleFlow::ConfirmAcknowledgement { .. }
            )
        ) {
            TerminalInputContext::AgentConfirmation
        } else if matches!(self.credential_flow, Some(CredentialFlow::Actions { .. })) {
            TerminalInputContext::CredentialActions
        } else if matches!(self.credential_flow, Some(CredentialFlow::Secret { .. })) {
            TerminalInputContext::CredentialSecret
        } else if matches!(
            self.credential_flow,
            Some(CredentialFlow::ConfirmReplace { .. })
                | Some(CredentialFlow::ConfirmForget { .. })
        ) {
            TerminalInputContext::CredentialConfirmation
        } else if self.confirming_discard {
            TerminalInputContext::DiscardConfirmation
        } else if self.controller.is_slash_panel() {
            TerminalInputContext::SlashPanel
        } else if self.controller.is_model_selector() {
            TerminalInputContext::ModelSelector
        } else if self.controller.is_model_discovery_dialect() {
            TerminalInputContext::ModelDiscoveryDialect
        } else if self.controller.is_stats() {
            TerminalInputContext::Stats
        } else if self.controller.is_agent_center() {
            TerminalInputContext::AgentCenter
        } else if self.controller.is_blocker_center() {
            TerminalInputContext::BlockerCenter
        } else if self.controller.is_tool_approval() {
            TerminalInputContext::ToolApproval
        } else if self.controller.is_product_output() {
            TerminalInputContext::ProductOutput
        } else if self.controller.is_config_object_create() {
            TerminalInputContext::ConfigObjectId
        } else if self.controller.is_config_object_selector() {
            TerminalInputContext::ConfigObject
        } else if self.controller.is_config_object_delete() {
            TerminalInputContext::ConfigDeleteConfirmation
        } else if let Some(field) = self.controller.config_editor_field() {
            match field.interaction {
                ConfigFieldInteraction::Choice { .. } => TerminalInputContext::ConfigChoice,
                ConfigFieldInteraction::Text { .. } => TerminalInputContext::ConfigText,
                ConfigFieldInteraction::CredentialReference { .. } => {
                    TerminalInputContext::ConfigCredentialReference
                }
                ConfigFieldInteraction::ReadOnly => TerminalInputContext::Other,
            }
        } else {
            TerminalInputContext::Other
        }
    }

    fn edit_config_object_id(&mut self, character: Option<char>) {
        let Some(current) = self.controller.config_object_id() else {
            return;
        };
        let mut next = current.to_owned();
        if let Some(character) = character {
            if character.is_control()
                || next.len().saturating_add(character.len_utf8()) > MAX_CONFIG_ID_BYTES
            {
                self.notice = Some("Config Object ID exceeds its input limit".to_owned());
                return;
            }
            next.push(character);
        } else {
            let last = UnicodeSegmentation::grapheme_indices(next.as_str(), true)
                .next_back()
                .map_or(0, |(index, _)| index);
            next.truncate(last);
        }
        match self.controller.set_config_object_id(&next) {
            Ok(()) => self.notice = None,
            Err(source) => self.notice = Some(presentation_notice(&source)),
        }
    }

    fn sync_config_text(&mut self) {
        let Some(field) = self.controller.config_editor_field() else {
            self.clear_config_text();
            return;
        };
        if !matches!(
            field.interaction,
            ConfigFieldInteraction::Text { .. }
                | ConfigFieldInteraction::CredentialReference { .. }
        ) {
            self.clear_config_text();
            return;
        }
        if matches!(
            field.interaction,
            ConfigFieldInteraction::CredentialReference { .. }
        ) {
            self.config_text = Some(ConfigTextInput {
                value: String::new(),
                replace_on_edit: true,
                pending: false,
            });
            self.validated_config_text = None;
            self.confirming_discard = false;
            return;
        }
        let ConfigFieldContents::Value {
            effective, target, ..
        } = &field.contents
        else {
            self.clear_config_text();
            return;
        };
        let value = target
            .as_ref()
            .or(effective.as_ref())
            .and_then(|value| match value {
                ConfigValue::String(value) => Some(value.clone()),
                ConfigValue::PositiveInteger(value) => Some(value.to_string()),
                ConfigValue::NonNegativeInteger(value) => Some(value.to_string()),
                ConfigValue::Boolean(value) => Some(value.to_string()),
                ConfigValue::StringList(values) => serde_json::to_string(values).ok(),
            })
            .unwrap_or_default();
        self.config_text = Some(ConfigTextInput {
            value,
            replace_on_edit: true,
            pending: false,
        });
        self.validated_config_text = None;
        self.confirming_discard = false;
    }

    fn clear_config_text(&mut self) {
        self.config_text = None;
        self.validated_config_text = None;
        self.confirming_discard = false;
    }

    fn edit_config_text(
        &mut self,
        runtime: &ConfigRuntime,
        character: Option<char>,
    ) -> Result<(), TerminalError> {
        let Some(current) = self.config_text.as_ref() else {
            return Ok(());
        };
        let Some(max_bytes) = self.config_text_limit() else {
            return Ok(());
        };
        let mut next = if current.replace_on_edit {
            String::new()
        } else {
            current.value.clone()
        };
        if let Some(character) = character {
            if character.is_control() || next.len().saturating_add(character.len_utf8()) > max_bytes
            {
                self.notice = Some("Config value exceeds its input limit".to_owned());
                return Ok(());
            }
            next.push(character);
        } else if current.replace_on_edit {
            self.reset_config_text(runtime);
            return Ok(());
        } else {
            let last = UnicodeSegmentation::grapheme_indices(next.as_str(), true)
                .next_back()
                .map_or(0, |(index, _)| index);
            next.truncate(last);
            if next.is_empty() {
                self.reset_config_text(runtime);
                return Ok(());
            }
        }
        if self.config_text_requires_deferred_stage() {
            self.config_text = Some(ConfigTextInput {
                value: next,
                replace_on_edit: false,
                pending: true,
            });
            self.validated_config_text = None;
            self.confirming_discard = false;
            self.notice = None;
        } else {
            self.stage_config_text(runtime, next);
        }
        Ok(())
    }

    fn config_text_requires_deferred_stage(&self) -> bool {
        self.controller.config_editor_field().is_some_and(|field| {
            !matches!(field.value_kind, ConfigValueKind::String)
                && matches!(field.interaction, ConfigFieldInteraction::Text { .. })
        })
    }

    fn config_text_limit(&self) -> Option<usize> {
        let field = self.controller.config_editor_field()?;
        match field.interaction {
            ConfigFieldInteraction::Text { max_bytes }
            | ConfigFieldInteraction::CredentialReference { max_bytes } => Some(max_bytes),
            ConfigFieldInteraction::ReadOnly | ConfigFieldInteraction::Choice { .. } => None,
        }
    }

    fn stage_config_text(&mut self, runtime: &ConfigRuntime, next: String) {
        let result = match self
            .controller
            .config_editor_field()
            .map(|field| field.interaction)
        {
            Some(ConfigFieldInteraction::CredentialReference { .. }) => self
                .controller
                .stage_provider_credential_reference(runtime, &next),
            _ => self.controller.stage_config(runtime, &next),
        };
        match result {
            Ok(()) => {
                self.config_text = Some(ConfigTextInput {
                    value: next,
                    replace_on_edit: false,
                    pending: false,
                });
                self.validated_config_text = None;
                self.confirming_discard = false;
                self.notice = None;
            }
            Err(source) => self.notice = Some(presentation_notice(&source)),
        }
    }

    fn reset_config_text(&mut self, runtime: &ConfigRuntime) {
        let result = match self
            .controller
            .config_editor_field()
            .map(|field| field.interaction)
        {
            Some(ConfigFieldInteraction::CredentialReference { .. }) => {
                self.controller.reset_provider_credential_reference(runtime)
            }
            _ => self.controller.reset_config(runtime),
        };
        match result {
            Ok(()) => {
                self.config_text = Some(ConfigTextInput {
                    value: String::new(),
                    replace_on_edit: true,
                    pending: false,
                });
                self.validated_config_text = None;
                self.confirming_discard = false;
                self.notice = None;
            }
            Err(source) => self.notice = Some(presentation_notice(&source)),
        }
    }

    fn submit_config_text(&mut self, runtime: &mut ConfigRuntime) {
        if !self.flush_config_text(runtime) {
            return;
        }
        let current = self.config_text.as_ref().map(|input| input.value.clone());
        if !self.controller.has_unsaved_config_draft() {
            self.notice = Some("No Config changes to commit".to_owned());
        } else if current != self.validated_config_text {
            match self.controller.preview_config(runtime) {
                Ok(_) => {
                    self.validated_config_text = current;
                    self.notice = Some("Config draft validated".to_owned());
                }
                Err(source) => {
                    self.validated_config_text = None;
                    self.notice = Some(presentation_notice(&source));
                }
            }
        } else {
            match self.controller.commit_config(runtime) {
                Ok(_) => {
                    self.clear_config_text();
                    self.notice = None;
                }
                Err(source) => self.notice = Some(presentation_notice(&source)),
            }
        }
    }

    fn flush_config_text(&mut self, runtime: &ConfigRuntime) -> bool {
        let Some(input) = self.config_text.as_ref().filter(|input| input.pending) else {
            return true;
        };
        let value = input.value.clone();
        match self.controller.stage_config(runtime, &value) {
            Ok(()) => {
                if let Some(input) = self.config_text.as_mut() {
                    input.pending = false;
                }
                self.notice = None;
                true
            }
            Err(source) => {
                self.notice = Some(presentation_notice(&source));
                false
            }
        }
    }

    fn has_unsaved_config_input(&self) -> bool {
        self.controller.has_unsaved_config_draft()
            || self.config_text.as_ref().is_some_and(|input| input.pending)
    }

    fn config_choice(&self) -> Option<&str> {
        let field = self.controller.config_editor_field()?;
        self.config_choices()?;
        let ConfigFieldContents::Value {
            effective, target, ..
        } = &field.contents
        else {
            return None;
        };
        target
            .as_ref()
            .or(effective.as_ref())
            .and_then(|value| match value {
                ConfigValue::String(value) => Some(value.as_str()),
                ConfigValue::Boolean(value) => Some(if *value { "true" } else { "false" }),
                _ => None,
            })
    }

    fn config_choices(&self) -> Option<&'static [&'static str]> {
        let field = self.controller.config_editor_field()?;
        match field.interaction {
            ConfigFieldInteraction::Choice { choices } => Some(choices),
            ConfigFieldInteraction::ReadOnly
            | ConfigFieldInteraction::Text { .. }
            | ConfigFieldInteraction::CredentialReference { .. } => None,
        }
    }

    fn move_config_choice(
        &mut self,
        runtime: &ConfigRuntime,
        offset: isize,
    ) -> Result<(), PresentationControllerError> {
        let Some(choices) = self.config_choices() else {
            return Ok(());
        };
        let current = self.config_choice();
        let next = current
            .and_then(|current| choices.iter().position(|choice| *choice == current))
            .map_or_else(
                || {
                    if offset.is_negative() {
                        choices.len().saturating_sub(1)
                    } else {
                        0
                    }
                },
                |index| {
                    index
                        .saturating_add_signed(offset)
                        .min(choices.len().saturating_sub(1))
                },
            );
        let Some(choice) = choices.get(next) else {
            return Ok(());
        };
        self.controller.stage_config(runtime, choice)
    }

    fn layout(
        &self,
        runtime: Option<&ConfigRuntime>,
        view: &TuiViewModel,
    ) -> Result<PresentationLayoutView, TerminalError> {
        let mut layout = self
            .controller
            .layout(runtime, view, self.viewport)
            .map_err(TerminalError::Presentation)?;
        if let Some(input) = self.config_text.as_ref().filter(|input| input.pending) {
            layout.show_pending_config_text(&input.value);
        }
        if let Some(flow) = self.credential_flow.as_ref() {
            layout.show_credential_flow(match flow {
                CredentialFlow::Actions { selected, .. } => CredentialFlowView::Actions {
                    selected: *selected,
                },
                CredentialFlow::Secret { action, input, .. } => CredentialFlowView::Secret {
                    replacing: *action == CredentialAction::Replace,
                    byte_len: input.len(),
                },
                CredentialFlow::ConfirmReplace { .. } => CredentialFlowView::ConfirmReplace,
                CredentialFlow::ConfirmForget { .. } => CredentialFlowView::ConfirmForget,
            });
        }
        if let Some(flow) = self.agent_flow {
            layout.show_agent_lifecycle_flow(match flow {
                AgentLifecycleFlow::Actions {
                    agent,
                    cancellable,
                    pending_operation,
                    selected,
                } => AgentLifecycleFlowView::Actions {
                    agent,
                    cancellable,
                    pending_operation,
                    selected,
                },
                AgentLifecycleFlow::ConfirmCancel { agent } => {
                    AgentLifecycleFlowView::ConfirmCancel { agent }
                }
                AgentLifecycleFlow::ConfirmAcknowledgement { operation } => {
                    AgentLifecycleFlowView::ConfirmAcknowledgement { operation }
                }
            });
        }
        Ok(layout)
    }

    fn frame(
        &self,
        runtime: Option<&ConfigRuntime>,
        view: &TuiViewModel,
    ) -> Result<TerminalFrame, TerminalError> {
        let layout = self.layout(runtime, view)?;
        TerminalFrame::from_layout_with_notice(&layout, self.notice.as_deref())
    }
}

fn presentation_notice(source: &PresentationControllerError) -> String {
    match source {
        PresentationControllerError::ConfigEditor(ConfigEditorError::Config(source))
        | PresentationControllerError::Config(source) => config_runtime_notice(source),
        PresentationControllerError::UnsavedConfigDraft => {
            "Config draft must be committed or discarded".to_owned()
        }
        PresentationControllerError::NoCommandSelection => "No command is selected".to_owned(),
        PresentationControllerError::NotSlashPanel
        | PresentationControllerError::NotConfigCenter
        | PresentationControllerError::NotConfigObjectCreate
        | PresentationControllerError::NotConfigEditor
        | PresentationControllerError::NotProviderWizard
        | PresentationControllerError::ConfigRuntimeRequired => {
            "Config action is unavailable in this view".to_owned()
        }
        PresentationControllerError::NoConfigObjectSelection => {
            "No Config Objects are available in this section".to_owned()
        }
        PresentationControllerError::ConfigObjectRouteUnavailable => {
            "Config Object editor is not available in the terminal".to_owned()
        }
        PresentationControllerError::ToolApprovalUnavailable => {
            "Tool approval is unavailable".to_owned()
        }
        PresentationControllerError::ProductOutputUnavailable => {
            "Provider output is unavailable".to_owned()
        }
        PresentationControllerError::Command(_) | PresentationControllerError::ConfigEditor(_) => {
            "Config action failed".to_owned()
        }
    }
}

fn config_runtime_notice(source: &ConfigRuntimeError) -> String {
    match source {
        ConfigRuntimeError::InvalidValue { path, .. } => {
            format!("Config validation failed at {path}")
        }
        ConfigRuntimeError::RevisionConflict { .. } => {
            "Config changed; discard and reopen the editor".to_owned()
        }
        ConfigRuntimeError::ReadOnlyScope(_) => "Config scope is read-only".to_owned(),
        ConfigRuntimeError::RepairRequired(_) => "Config repair is required".to_owned(),
        ConfigRuntimeError::Locked(_) => "Config is locked by another writer".to_owned(),
        ConfigRuntimeError::Io(_) => "Config I/O failed".to_owned(),
        ConfigRuntimeError::UnknownObject(_)
        | ConfigRuntimeError::WrongType { .. }
        | ConfigRuntimeError::SecretReadForbidden(_)
        | ConfigRuntimeError::BackupUnavailable { .. }
        | ConfigRuntimeError::UnsupportedSchema { .. }
        | ConfigRuntimeError::Parse { .. }
        | ConfigRuntimeError::SymlinkPath(_)
        | ConfigRuntimeError::NotRegularFile(_) => "Config validation failed".to_owned(),
    }
}

#[cfg(test)]
fn run_terminal_loop<W, M, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    view: &TuiViewModel,
    width: u16,
    height: u16,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    F: FnMut() -> io::Result<Event>,
{
    let tester_vault = PlatformCredentialVault;
    let mut tester = ModelsHttpConnectionTester::new(&tester_vault);
    let mut credential_vault = PlatformCredentialVault;
    let viewport = Viewport::new(width, height)?;
    run_terminal_loop_core(
        writer,
        mode,
        config,
        TerminalSnapshotSource {
            initial: view,
            refresh_ledger: None,
            viewport,
        },
        TerminalLoopServices {
            tester: &mut tester,
            credential_vault: &mut credential_vault,
            product: None,
            discovery_task: None,
        },
        read_event,
    )
}

fn run_terminal_loop_with_discovery_task<W, M, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    view: &TuiViewModel,
    ledger: &Path,
    viewport: Viewport,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    F: FnMut() -> io::Result<Event>,
{
    let tester_vault = PlatformCredentialVault;
    let mut tester = ModelsHttpConnectionTester::new(&tester_vault);
    let mut credential_vault = PlatformCredentialVault;
    let mut product = LedgerTerminalProductActions {
        ledger,
        pending: None,
    };
    let mut discovery_task = OnDemandProviderDiscoveryTask::platform();
    run_terminal_loop_core(
        writer,
        mode,
        config,
        TerminalSnapshotSource {
            initial: view,
            refresh_ledger: Some(ledger),
            viewport,
        },
        TerminalLoopServices {
            tester: &mut tester,
            credential_vault: &mut credential_vault,
            product: Some(&mut product),
            discovery_task: Some(&mut discovery_task),
        },
        read_event,
    )
}

#[cfg(test)]
fn run_terminal_loop_with_discovery_service<W, M, T, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    snapshot: TerminalSnapshotSource<'_>,
    tester: &mut T,
    discovery_task: &mut dyn ProviderDiscoveryTask,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    T: ProviderConnectionTester,
    F: FnMut() -> io::Result<Event>,
{
    let mut credential_vault = PlatformCredentialVault;
    run_terminal_loop_core(
        writer,
        mode,
        config,
        snapshot,
        TerminalLoopServices {
            tester,
            credential_vault: &mut credential_vault,
            product: None,
            discovery_task: Some(discovery_task),
        },
        read_event,
    )
}

#[cfg(test)]
fn run_terminal_loop_with_snapshot_refresh<W, M, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    view: &TuiViewModel,
    ledger: &Path,
    viewport: Viewport,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    F: FnMut() -> io::Result<Event>,
{
    let tester_vault = PlatformCredentialVault;
    let mut tester = ModelsHttpConnectionTester::new(&tester_vault);
    let mut credential_vault = PlatformCredentialVault;
    let mut product = LedgerTerminalProductActions {
        ledger,
        pending: None,
    };
    run_terminal_loop_core(
        writer,
        mode,
        config,
        TerminalSnapshotSource {
            initial: view,
            refresh_ledger: Some(ledger),
            viewport,
        },
        TerminalLoopServices {
            tester: &mut tester,
            credential_vault: &mut credential_vault,
            product: Some(&mut product),
            discovery_task: None,
        },
        read_event,
    )
}

#[cfg(test)]
fn run_terminal_loop_with_connection_tester<W, M, T, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    view: &TuiViewModel,
    viewport: Viewport,
    tester: &mut T,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    T: ProviderConnectionTester,
    F: FnMut() -> io::Result<Event>,
{
    let mut credential_vault = PlatformCredentialVault;
    run_terminal_loop_core(
        writer,
        mode,
        config,
        TerminalSnapshotSource {
            initial: view,
            refresh_ledger: None,
            viewport,
        },
        TerminalLoopServices {
            tester,
            credential_vault: &mut credential_vault,
            product: None,
            discovery_task: None,
        },
        read_event,
    )
}

#[cfg(test)]
fn run_terminal_loop_with_credential_vault<W, M, T, V, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    snapshot: TerminalSnapshotSource<'_>,
    tester: &mut T,
    credential_vault: &mut V,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    T: ProviderConnectionTester,
    V: CredentialVault,
    F: FnMut() -> io::Result<Event>,
{
    run_terminal_loop_core(
        writer,
        mode,
        config,
        snapshot,
        TerminalLoopServices {
            tester,
            credential_vault,
            product: None,
            discovery_task: None,
        },
        read_event,
    )
}

#[cfg(test)]
fn run_terminal_loop_with_product_actions<W, M, P, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    snapshot: TerminalSnapshotSource<'_>,
    product: &mut P,
    read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    P: TerminalProductActions,
    F: FnMut() -> io::Result<Event>,
{
    let tester_vault = PlatformCredentialVault;
    let mut tester = ModelsHttpConnectionTester::new(&tester_vault);
    let mut credential_vault = PlatformCredentialVault;
    run_terminal_loop_core(
        writer,
        mode,
        config,
        snapshot,
        TerminalLoopServices {
            tester: &mut tester,
            credential_vault: &mut credential_vault,
            product: Some(product),
            discovery_task: None,
        },
        read_event,
    )
}

struct TerminalSnapshotSource<'a> {
    initial: &'a TuiViewModel,
    refresh_ledger: Option<&'a Path>,
    viewport: Viewport,
}

struct TerminalLoopServices<'a, T> {
    tester: &'a mut T,
    credential_vault: &'a mut dyn CredentialVault,
    product: Option<&'a mut dyn TerminalProductActions>,
    discovery_task: Option<&'a mut dyn ProviderDiscoveryTask>,
}

#[allow(clippy::too_many_arguments)]
fn handle_terminal_discovery_task_event(
    event: ProviderDiscoveryTaskEvent,
    ledger: Option<&Path>,
    config: &ConfigRuntime,
    discovery_state: &mut Option<ProviderDiscoveryState>,
    session: &mut TerminalSession,
    view: &mut TuiViewModel,
) -> Result<bool, TerminalError> {
    match event {
        ProviderDiscoveryTaskEvent::Started(job) => {
            session.notice = Some(match job.trigger() {
                ProviderDiscoveryTrigger::OnOpen => {
                    "Provider discovery checking current Profile".to_owned()
                }
                ProviderDiscoveryTrigger::Manual => "Provider discovery refreshing".to_owned(),
            });
            Ok(true)
        }
        ProviderDiscoveryTaskEvent::Completed { job, status } => {
            let profile = config
                .reload_candidate()
                .ok()
                .and_then(|candidate| eligible_terminal_discovery_profile(&candidate))
                .filter(|profile| {
                    profile.profile() == job.identity().profile()
                        && profile.template() == job.identity().template()
                        && profile.fingerprint() == job.identity().fingerprint()
                });
            let Some(profile) = profile else {
                session.notice =
                    Some("Provider discovery result discarded after Config change".to_owned());
                return Ok(true);
            };

            match &status {
                crate::provider_connection::ProviderConnectionTestStatus::Succeeded { .. } => {
                    let Some(ledger) = ledger else {
                        session.notice = Some("Provider discovery refresh unavailable".to_owned());
                        return Ok(true);
                    };
                    let now_unix_ms = UsageTimestamp::now()?.unix_millis();
                    match commit_provider_discovery_status(
                        &profile,
                        &terminal_discovery_path(ledger),
                        now_unix_ms,
                        &status,
                    ) {
                        Ok(Some(state)) => {
                            *discovery_state = Some(state);
                            match build_terminal_view(
                                ledger,
                                config,
                                session.controller.slash_query(),
                            ) {
                                Ok(refreshed) => {
                                    session.controller.reconcile_snapshot(&refreshed);
                                    *view = refreshed;
                                    session.notice =
                                        Some("Provider discovery refreshed".to_owned());
                                    Ok(true)
                                }
                                Err(_) => {
                                    session.notice = Some(
                                        "Provider discovery saved; press F6 to refresh the view"
                                            .to_owned(),
                                    );
                                    Ok(true)
                                }
                            }
                        }
                        Ok(None) | Err(_) => {
                            session.notice = Some(
                                "Provider discovery save failed; press F5 to retry".to_owned(),
                            );
                            Ok(true)
                        }
                    }
                }
                crate::provider_connection::ProviderConnectionTestStatus::Failed { .. }
                | crate::provider_connection::ProviderConnectionTestStatus::Untested => {
                    session.notice =
                        Some("Provider discovery failed; press F5 to retry".to_owned());
                    Ok(true)
                }
            }
        }
        ProviderDiscoveryTaskEvent::AlreadyRunning => {
            session.notice = Some("Provider discovery is already running".to_owned());
            Ok(true)
        }
        ProviderDiscoveryTaskEvent::WorkerUnavailable => {
            session.notice = Some("Provider discovery worker unavailable".to_owned());
            Ok(true)
        }
    }
}

fn run_terminal_loop_core<W, M, T, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    snapshot: TerminalSnapshotSource<'_>,
    services: TerminalLoopServices<'_, T>,
    mut read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    T: ProviderConnectionTester,
    F: FnMut() -> io::Result<Event>,
{
    let TerminalLoopServices {
        tester,
        credential_vault,
        mut product,
        mut discovery_task,
    } = services;
    let width = snapshot.viewport.width();
    let height = snapshot.viewport.height();
    let mut surface = TerminalSurface::enter(writer, mode)?;
    let mut renderer = DirectVtRenderer::new(width, height)?;
    let mut session = TerminalSession::new("/", width, height)?;
    let mut view = snapshot.initial.clone();

    let frame = session.frame(Some(config), &view)?;
    surface.write_frame(&renderer.draw(&frame)?)?;

    let mut discovery_state = snapshot.refresh_ledger.and_then(inspect_terminal_discovery);

    loop {
        let event = read_event()?;
        match session.handle_with_view_and_connection_tester(
            map_crossterm_event(event),
            Some(config),
            Some(&view),
            Some(tester),
        )? {
            TerminalLoopOutcome::Quit => {
                if session.controller.is_tool_approval()
                    && let Some(product) = product.as_deref_mut()
                {
                    product.cancel_tool_approval();
                }
                if let Some(task) = discovery_task.as_deref_mut() {
                    task.cancel();
                }
                break;
            }
            TerminalLoopOutcome::Resize(width, height) => {
                surface.write_frame(&renderer.resize(width, height)?)?;
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::RefreshSnapshot => {
                if let Some(ledger) = snapshot.refresh_ledger {
                    match refresh_terminal_view(ledger, config, session.controller.slash_query()) {
                        Ok(refreshed) => {
                            session.controller.reconcile_snapshot(&refreshed.view);
                            *config = refreshed.config;
                            view = refreshed.view;
                            discovery_state = inspect_terminal_discovery(ledger);
                            session.notice = Some("Snapshot refreshed".to_owned());
                        }
                        Err(_) => {
                            session.notice = Some(
                                "Snapshot refresh failed; showing previous snapshot".to_owned(),
                            );
                        }
                    }
                } else {
                    session.notice = Some("Snapshot refresh unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            outcome @ (TerminalLoopOutcome::RefreshProviderDiscovery
            | TerminalLoopOutcome::RefreshProviderDiscoveryOnOpen) => {
                let on_open =
                    matches!(outcome, TerminalLoopOutcome::RefreshProviderDiscoveryOnOpen);
                if let Some(task) = discovery_task.as_deref_mut() {
                    let Some(profile) = eligible_terminal_discovery_profile(config) else {
                        if !on_open {
                            session.notice =
                                Some("Provider discovery refresh unavailable".to_owned());
                        }
                        let frame = session.frame(Some(config), &view)?;
                        surface.write_frame(&renderer.draw(&frame)?)?;
                        continue;
                    };
                    let event = task.request(
                        profile,
                        if on_open {
                            ProviderDiscoveryTrigger::OnOpen
                        } else {
                            ProviderDiscoveryTrigger::Manual
                        },
                    );
                    let started = matches!(event, ProviderDiscoveryTaskEvent::Started(_));
                    let redraw = handle_terminal_discovery_task_event(
                        event,
                        snapshot.refresh_ledger,
                        config,
                        &mut discovery_state,
                        &mut session,
                        &mut view,
                    )?;
                    if redraw {
                        let frame = session.frame(Some(config), &view)?;
                        surface.write_frame(&renderer.draw(&frame)?)?;
                    }
                    if started && let Some(completed) = task.wait() {
                        let redraw = handle_terminal_discovery_task_event(
                            completed,
                            snapshot.refresh_ledger,
                            config,
                            &mut discovery_state,
                            &mut session,
                            &mut view,
                        )?;
                        if redraw {
                            let frame = session.frame(Some(config), &view)?;
                            surface.write_frame(&renderer.draw(&frame)?)?;
                        }
                    }
                    continue;
                }
                if let Some(ledger) = snapshot.refresh_ledger {
                    match refresh_terminal_discovery(ledger, config, tester) {
                        Ok(
                            crate::provider_connection::ProviderConnectionTestStatus::Succeeded {
                                ..
                            },
                        ) => match refresh_terminal_view(
                            ledger,
                            config,
                            session.controller.slash_query(),
                        ) {
                            Ok(refreshed) => {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                                session.notice = Some("Provider discovery refreshed".to_owned());
                            }
                            Err(_) => {
                                session.notice = Some(
                                    "Provider discovery refresh failed; showing previous catalog"
                                        .to_owned(),
                                );
                            }
                        },
                        Ok(
                            crate::provider_connection::ProviderConnectionTestStatus::Failed {
                                ..
                            }
                            | crate::provider_connection::ProviderConnectionTestStatus::Untested,
                        )
                        | Err(_) => {
                            session.notice = Some(
                                "Provider discovery refresh failed; showing previous catalog"
                                    .to_owned(),
                            );
                        }
                    }
                } else if !on_open {
                    session.notice = Some("Provider discovery refresh unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::BeginDiscoveryAcceptance => {
                let acceptance = session
                    .take_discovery_acceptance()
                    .ok_or(TerminalError::ViewModelRequired)?;
                if let Some(ledger) = snapshot.refresh_ledger {
                    match validate_terminal_discovery_acceptance(ledger, config, &acceptance) {
                        Ok(()) => {
                            session
                                .controller
                                .begin_discovered_model_acceptance(acceptance);
                            session.notice = Some("Enter a Preset ID".to_owned());
                        }
                        Err(_) => {
                            session.notice = Some(
                                "Discovery observation is stale; press F5 and retry".to_owned(),
                            );
                        }
                    }
                } else {
                    session.notice = Some("Discovery Preset acceptance unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::ConfirmDiscoveryAcceptance => {
                let acceptance = session
                    .controller
                    .discovered_model_acceptance()
                    .cloned()
                    .ok_or(TerminalError::ViewModelRequired)?;
                let valid = snapshot.refresh_ledger.is_some_and(|ledger| {
                    validate_terminal_discovery_acceptance(ledger, config, &acceptance).is_ok()
                });
                if valid {
                    match session
                        .controller
                        .confirm_discovered_model_dialect(config, ConfigScope::User)
                    {
                        Ok(()) => {
                            session.validated_config_choice = None;
                            session.sync_config_text();
                            session.notice = Some("Discovery Preset draft staged".to_owned());
                        }
                        Err(source) => session.notice = Some(presentation_notice(&source)),
                    }
                } else {
                    session.controller.cancel_discovered_model_acceptance();
                    session.notice =
                        Some("Discovery observation is stale; press F5 and retry".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::ApplyModelSelection => {
                let preset = session
                    .take_model_selection()
                    .ok_or(TerminalError::ViewModelRequired)?;
                if let Some(ledger) = snapshot.refresh_ledger {
                    match stage_terminal_model_selection(ledger, config, &preset) {
                        Ok(()) => {
                            if let Ok(refreshed) = refresh_terminal_view(
                                ledger,
                                config,
                                session.controller.slash_query(),
                            ) {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.notice = Some(format!(
                                "Preset '{}' selected for current Agent next Turn",
                                preset.id
                            ));
                        }
                        Err(TerminalError::ProductDriver(
                            ProductDriverError::CurrentAgentUnavailable,
                        )) => {
                            session.notice = Some(
                                "Current Agent is unavailable; start a Turn before selecting a Preset"
                                    .to_owned(),
                            );
                        }
                        Err(_) => {
                            session.notice =
                                Some("Model selection failed; refresh and retry".to_owned());
                        }
                    }
                } else {
                    session.notice = Some("Model selection unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::CancelAgent(agent) => {
                if let Some(product) = product.as_deref_mut() {
                    match product.cancel_agent(agent) {
                        Ok(operation) => {
                            if let Some(ledger) = snapshot.refresh_ledger {
                                match refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                ) {
                                    Ok(refreshed) => {
                                        session.controller.reconcile_snapshot(&refreshed.view);
                                        *config = refreshed.config;
                                        view = refreshed.view;
                                        session.notice = Some(format!(
                                            "Agent {agent} cancelled; operation {operation} awaits acknowledgement"
                                        ));
                                    }
                                    Err(_) => {
                                        session.notice = Some(format!(
                                            "Agent cancellation committed as operation {operation}; refresh failed"
                                        ));
                                    }
                                }
                            } else {
                                session.notice = Some(format!(
                                    "Agent {agent} cancelled; operation {operation} awaits acknowledgement"
                                ));
                            }
                        }
                        Err(_) => {
                            session.agent_flow = Some(AgentLifecycleFlow::ConfirmCancel { agent });
                            session.notice = Some(
                                "Agent cancellation failed; confirmation remains available"
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    session.agent_flow = Some(AgentLifecycleFlow::ConfirmCancel { agent });
                    session.notice = Some("Agent cancellation unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::AcknowledgeTeamOperation(operation) => {
                if let Some(product) = product.as_deref_mut() {
                    match product.acknowledge_team_operation(operation) {
                        Ok(()) => {
                            if let Some(ledger) = snapshot.refresh_ledger {
                                match refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                ) {
                                    Ok(refreshed) => {
                                        session.controller.reconcile_snapshot(&refreshed.view);
                                        *config = refreshed.config;
                                        view = refreshed.view;
                                        session.notice = Some(format!(
                                            "Team operation {operation} acknowledged"
                                        ));
                                    }
                                    Err(_) => {
                                        session.notice = Some(format!(
                                            "Team operation {operation} acknowledged; refresh failed"
                                        ));
                                    }
                                }
                            } else {
                                session.notice =
                                    Some(format!("Team operation {operation} acknowledged"));
                            }
                        }
                        Err(_) => {
                            session.agent_flow =
                                Some(AgentLifecycleFlow::ConfirmAcknowledgement { operation });
                            session.notice = Some(
                                "Team operation acknowledgement failed; operation remains pending"
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    session.agent_flow =
                        Some(AgentLifecycleFlow::ConfirmAcknowledgement { operation });
                    session.notice = Some("Team operation acknowledgement unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::ResolveCredential => {
                resolve_pending_credential_command(&mut session, config, credential_vault)?;
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::LoadToolApproval(call) => {
                if let Some(product) = product.as_deref_mut() {
                    match product.load_tool_approval(call) {
                        Ok(approval) => {
                            session.controller.show_tool_approval(approval);
                            session.notice = Some("Review the exact Tool request".to_owned());
                        }
                        Err(_) => {
                            product.cancel_tool_approval();
                            if let Some(ledger) = snapshot.refresh_ledger
                                && let Ok(refreshed) = refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                )
                            {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.notice = Some(
                                "Tool approval details are unavailable; inspect current blockers"
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    session.notice = Some("Tool approval unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::ResolveToolApproval => {
                let (call, decision) = session
                    .take_tool_action()
                    .ok_or(TerminalError::ViewModelRequired)?;
                if let Some(product) = product.as_deref_mut() {
                    match product.resolve_tool_approval(call, decision) {
                        Ok(TerminalToolResolution::Prepared { delivery, text }) => {
                            if let Some(ledger) = snapshot.refresh_ledger
                                && let Ok(refreshed) = refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                )
                            {
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.show_product_output(delivery, text);
                        }
                        Ok(TerminalToolResolution::Denied) => {
                            session.controller.finish_product_output();
                            if let Some(ledger) = snapshot.refresh_ledger
                                && let Ok(refreshed) = refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                )
                            {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.notice = Some("Tool call denied; Turn blocked".to_owned());
                        }
                        Err(_) => {
                            product.cancel_tool_approval();
                            session.controller.cancel_tool_approval();
                            if let Some(ledger) = snapshot.refresh_ledger
                                && let Ok(refreshed) = refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                )
                            {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.notice = Some(
                                "Tool approval did not complete; inspect current blockers"
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    session.controller.cancel_tool_approval();
                    session.notice = Some("Tool approval unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::CancelToolApproval => {
                if let Some(product) = product.as_deref_mut() {
                    product.cancel_tool_approval();
                }
                session.controller.cancel_tool_approval();
                session.notice = Some("Tool approval left pending".to_owned());
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::AcknowledgeProductOutput(delivery) => {
                if let Some(product) = product.as_deref_mut() {
                    match product.acknowledge_output(delivery) {
                        Ok(()) => {
                            session.controller.finish_product_output();
                            if let Some(ledger) = snapshot.refresh_ledger
                                && let Ok(refreshed) = refresh_terminal_view(
                                    ledger,
                                    config,
                                    session.controller.slash_query(),
                                )
                            {
                                session.controller.reconcile_snapshot(&refreshed.view);
                                *config = refreshed.config;
                                view = refreshed.view;
                            }
                            session.notice = Some("Provider output acknowledged".to_owned());
                        }
                        Err(_) => {
                            session.notice = Some(
                                "Output acknowledgement failed; delivery remains pending"
                                    .to_owned(),
                            );
                        }
                    }
                } else {
                    session.notice = Some("Output acknowledgement unavailable".to_owned());
                }
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::Redraw => {
                let frame = session.frame(Some(config), &view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::Noop => {}
        }
    }

    Ok(surface.finish()?)
}

fn resolve_pending_credential_command(
    session: &mut TerminalSession,
    config: &ConfigRuntime,
    credential_vault: &mut dyn CredentialVault,
) -> Result<(), TerminalError> {
    let command = session
        .take_credential_command()
        .ok_or(TerminalError::ViewModelRequired)?;
    if !session.validate_credential_command_scope(config, &command) {
        drop(command);
        return Ok(());
    }
    let (action, result) = match command {
        CredentialCommand::Bind { scope, secret } => (
            CredentialAction::Bind,
            credential_vault
                .bind(&scope, secret)
                .map(|()| CredentialCommandOutcome::Completed),
        ),
        CredentialCommand::Replace { scope, secret } => (
            CredentialAction::Replace,
            credential_vault
                .replace(&scope, secret)
                .map(|()| CredentialCommandOutcome::Completed),
        ),
        CredentialCommand::Test { scope } => (
            CredentialAction::Test,
            match credential_vault.resolve(&scope) {
                Ok(secret) => {
                    drop(secret);
                    Ok(CredentialCommandOutcome::Available)
                }
                Err(CredentialVaultError::NotFound) => Ok(CredentialCommandOutcome::Missing),
                Err(source) => Err(source),
            },
        ),
        CredentialCommand::Forget { scope } => (
            CredentialAction::Forget,
            credential_vault.forget(&scope).map(|forgotten| {
                if forgotten {
                    CredentialCommandOutcome::Forgotten
                } else {
                    CredentialCommandOutcome::Missing
                }
            }),
        ),
    };
    session.finish_credential_command(action, result);
    Ok(())
}

#[derive(Debug)]
pub(crate) enum TerminalError {
    NonInteractive,
    InvalidDimensions,
    DimensionMismatch,
    UnsupportedCellWidth,
    InvalidQuery,
    ConfigRuntimeRequired,
    ViewModelRequired,
    ToolApprovalUnavailable,
    PendingProviderEpochRequired,
    Presentation(PresentationControllerError),
    PresentationModel(PresentationError),
    Viewport(ViewportError),
    Config(ConfigError),
    ConfigRuntime(ConfigRuntimeError),
    Runtime(RuntimeError),
    Usage(UsageError),
    ProductDriver(ProductDriverError),
    Provider(ProviderError),
    ProviderDiscovery(ProviderDiscoveryError),
    LocalProcess(LocalProcessError),
    Io(std::io::Error),
}

impl fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonInteractive => formatter.write_str("TUI requires an interactive terminal"),
            Self::InvalidDimensions => formatter.write_str("terminal dimensions must be positive"),
            Self::DimensionMismatch => {
                formatter.write_str("terminal frame dimensions do not match")
            }
            Self::UnsupportedCellWidth => {
                formatter.write_str("terminal grapheme width is unsupported")
            }
            Self::InvalidQuery => formatter.write_str("terminal slash query is invalid"),
            Self::ConfigRuntimeRequired => {
                formatter.write_str("terminal action requires Config Runtime")
            }
            Self::ViewModelRequired => {
                formatter.write_str("terminal action requires its frozen snapshot")
            }
            Self::ToolApprovalUnavailable => {
                formatter.write_str("selected Tool approval is unavailable")
            }
            Self::PendingProviderEpochRequired => {
                formatter.write_str("Tool approval requires a pending Provider Epoch")
            }
            Self::Presentation(source) => write!(formatter, "{source}"),
            Self::PresentationModel(source) => write!(formatter, "{source}"),
            Self::Viewport(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::ConfigRuntime(source) => write!(formatter, "{source}"),
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::Usage(source) => write!(formatter, "{source}"),
            Self::ProductDriver(source) => write!(formatter, "{source}"),
            Self::Provider(source) => write!(formatter, "{source}"),
            Self::ProviderDiscovery(source) => write!(formatter, "{source}"),
            Self::LocalProcess(source) => write!(formatter, "{source}"),
            Self::Io(source) => write!(formatter, "terminal I/O failed: {source}"),
        }
    }
}

impl Error for TerminalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Presentation(source) => Some(source),
            Self::PresentationModel(source) => Some(source),
            Self::Viewport(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::ConfigRuntime(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Usage(source) => Some(source),
            Self::ProductDriver(source) => Some(source),
            Self::Provider(source) => Some(source),
            Self::ProviderDiscovery(source) => Some(source),
            Self::LocalProcess(source) => Some(source),
            Self::InvalidDimensions
            | Self::DimensionMismatch
            | Self::UnsupportedCellWidth
            | Self::InvalidQuery
            | Self::ConfigRuntimeRequired
            | Self::ViewModelRequired
            | Self::ToolApprovalUnavailable
            | Self::PendingProviderEpochRequired
            | Self::NonInteractive => None,
        }
    }
}

impl From<std::io::Error> for TerminalError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<PresentationControllerError> for TerminalError {
    fn from(source: PresentationControllerError) -> Self {
        Self::Presentation(source)
    }
}

impl From<ViewportError> for TerminalError {
    fn from(source: ViewportError) -> Self {
        Self::Viewport(source)
    }
}

impl From<ConfigError> for TerminalError {
    fn from(source: ConfigError) -> Self {
        Self::Config(source)
    }
}

impl From<ConfigRuntimeError> for TerminalError {
    fn from(source: ConfigRuntimeError) -> Self {
        Self::ConfigRuntime(source)
    }
}

impl From<RuntimeError> for TerminalError {
    fn from(source: RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

impl From<UsageError> for TerminalError {
    fn from(source: UsageError) -> Self {
        Self::Usage(source)
    }
}

impl From<ProductDriverError> for TerminalError {
    fn from(source: ProductDriverError) -> Self {
        Self::ProductDriver(source)
    }
}

impl From<ProviderError> for TerminalError {
    fn from(source: ProviderError) -> Self {
        Self::Provider(source)
    }
}

impl From<ProviderDiscoveryError> for TerminalError {
    fn from(source: ProviderDiscoveryError) -> Self {
        Self::ProviderDiscovery(source)
    }
}

impl From<LocalProcessError> for TerminalError {
    fn from(source: LocalProcessError) -> Self {
        Self::LocalProcess(source)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::{self, Write as _};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use greentyper_core::agent_team::{
        Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope, TaskSpec,
        TeamCommand, TeamOperationRecord,
    };
    use greentyper_core::config::{
        ConfigDocument, ConfigEditorSession, ConfigFieldContents, ConfigLayers, ConfigObjectKind,
        ConfigObjectRef, ConfigPaths, ConfigRuntime, ConfigRuntimeError, ConfigScope, ConfigValue,
        ContextMode, MAX_CONFIG_STRING_BYTES, ReasoningEffort, ServiceTier,
    };
    use greentyper_core::pricing::PriceScheduleSource;
    use greentyper_core::provider::{
        ProviderDialect, ProviderError, ProviderEvent, ProviderPricingSource,
        ProviderProfileSnapshot, ProviderRequest, ProviderRuntime, UsageRecord,
    };
    use greentyper_core::provider_discovery::{
        DiscoveredProviderModel, ProviderDiscoveryProfile, ProviderDiscoveryState,
    };
    use greentyper_core::runtime::{ProviderToolApproval, ProviderTurnOutcome, RuntimeKernel};
    use greentyper_core::usage::{UsageWeekday, UsageWindow};

    use crate::credential_vault::{
        CredentialVault, CredentialVaultError, InMemoryCredentialVault, ProviderCredentialScope,
        SecretValue,
    };
    use crate::local_process::LocalProcessExecutor;
    use crate::presentation::{PresentationScreenView, ProductToolApprovalView, build_smoke_view};
    use crate::product_driver::{ProductDriver, ProductInteraction, ProductToolDecision};
    use crate::provider_connection::{
        ObservedProviderModel, ProviderConnectionFailureCategory, ProviderConnectionTestStatus,
        ProviderConnectionTester,
    };
    use crate::provider_discovery_task::OnDemandProviderDiscoveryTask;

    use super::{
        DirectVtRenderer, ENTER_TERMINAL, LEAVE_TERMINAL, TerminalError, TerminalFrame,
        TerminalInputContext, TerminalInputEvent, TerminalInputState, TerminalIntent,
        TerminalLoopOutcome, TerminalMode, TerminalProductActions, TerminalSession,
        TerminalSnapshotSource, TerminalSurface, TerminalToolResolution, Viewport,
        build_terminal_view, inspect_product_team, map_crossterm_event, refresh_terminal_view,
        run_terminal_loop, run_terminal_loop_with_connection_tester,
        run_terminal_loop_with_credential_vault, run_terminal_loop_with_discovery_service,
        run_terminal_loop_with_product_actions, run_terminal_loop_with_snapshot_refresh,
    };

    struct CompleteStatsUsageProvider;

    struct PendingApprovalProvider;

    impl ProviderRuntime for PendingApprovalProvider {
        fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::FunctionCall(greentyper_core::provider::ProviderToolCall::new(
                    "terminal-approval-call",
                    "local.echo",
                    r#"{"message":"private-terminal-approval"}"#,
                )?),
                ProviderEvent::Completed(UsageRecord::default()),
            ])
        }
    }

    struct InterruptToolApproval;

    impl ProductInteraction for InterruptToolApproval {
        fn present_team_operation(&mut self, _record: TeamOperationRecord) -> io::Result<()> {
            Ok(())
        }

        fn decide_tool(
            &mut self,
            _approval: &ProviderToolApproval,
        ) -> io::Result<ProductToolDecision> {
            Err(io::Error::other("approval interrupted"))
        }
    }

    struct RecordingTerminalProductActions {
        outcomes: VecDeque<Result<TerminalToolResolution, TerminalError>>,
        loads: Vec<u64>,
        loaded_call: Option<u64>,
        decisions: Vec<(u64, ProductToolDecision)>,
        cancellations: usize,
        acknowledgements: Vec<u64>,
        acknowledgement_failures: usize,
        agent_cancellations: Vec<u64>,
        team_acknowledgements: Vec<u64>,
    }

    impl RecordingTerminalProductActions {
        fn new(outcomes: impl IntoIterator<Item = TerminalToolResolution>) -> Self {
            Self {
                outcomes: outcomes.into_iter().map(Ok).collect(),
                loads: Vec::new(),
                loaded_call: None,
                decisions: Vec::new(),
                cancellations: 0,
                acknowledgements: Vec::new(),
                acknowledgement_failures: 0,
                agent_cancellations: Vec::new(),
                team_acknowledgements: Vec::new(),
            }
        }

        fn fail_acknowledgement_once(mut self) -> Self {
            self.acknowledgement_failures = 1;
            self
        }

        fn fail_resolution_once(mut self) -> Self {
            self.outcomes
                .push_front(Err(TerminalError::ToolApprovalUnavailable));
            self
        }
    }

    impl TerminalProductActions for RecordingTerminalProductActions {
        fn load_tool_approval(
            &mut self,
            call: u64,
        ) -> Result<ProductToolApprovalView, TerminalError> {
            self.loads.push(call);
            self.loaded_call = Some(call);
            Ok(ProductToolApprovalView {
                call,
                agent: 1,
                tool: "local.echo".to_owned(),
                identity: "terminal-approval-call".to_owned(),
                arguments: r#"{"message":"private-terminal-approval"}"#.to_owned(),
                filesystem_reads: Vec::new(),
                filesystem_writes: Vec::new(),
                process: Some("local.echo".to_owned()),
                network_targets: Vec::new(),
            })
        }

        fn resolve_tool_approval(
            &mut self,
            call: u64,
            decision: ProductToolDecision,
        ) -> Result<TerminalToolResolution, TerminalError> {
            if self.loaded_call.take() != Some(call) {
                return Err(TerminalError::ToolApprovalUnavailable);
            }
            self.decisions.push((call, decision));
            self.outcomes
                .pop_front()
                .unwrap_or(Err(TerminalError::ToolApprovalUnavailable))
        }

        fn cancel_tool_approval(&mut self) {
            self.loaded_call = None;
            self.cancellations = self.cancellations.saturating_add(1);
        }

        fn acknowledge_output(&mut self, delivery: u64) -> Result<(), TerminalError> {
            self.acknowledgements.push(delivery);
            if self.acknowledgement_failures > 0 {
                self.acknowledgement_failures -= 1;
                return Err(TerminalError::Io(io::Error::other(
                    "injected acknowledgement failure",
                )));
            }
            Ok(())
        }

        fn cancel_agent(&mut self, agent: u64) -> Result<u64, TerminalError> {
            self.agent_cancellations.push(agent);
            Ok(91)
        }

        fn acknowledge_team_operation(&mut self, operation: u64) -> Result<(), TerminalError> {
            self.team_acknowledgements.push(operation);
            Ok(())
        }
    }

    impl ProviderRuntime for CompleteStatsUsageProvider {
        fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::TextDelta("usage recorded".to_owned()),
                ProviderEvent::Completed(UsageRecord::new(
                    Some(100),
                    Some(10),
                    Some(5),
                    Some(20),
                    Some(2),
                    Some(120),
                    None,
                )?),
            ])
        }
    }

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn direct_vt_clears_stale_cells_and_skips_identical_frames() {
        let mut renderer = DirectVtRenderer::new(6, 1).expect("renderer");

        let first = TerminalFrame::from_rows(6, 1, &["abcdef"]).expect("first frame");
        assert!(!renderer.draw(&first).expect("first draw").is_empty());

        let shorter = TerminalFrame::from_rows(6, 1, &["ok"]).expect("short frame");
        let bytes = renderer.draw(&shorter).expect("short draw");
        assert_eq!(bytes, b"\x1b[1;1H\x1b[0mok    \x1b[0m");

        assert!(renderer.draw(&shorter).expect("no-op draw").is_empty());
    }

    #[test]
    fn direct_vt_tracks_wide_cell_geometry_when_clearing() {
        let mut renderer = DirectVtRenderer::new(4, 1).expect("renderer");
        let wide = TerminalFrame::from_rows(4, 1, &["双a"]).expect("wide frame");
        renderer.draw(&wide).expect("wide draw");

        let shorter = TerminalFrame::from_rows(4, 1, &["x"]).expect("short frame");
        assert_eq!(
            renderer.draw(&shorter).expect("clear wide frame"),
            b"\x1b[1;1H\x1b[0mx  \x1b[0m"
        );
    }

    #[test]
    fn terminal_input_maps_hierarchical_navigation_without_polling() {
        let mut input = TerminalInputState::new("/").expect("input state");

        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('c'),
                TerminalInputContext::SlashPanel,
            ),
            TerminalIntent::SetSlashQuery("/c".to_owned())
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Backspace,
                TerminalInputContext::SlashPanel,
            ),
            TerminalIntent::SetSlashQuery("/".to_owned())
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Up, TerminalInputContext::SlashPanel),
            TerminalIntent::MoveSelection(-1)
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Down, TerminalInputContext::SlashPanel),
            TerminalIntent::MoveSelection(1)
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Enter, TerminalInputContext::SlashPanel),
            TerminalIntent::Activate
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Escape, TerminalInputContext::SlashPanel),
            TerminalIntent::Quit
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Escape, TerminalInputContext::Other),
            TerminalIntent::Back
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('m'),
                TerminalInputContext::ModelSelector,
            ),
            TerminalIntent::EditModelQuery('m')
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Tab, TerminalInputContext::ModelSelector),
            TerminalIntent::MoveModelGroup(1)
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Down,
                TerminalInputContext::ModelSelector
            ),
            TerminalIntent::MoveModelSelection(1)
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Enter,
                TerminalInputContext::ModelSelector,
            ),
            TerminalIntent::ToggleModelDetail
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Down, TerminalInputContext::Stats),
            TerminalIntent::MoveStatsSelection(1)
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Enter, TerminalInputContext::Stats),
            TerminalIntent::ToggleStatsDetail
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Down, TerminalInputContext::AgentCenter,),
            TerminalIntent::MoveAgentSelection(1)
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Enter, TerminalInputContext::AgentCenter,),
            TerminalIntent::ToggleAgentDetail
        );
        for context in [
            TerminalInputContext::SlashPanel,
            TerminalInputContext::ModelSelector,
            TerminalInputContext::Stats,
            TerminalInputContext::AgentCenter,
        ] {
            assert_eq!(
                input.apply(TerminalInputEvent::RefreshSnapshot, context),
                TerminalIntent::RefreshSnapshot
            );
        }
        assert_eq!(
            input.apply(
                TerminalInputEvent::RefreshSnapshot,
                TerminalInputContext::ConfigText,
            ),
            TerminalIntent::None
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::CredentialActions,
                TerminalInputContext::ConfigCredentialReference,
            ),
            TerminalIntent::OpenCredentialActions
        );
        for context in [
            TerminalInputContext::CredentialActions,
            TerminalInputContext::CredentialSecret,
            TerminalInputContext::CredentialConfirmation,
        ] {
            assert_eq!(
                input.apply(TerminalInputEvent::TestProviderConnection, context),
                TerminalIntent::None
            );
        }
        assert_eq!(
            input.apply(TerminalInputEvent::Down, TerminalInputContext::ConfigObject),
            TerminalIntent::MoveConfigObjectSelection(1)
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Enter,
                TerminalInputContext::ConfigObject,
            ),
            TerminalIntent::ActivateConfigObject
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Resize(80, 24),
                TerminalInputContext::Other
            ),
            TerminalIntent::Resize(80, 24)
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Down, TerminalInputContext::ConfigChoice),
            TerminalIntent::MoveConfigChoice(1)
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Enter,
                TerminalInputContext::ConfigChoice
            ),
            TerminalIntent::PreviewConfig
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('c'),
                TerminalInputContext::ConfigChoice,
            ),
            TerminalIntent::CommitConfig
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('d'),
                TerminalInputContext::ConfigChoice,
            ),
            TerminalIntent::DiscardConfig
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('h'),
                TerminalInputContext::ConfigText,
            ),
            TerminalIntent::EditConfigText('h')
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Backspace,
                TerminalInputContext::ConfigText,
            ),
            TerminalIntent::BackspaceConfigText
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Delete, TerminalInputContext::ConfigText),
            TerminalIntent::ClearConfigText
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Enter, TerminalInputContext::ConfigText),
            TerminalIntent::SubmitConfigText
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Escape,
                TerminalInputContext::DiscardConfirmation,
            ),
            TerminalIntent::CancelDiscard
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Enter,
                TerminalInputContext::DiscardConfirmation,
            ),
            TerminalIntent::DiscardConfig
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Enter,
                TerminalInputContext::ConfigDeleteConfirmation,
            ),
            TerminalIntent::ConfirmConfigDelete
        );
        assert_eq!(
            input.apply(
                TerminalInputEvent::Escape,
                TerminalInputContext::ConfigDeleteConfirmation,
            ),
            TerminalIntent::DiscardConfig
        );
        assert_eq!(
            input.apply(TerminalInputEvent::Enter, TerminalInputContext::Other),
            TerminalIntent::None
        );
    }

    #[test]
    fn terminal_dimensions_and_text_inputs_are_bounded_before_growth() {
        assert!(TerminalFrame::blank(512, 256).is_ok());
        assert!(TerminalFrame::blank(513, 1).is_err());
        assert!(TerminalFrame::blank(512, 257).is_err());

        let oversized_query = format!("/{}", "a".repeat(256));
        assert!(TerminalInputState::new(&oversized_query).is_err());

        let full_query = format!("/{}", "a".repeat(255));
        let mut input = TerminalInputState::new(&full_query).expect("bounded query");
        assert_eq!(
            input.apply(
                TerminalInputEvent::Character('b'),
                TerminalInputContext::SlashPanel,
            ),
            TerminalIntent::None
        );

        let root = terminal_test_root("bounded-provider-url");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session = TerminalSession::new("/config provider url", 80, 24).expect("session");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider URL editor");
        let bounded = format!(
            "https://{}",
            "a".repeat(MAX_CONFIG_STRING_BYTES - "https://".len())
        );
        for character in bounded.chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage bounded URL");
        }
        session
            .handle(TerminalInputEvent::Character('b'), Some(&mut config))
            .expect("reject oversized URL input");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config value exceeds its input limit")
        );
        let PresentationScreenView::ProviderWizard { editor, .. } = session
            .controller
            .screen(Some(&config))
            .expect("bounded provider editor screen")
        else {
            panic!("expected provider editor")
        };
        let ConfigFieldContents::Value {
            target: Some(ConfigValue::String(target)),
            ..
        } = editor.field.contents
        else {
            panic!("expected bounded string target")
        };
        assert_eq!(target.len(), MAX_CONFIG_STRING_BYTES);
        assert_eq!(
            config_string_target(&config, "providers.edge.base_url").as_deref(),
            Some("https://gateway.example.com/v1")
        );
        drop(session);
        drop(config);
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn presentation_layout_maps_to_styled_terminal_cells() {
        let smoke = build_smoke_view("/config").expect("smoke view");
        let layout = &smoke.layouts()[0];
        let frame = TerminalFrame::from_layout(layout).expect("terminal frame");
        let mut renderer = DirectVtRenderer::new(40, 12).expect("renderer");
        let output = String::from_utf8(renderer.draw(&frame).expect("draw")).expect("UTF-8");

        assert!(output.contains("\x1b[1;1H\x1b[0;1;38;5;6mCommands"));
        assert!(output.contains("\x1b[2;1H\x1b[0;38;5;2m> /config"));
        assert!(output.contains("\x1b[12;1H\x1b[0;2;38;5;8mready"));
    }

    #[test]
    fn direct_vt_resize_clears_and_rebuilds_the_frame() {
        let mut renderer = DirectVtRenderer::new(4, 1).expect("renderer");
        let original = TerminalFrame::from_rows(4, 1, &["old"]).expect("original frame");
        renderer.draw(&original).expect("original draw");

        assert_eq!(renderer.resize(2, 2).expect("resize"), b"\x1b[2J\x1b[H");
        let resized = TerminalFrame::from_rows(2, 2, &["n", "v"]).expect("resized frame");
        assert!(!renderer.draw(&resized).expect("redraw").is_empty());
        assert!(renderer.draw(&resized).expect("no-op redraw").is_empty());
    }

    #[test]
    fn terminal_session_drives_the_presentation_controller() {
        let smoke = build_smoke_view("/").expect("smoke view");
        let mut session = TerminalSession::new("/", 40, 12).expect("session");

        assert_eq!(
            session
                .handle(TerminalInputEvent::Character('c'), None)
                .expect("query"),
            TerminalLoopOutcome::Redraw
        );
        let frame = TerminalFrame::from_layout(
            &session.layout(None, smoke.view()).expect("updated layout"),
        )
        .expect("updated frame");
        let mut renderer = DirectVtRenderer::new(40, 12).expect("renderer");
        let output = String::from_utf8(renderer.draw(&frame).expect("draw")).expect("UTF-8");
        assert!(output.contains("> /config"));

        assert_eq!(
            session
                .handle(TerminalInputEvent::Resize(80, 24), None)
                .expect("resize"),
            TerminalLoopOutcome::Resize(80, 24)
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Escape, None)
                .expect("quit"),
            TerminalLoopOutcome::Quit
        );
    }

    #[test]
    fn terminal_loop_browses_models_from_real_key_events_without_mutating_state() {
        let root = terminal_test_root("model-browser");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create model browser fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-model-browser-credential-reference"

[model_presets.fast]
provider = "edge"
model = "gpt-5.4-mini"
dialect = "responses"
favorite = true

[model_presets.careful]
provider = "edge"
model = "gpt-5.4"
dialect = "responses"
"#,
        )
        .expect("write model browser config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let before = std::fs::read(paths.user()).expect("read model browser config");
        let view = build_terminal_view(&ledger, &config, "/").expect("model browser view");
        let mode = FakeTerminalMode::default();
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(Vec::new(), mode, &mut config, &view, 80, 24, move || {
            Ok(events.pop_front().expect("bounded model browser events"))
        })
        .expect("model browser loop");
        let output = String::from_utf8(output).expect("model browser VT output");

        assert!(output.contains("Models / Favorites"));
        assert!(output.contains("query"));
        assert!(output.contains("fast"));
        assert!(output.contains("source"));
        assert!(output.contains("configured"));
        assert!(output.contains("gpt-5.4-mini"));
        assert!(!output.contains("synthetic-model-browser-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread model browser config"),
            before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove model browser fixture");
    }

    #[test]
    fn terminal_model_browser_merges_current_discovery_without_writing_state() {
        let root = terminal_test_root("model-discovery-browser");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create discovery browser fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-discovery-browser-credential-reference"

[providers.edge.catalog]
mode = "template_and_discovery"
"#,
        )
        .expect("write discovery browser config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve discovery Profile")
            .expect("external discovery Profile");
        ProviderDiscoveryState::replace_profile(
            &discovery,
            ProviderDiscoveryProfile::new(
                profile.profile(),
                profile.template(),
                profile.fingerprint(),
                1_786_451_200_000,
                vec![DiscoveredProviderModel::new("gpt-5.6-live", None).expect("discovered model")],
            )
            .expect("discovery observation"),
        )
        .expect("persist discovery observation");
        let config_before = std::fs::read(paths.user()).expect("read discovery browser config");
        let discovery_before =
            std::fs::read(&discovery).expect("read discovery browser observation");
        let view = build_terminal_view(&ledger, &config, "/").expect("discovery browser view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            100,
            24,
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded discovery browser events"))
            },
        )
        .expect("discovery browser loop");
        let output = String::from_utf8(output).expect("discovery browser VT output");

        assert!(output.contains("gpt-5.6-live"), "{output}");
        assert!(output.contains("source discovery"), "{output}");
        assert!(output.contains("freshness"), "{output}");
        assert!(output.contains("current"), "{output}");
        assert!(output.contains("availability"), "{output}");
        assert!(output.contains("yes"), "{output}");
        assert!(!output.contains("synthetic-discovery-browser-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread discovery browser config"),
            config_before
        );
        assert_eq!(
            std::fs::read(&discovery).expect("reread discovery browser observation"),
            discovery_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove discovery browser fixture");
    }

    #[test]
    fn terminal_model_browser_accepts_discovery_with_an_explicit_dialect() {
        let root = terminal_test_root("model-discovery-accept");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create discovery acceptance fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-live"

[providers.edge]
template = "openai-compatible"
credential = "synthetic-discovery-acceptance-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses", "chat_completions"]

[providers.edge.routes]
responses = "/responses"
chat_completions = "/chat/completions"
models = "/models"

[providers.edge.catalog]
mode = "discovery"

[providers.edge.pricing]
source = "unknown"
"#,
        )
        .expect("write discovery acceptance config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve discovery acceptance Profile")
            .expect("external discovery acceptance Profile");
        ProviderDiscoveryState::replace_profile(
            &discovery,
            ProviderDiscoveryProfile::new(
                profile.profile(),
                profile.template(),
                profile.fingerprint(),
                1_786_451_200_000,
                vec![
                    DiscoveredProviderModel::new("gpt-5.6-live", None)
                        .expect("discovered acceptance model"),
                ],
            )
            .expect("discovery acceptance observation"),
        )
        .expect("persist discovery acceptance observation");
        let discovery_before =
            std::fs::read(&discovery).expect("read discovery acceptance observation");
        let view = build_terminal_view(&ledger, &config, "/").expect("discovery acceptance view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend("accepted-live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(100, 24).expect("discovery acceptance viewport"),
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded discovery acceptance events"))
            },
        )
        .expect("discovery acceptance loop");
        let output = String::from_utf8(output).expect("discovery acceptance VT output");

        let reopened = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
            .expect("reopen accepted discovery Config");
        let preset = reopened
            .model_preset("accepted-live")
            .unwrap_or_else(|source| panic!("accepted discovery Preset: {source}\n{output}"));
        assert_eq!(preset.provider, "edge");
        assert_eq!(preset.model, "gpt-5.6-live");
        assert_eq!(preset.dialect, ProviderDialect::ChatCompletions);
        assert!(output.contains("trusted dialect"), "{output}");
        assert!(output.contains("chat_completions"), "{output}");
        assert!(!output.contains("synthetic-discovery-acceptance-reference"));
        assert_eq!(
            std::fs::read(&discovery).expect("reread discovery acceptance observation"),
            discovery_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove discovery acceptance fixture");
    }

    #[test]
    fn terminal_model_browser_revalidates_discovery_before_creating_a_preset_draft() {
        let root = terminal_test_root("model-discovery-stale-accept");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create stale acceptance fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-live"

[providers.edge]
template = "openai-compatible"
credential = "synthetic-stale-acceptance-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.catalog]
mode = "discovery"

[providers.edge.pricing]
source = "unknown"
"#,
        )
        .expect("write stale acceptance config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve stale acceptance Profile")
            .expect("external stale acceptance Profile");
        ProviderDiscoveryState::replace_profile(
            &discovery,
            ProviderDiscoveryProfile::new(
                profile.profile(),
                profile.template(),
                profile.fingerprint(),
                1_786_451_200_000,
                vec![
                    DiscoveredProviderModel::new("gpt-5.6-live", None)
                        .expect("stale acceptance model"),
                ],
            )
            .expect("initial stale acceptance observation"),
        )
        .expect("persist initial stale acceptance observation");
        let view = build_terminal_view(&ledger, &config, "/").expect("stale acceptance view");
        let config_before = std::fs::read(paths.user()).expect("read stale acceptance Config");
        let external_discovery_bytes = Rc::new(RefCell::new(None));
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend("accepted-live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let discovery_for_change = discovery.clone();
        let profile_id = profile.profile().to_owned();
        let template = profile.template().to_owned();
        let fingerprint = profile.fingerprint();
        let external_discovery_bytes_for_event = Rc::clone(&external_discovery_bytes);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(100, 24).expect("stale acceptance viewport"),
            move || {
                let event = events.pop_front().expect("bounded stale acceptance events");
                if matches!(event, Event::Key(key) if key.code == KeyCode::Enter)
                    && events.len() == 4
                {
                    ProviderDiscoveryState::replace_profile(
                        &discovery_for_change,
                        ProviderDiscoveryProfile::new(
                            &profile_id,
                            &template,
                            fingerprint,
                            1_786_451_201_000,
                            vec![
                                DiscoveredProviderModel::new("gpt-5.6-live", None)
                                    .expect("replacement stale acceptance model"),
                            ],
                        )
                        .expect("replacement stale acceptance observation"),
                    )
                    .expect("replace stale acceptance observation externally");
                    *external_discovery_bytes_for_event.borrow_mut() = Some(
                        std::fs::read(&discovery_for_change)
                            .expect("read external stale observation"),
                    );
                }
                Ok(event)
            },
        )
        .expect("stale acceptance loop");
        let output = String::from_utf8(output).expect("stale acceptance VT output");

        assert!(output.contains("press F5 and retry"), "{output}");
        assert!(matches!(
            config.model_preset("accepted-live"),
            Err(ConfigRuntimeError::UnknownObject(_))
        ));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread stale acceptance Config"),
            config_before
        );
        assert_eq!(
            std::fs::read(&discovery).expect("reread external stale observation"),
            external_discovery_bytes
                .borrow()
                .clone()
                .expect("external stale observation captured")
        );
        assert!(!output.contains("synthetic-stale-acceptance-reference"));
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove stale acceptance fixture");
    }

    #[test]
    fn terminal_model_browser_keeps_local_models_when_discovery_is_corrupt() {
        let root = terminal_test_root("model-discovery-corrupt");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create corrupt discovery fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-corrupt-discovery-reference"

[providers.edge.catalog]
mode = "template_and_discovery"
"#,
        )
        .expect("write corrupt discovery config");
        std::fs::write(&discovery, b"not-json").expect("write corrupt discovery state");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let config_before = std::fs::read(paths.user()).expect("read corrupt discovery config");
        let discovery_before = std::fs::read(&discovery).expect("read corrupt discovery state");
        let view = build_terminal_view(&ledger, &config, "/").expect("local fallback view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            100,
            24,
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded corrupt discovery events"))
            },
        )
        .expect("corrupt discovery fallback loop");
        let output = String::from_utf8(output).expect("corrupt discovery VT output");

        assert!(output.contains("Discovery"), "{output}");
        assert!(output.contains("unavailable"), "{output}");
        assert!(output.contains("gpt-5.6-sol"), "{output}");
        assert!(!output.contains("synthetic-corrupt-discovery-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread corrupt discovery config"),
            config_before
        );
        assert_eq!(
            std::fs::read(&discovery).expect("reread corrupt discovery state"),
            discovery_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove corrupt discovery fixture");
    }

    #[test]
    fn terminal_model_browser_discovers_once_on_open_without_background_polling() {
        let root = terminal_test_root("model-discovery-on-open");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create on-open discovery fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-sol"

[providers.edge]
template = "openai"
credential = "synthetic-on-open-discovery-reference"

[providers.edge.catalog]
mode = "template_and_discovery"
"#,
        )
        .expect("write on-open discovery config");
        let config_before = std::fs::read(paths.user()).expect("read on-open discovery config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("on-open discovery view");
        let mut tester = ScriptedDiscoveryTester {
            results: [DiscoveryTestResult::Success("gpt-5.6-live-on-open")].into(),
            calls: Vec::new(),
        };
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("live".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut credential_vault = InMemoryCredentialVault::default();

        let output = super::run_terminal_loop_core(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(100, 24).expect("on-open discovery viewport"),
            },
            super::TerminalLoopServices {
                tester: &mut tester,
                credential_vault: &mut credential_vault,
                product: None,
                discovery_task: None,
            },
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded on-open discovery events"))
            },
        )
        .expect("on-open discovery loop");
        let output = String::from_utf8(output).expect("on-open discovery VT output");

        assert_eq!(tester.calls.len(), 1);
        assert_eq!(tester.calls[0].0, "edge");
        assert!(output.contains("Provider discovery refreshed"), "{output}");
        assert!(output.contains("gpt-5.6-live-on-open"), "{output}");
        assert!(!output.contains("synthetic-on-open-discovery-reference"));
        let final_state =
            ProviderDiscoveryState::inspect(&discovery).expect("inspect on-open discovery state");
        assert_eq!(final_state.profiles().len(), 1);
        assert_eq!(
            final_state.profiles()[0].models()[0].id(),
            "gpt-5.6-live-on-open"
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("reread on-open discovery config"),
            config_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove on-open discovery fixture");
    }

    #[test]
    fn terminal_model_browser_skips_on_open_discovery_without_an_eligible_profile() {
        let cases = [
            (
                "catalog-disabled",
                r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-sol"

[providers.edge]
template = "openai"
credential = "synthetic-disabled-discovery-reference"

[providers.edge.catalog]
mode = "template"
"#,
                "gpt-5.6-sol",
                "synthetic-disabled-discovery-reference",
            ),
            (
                "models-route-missing",
                r#"schema_version = 1

[provider]
profile = "edge"
model = "local-model"

[providers.edge]
template = "openai-compatible"
credential = "synthetic-missing-route-reference"
base_url = "https://gateway.example.com/v1"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"

[providers.edge.catalog]
mode = "template_and_discovery"

[providers.edge.pricing]
source = "unknown"

[model_presets.local]
provider = "edge"
model = "local-model"
dialect = "responses"
"#,
                "local-model",
                "synthetic-missing-route-reference",
            ),
        ];

        for (case, config_text, expected_model, credential_reference) in cases {
            let root = terminal_test_root(&format!("model-discovery-on-open-{case}"));
            let ledger = root.join("runtime.ledger");
            let discovery = ledger.with_file_name("provider-discovery.json");
            let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
            std::fs::create_dir_all(&root).expect("create ineligible discovery fixture directory");
            std::fs::write(paths.user(), config_text).expect("write ineligible discovery config");
            let config_before =
                std::fs::read(paths.user()).expect("read ineligible discovery config");
            let mut config = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
                .expect("Config Runtime");
            let view =
                build_terminal_view(&ledger, &config, "/").expect("ineligible discovery view");
            let mut tester = ScriptedDiscoveryTester {
                results: VecDeque::new(),
                calls: Vec::new(),
            };
            let mut events = "model"
                .chars()
                .map(|character| {
                    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                })
                .collect::<VecDeque<_>>();
            events.extend([
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Resize(80, 24),
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ]);
            let mut credential_vault = InMemoryCredentialVault::default();

            let output = super::run_terminal_loop_core(
                Vec::new(),
                FakeTerminalMode::default(),
                &mut config,
                TerminalSnapshotSource {
                    initial: &view,
                    refresh_ledger: Some(&ledger),
                    viewport: Viewport::new(100, 24).expect("ineligible discovery viewport"),
                },
                super::TerminalLoopServices {
                    tester: &mut tester,
                    credential_vault: &mut credential_vault,
                    product: None,
                    discovery_task: None,
                },
                move || {
                    Ok(events
                        .pop_front()
                        .expect("bounded ineligible discovery events"))
                },
            )
            .expect("ineligible discovery loop");
            let output = String::from_utf8(output).expect("ineligible discovery VT output");

            assert!(tester.calls.is_empty(), "{case}: {output}");
            assert!(output.contains(expected_model), "{case}: {output}");
            assert!(!output.contains(credential_reference), "{case}: {output}");
            assert!(!discovery.exists(), "{case}: discovery state created");
            assert_eq!(
                std::fs::read(paths.user()).expect("reread ineligible discovery config"),
                config_before,
                "{case}"
            );
            assert!(!ledger.exists(), "{case}");
            std::fs::remove_dir_all(root).expect("remove ineligible discovery fixture");
        }
    }

    #[test]
    fn terminal_model_discovery_refresh_preserves_failure_and_retries() {
        let root = terminal_test_root("model-discovery-refresh");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create discovery refresh fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-sol"

[providers.edge]
template = "openai"
credential = "synthetic-discovery-refresh-reference"

[providers.edge.catalog]
mode = "template_and_discovery"
"#,
        )
        .expect("write discovery refresh config");
        let config_before = std::fs::read(paths.user()).expect("read discovery refresh config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve discovery refresh Profile")
            .expect("external discovery refresh Profile");
        ProviderDiscoveryState::replace_profile(
            &discovery,
            ProviderDiscoveryProfile::new(
                profile.profile(),
                profile.template(),
                profile.fingerprint(),
                1_786_451_200_000,
                vec![
                    DiscoveredProviderModel::new("gpt-5.6-live-one", None)
                        .expect("initial discovery refresh model"),
                ],
            )
            .expect("initial discovery refresh observation"),
        )
        .expect("persist initial discovery refresh observation");
        let discovery_before =
            std::fs::read(&discovery).expect("read initial discovery refresh observation");
        let view = build_terminal_view(&ledger, &config, "/").expect("discovery refresh view");
        let mut tester = ScriptedDiscoveryTester {
            results: [
                DiscoveryTestResult::Failure,
                DiscoveryTestResult::Success("gpt-5.6-live-two"),
            ]
            .into(),
            calls: Vec::new(),
        };
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let discovery_for_events = discovery.clone();
        let discovery_before_failure = discovery_before.clone();
        let mut credential_vault = InMemoryCredentialVault::default();
        let output = super::run_terminal_loop_core(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(100, 24).expect("discovery refresh viewport"),
            },
            super::TerminalLoopServices {
                tester: &mut tester,
                credential_vault: &mut credential_vault,
                product: None,
                discovery_task: None,
            },
            move || {
                let event = events
                    .pop_front()
                    .expect("bounded discovery refresh events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::Esc) {
                    assert_eq!(
                        std::fs::read(&discovery_for_events)
                            .expect("discovery observation after failed on-open refresh"),
                        discovery_before_failure
                    );
                }
                Ok(event)
            },
        )
        .expect("discovery refresh loop");
        let output = String::from_utf8(output).expect("discovery refresh VT output");

        assert_eq!(tester.calls.len(), 2);
        assert!(tester.calls.iter().all(|(profile, _)| profile == "edge"));
        assert!(output.contains("Provider discovery refreshed"), "{output}");
        assert!(
            output.contains("failed; showing previous catalog"),
            "{output}"
        );
        assert!(output.contains("gpt-5.6-live-one"), "{output}");
        assert!(output.contains("gpt-5.6-live-two"), "{output}");
        assert!(!output.contains("synthetic-discovery-refresh-reference"));
        let final_state =
            ProviderDiscoveryState::inspect(&discovery).expect("inspect final discovery state");
        assert_eq!(final_state.profiles().len(), 1);
        assert_eq!(
            final_state.profiles()[0].models()[0].id(),
            "gpt-5.6-live-two"
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("reread discovery refresh config"),
            config_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove discovery refresh fixture");
    }

    #[test]
    fn terminal_model_discovery_runs_once_and_atomically_refreshes_the_view() {
        let root = terminal_test_root("model-discovery-on-demand");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create on-demand discovery fixture");
        write_on_demand_discovery_config(
            paths.user(),
            "synthetic-on-demand-discovery-reference",
            "https://provider.invalid/v1",
        );
        let config_before = std::fs::read(paths.user()).expect("read on-demand config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("on-demand discovery view");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let results = Arc::new(Mutex::new(
            [DiscoveryTestResult::Success("gpt-5.6-on-demand")].into(),
        ));
        let probe_calls = Arc::clone(&calls);
        let probe_results = Arc::clone(&results);
        let mut discovery_task = OnDemandProviderDiscoveryTask::testing(move |profile| {
            let mut tester = SharedScriptedDiscoveryTester {
                results: Arc::clone(&probe_results),
                calls: Arc::clone(&probe_calls),
            };
            tester.test(&profile)
        });
        let mut foreground = RecordingConnectionTester { calls: Vec::new() };
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));

        let output = run_terminal_loop_with_discovery_service(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(100, 24).expect("on-demand discovery viewport"),
            },
            &mut foreground,
            &mut discovery_task,
            move || Ok(events.pop_front().expect("bounded on-demand events")),
        )
        .expect("on-demand discovery terminal loop");
        let output = String::from_utf8(output).expect("on-demand discovery VT output");

        assert_eq!(calls.lock().expect("read calls").len(), 1);
        assert!(foreground.calls.is_empty());
        assert!(output.contains("Provider discovery checking"), "{output}");
        assert!(output.contains("refreshed"), "{output}");
        assert!(output.contains("gpt-5.6-on-demand"), "{output}");
        assert!(!output.contains("synthetic-on-demand-discovery-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread on-demand config"),
            config_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove on-demand discovery fixture");
    }

    #[test]
    fn terminal_model_discovery_discards_result_after_profile_change() {
        let root = terminal_test_root("model-discovery-stale-task");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create stale task fixture");
        write_on_demand_discovery_config(
            paths.user(),
            "synthetic-stale-task-reference",
            "https://provider.invalid/v1",
        );
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("stale task view");
        let user = paths.user().to_path_buf();
        let calls = Arc::new(AtomicU64::new(0));
        let probe_calls = Arc::clone(&calls);
        let mut discovery_task = OnDemandProviderDiscoveryTask::testing(move |profile| {
            probe_calls.fetch_add(1, Ordering::Relaxed);
            write_on_demand_discovery_config(
                &user,
                "synthetic-stale-task-reference",
                "https://changed.invalid/v1",
            );
            ProviderConnectionTestStatus::Succeeded {
                profile: profile.profile().to_owned(),
                fingerprint: profile.fingerprint(),
                models: vec![ObservedProviderModel {
                    id: "gpt-5.6-stale-result".to_owned(),
                    release_catalog_key: None,
                }],
            }
        });
        let mut foreground = RecordingConnectionTester { calls: Vec::new() };
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_discovery_service(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(100, 24).expect("stale task viewport"),
            },
            &mut foreground,
            &mut discovery_task,
            move || Ok(events.pop_front().expect("bounded stale task events")),
        )
        .expect("stale task terminal loop");
        let output = String::from_utf8(output).expect("stale task VT output");

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(foreground.calls.is_empty());
        assert!(output.contains("result disca"), "{output}");
        assert!(output.contains("after Config change"), "{output}");
        assert!(!output.contains("gpt-5.6-stale-result"));
        assert!(!output.contains("synthetic-stale-task-reference"));
        assert!(!discovery.exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove stale task fixture");
    }

    #[test]
    fn terminal_model_discovery_failure_waits_for_f5_then_recovers() {
        let root = terminal_test_root("model-discovery-explicit-retry");
        let ledger = root.join("runtime.ledger");
        let discovery = ledger.with_file_name("provider-discovery.json");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create explicit retry fixture");
        write_on_demand_discovery_config(
            paths.user(),
            "synthetic-explicit-retry-reference",
            "https://provider.invalid/v1",
        );
        let config_before = std::fs::read(paths.user()).expect("read explicit retry config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve retry Profile")
            .expect("retry Profile");
        ProviderDiscoveryState::replace_profile(
            &discovery,
            ProviderDiscoveryProfile::new(
                profile.profile(),
                profile.template(),
                profile.fingerprint(),
                1_786_451_200_000,
                vec![
                    DiscoveredProviderModel::new("gpt-5.6-last-good", None)
                        .expect("last good model"),
                ],
            )
            .expect("last good observation"),
        )
        .expect("persist last good observation");
        let discovery_before = std::fs::read(&discovery).expect("read last good observation");
        let view = build_terminal_view(&ledger, &config, "/").expect("explicit retry view");
        let calls = Arc::new(AtomicU64::new(0));
        let results = Arc::new(Mutex::new(
            [
                DiscoveryTestResult::Failure,
                DiscoveryTestResult::Success("gpt-5.6-recovered"),
            ]
            .into(),
        ));
        let probe_calls = Arc::clone(&calls);
        let probe_results = Arc::clone(&results);
        let mut discovery_task = OnDemandProviderDiscoveryTask::testing(move |profile| {
            probe_calls.fetch_add(1, Ordering::Relaxed);
            let mut tester = SharedScriptedDiscoveryTester {
                results: Arc::clone(&probe_results),
                calls: Arc::new(Mutex::new(Vec::new())),
            };
            tester.test(&profile)
        });
        let mut foreground = RecordingConnectionTester { calls: Vec::new() };
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let calls_before_f5 = Arc::clone(&calls);
        let discovery_before_f5 = discovery_before.clone();
        let discovery_during_events = discovery.clone();

        let output = run_terminal_loop_with_discovery_service(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(100, 24).expect("explicit retry viewport"),
            },
            &mut foreground,
            &mut discovery_task,
            move || {
                let event = events.pop_front().expect("bounded explicit retry events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::F(5)) {
                    assert_eq!(calls_before_f5.load(Ordering::Relaxed), 1);
                    assert_eq!(
                        std::fs::read(&discovery_during_events)
                            .expect("observation before explicit retry"),
                        discovery_before_f5
                    );
                }
                Ok(event)
            },
        )
        .expect("explicit retry terminal loop");
        let output = String::from_utf8(output).expect("explicit retry VT output");

        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(foreground.calls.is_empty());
        assert!(output.contains("failed; press F5 to retry"), "{output}");
        assert!(output.contains("gpt-5.6-last-good"), "{output}");
        assert!(output.contains("recovere"), "{output}");
        assert!(!output.contains("synthetic-explicit-retry-reference"));
        let final_state =
            ProviderDiscoveryState::inspect(&discovery).expect("inspect recovered observation");
        assert_eq!(
            final_state.profiles()[0].models()[0].id(),
            "gpt-5.6-recovered"
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("reread explicit retry config"),
            config_before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove explicit retry fixture");
    }

    #[test]
    fn terminal_loop_selects_configured_preset_for_current_agent_next_turn() {
        let root = terminal_test_root("model-selection");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create model selection fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[agent]
default_model_preset = "fast"

[providers.edge]
template = "openai"
credential = "synthetic-model-selection-credential-reference"

[model_presets.fast]
provider = "edge"
model = "gpt-5.6-fast"
dialect = "responses"
favorite = true

[model_presets.careful]
provider = "edge"
model = "gpt-5.6-careful"
dialect = "responses"
"#,
        )
        .expect("write model selection config");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 1)
                .expect("open Product state");
        let root_commit = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("select a Model Preset", TaskScope::default()),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("admit current Agent");
        let current_agent = match root_commit.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session.agent(),
            other => panic!("unexpected root outcome: {other:?}"),
        };
        runtime
            .acknowledge_team_operation(root_commit.operation)
            .expect("acknowledge current Agent admission");
        drop(runtime);

        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let config_before = std::fs::read(paths.user()).expect("read Model config");
        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let view = build_terminal_view(&ledger, &config, "/").expect("Model selector view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(120, 24).expect("Model selection viewport"),
            move || Ok(events.pop_front().expect("bounded Model selection events")),
        )
        .expect("Model selection loop");
        let output = String::from_utf8(output).expect("Model selection VT output");

        assert!(
            output.contains("Preset 'fast' selected for current Agent next Turn"),
            "{output}"
        );
        assert!(
            output.contains("Preset 'careful' selected for current Agent next Turn"),
            "{output}"
        );
        assert!(
            output.contains("[configured default] fast / edge / gpt-5.6-fast"),
            "{output}"
        );
        assert!(!output.contains("synthetic-model-selection-credential-reference"));
        let pending = RuntimeKernel::inspect(&ledger)
            .expect("inspect selected Preset")
            .pending_model_selection
            .expect("pending Model selection");
        assert_eq!(pending.agent(), current_agent);
        assert_eq!(pending.selection().preset_id(), "careful");
        assert_eq!(pending.selection().provider_profile(), "edge");
        assert_eq!(pending.selection().provider_model(), "gpt-5.6-careful");
        let final_view = build_terminal_view(&ledger, &config, "/").expect("final Model view");
        let mut display = TerminalSession::new("/model", 120, 24).expect("display session");
        display
            .controller
            .activate(&mut config, ConfigScope::User, None)
            .expect("open final Model selector");
        let layout = display
            .layout(Some(&config), &final_view)
            .expect("final Model layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.text() == "next turn careful")
        );
        display
            .controller
            .set_model_query("fast")
            .expect("filter default Preset");
        display.controller.activate_model_entry(&final_view.models);
        let detail_layout = display
            .layout(Some(&config), &final_view)
            .expect("default Preset detail layout");
        assert!(
            detail_layout
                .body()
                .iter()
                .any(|row| row.text() == "default true")
        );
        assert_ne!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("reread Model config"),
            config_before
        );
        std::fs::remove_dir_all(root).expect("remove model selection fixture");
    }

    #[test]
    fn terminal_loop_accepts_a_release_starter_then_selects_it_for_the_next_turn() {
        let root = terminal_test_root("model-starter-acceptance");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create starter acceptance fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.openai-main]
template = "openai"
credential = "synthetic-starter-acceptance-reference"
"#,
        )
        .expect("write starter Provider profile");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 1)
                .expect("open starter Product state");
        let root_commit = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("accept a release starter", TaskScope::default()),
                budget: ResourceBudget::new(1_000, 1),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("admit starter current Agent");
        let current_agent = match root_commit.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session.agent(),
            other => panic!("unexpected root outcome: {other:?}"),
        };
        runtime
            .acknowledge_team_operation(root_commit.operation)
            .expect("acknowledge starter Agent admission");
        drop(runtime);

        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let view = build_terminal_view(&ledger, &config, "/").expect("starter Model view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("sol".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend("frontier".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend("frontier".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(120, 24).expect("starter acceptance viewport"),
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded starter acceptance events"))
            },
        )
        .expect("starter acceptance loop");
        let output = String::from_utf8(output).expect("starter acceptance VT output");

        assert!(
            output.contains("Enter a Preset ID to accept this release starter"),
            "{output}"
        );
        assert!(output.contains("Config draft validated"), "{output}");
        assert!(output.contains("Snapshot refreshed"), "{output}");
        assert!(
            output.contains("Preset 'frontier' selected for current Agent next Turn"),
            "{output}"
        );
        assert!(!output.contains("synthetic-starter-acceptance-reference"));
        let preset = config
            .model_preset("frontier")
            .expect("resolve accepted starter");
        assert_eq!(preset.provider, "openai-main");
        assert_eq!(preset.model, "gpt-5.6-sol");
        assert_eq!(preset.dialect, ProviderDialect::Responses);
        let pending = RuntimeKernel::inspect(&ledger)
            .expect("inspect starter selection")
            .pending_model_selection
            .expect("pending starter selection");
        assert_eq!(pending.agent(), current_agent);
        assert_eq!(pending.selection().preset_id(), "frontier");
        assert_eq!(pending.selection().provider_profile(), "openai-main");
        assert_eq!(pending.selection().provider_model(), "gpt-5.6-sol");
        assert_ne!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty())
            .expect("reopen accepted starter Config");
        assert_eq!(
            reopened
                .model_preset("frontier")
                .expect("reopen accepted starter")
                .model,
            "gpt-5.6-sol"
        );
        std::fs::remove_dir_all(root).expect("remove starter acceptance fixture");
    }

    #[test]
    fn terminal_loop_updates_a_release_starter_from_real_key_events_without_product_state() {
        let root = terminal_test_root("model-starter-update");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create starter update fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 2

[providers.openai-main]
template = "openai"
credential = "private-terminal-starter-reference"
dialects = ["responses", "chat_completions"]

[model_presets.frontier]
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
favorite = true

[model_presets.frontier.starter]
catalog_key = "openai/gpt-5.6-sol"
seed_revision = "2026-08-10.1"
provider = "openai-main"
model = "gpt-5.6-sol"
dialect = "responses"
"#,
        )
        .expect("write old release starter");
        let mut config = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
            .expect("open starter update Config");
        let view = build_terminal_view(&ledger, &config, "/").expect("starter update view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(120, 24).expect("starter update viewport"),
            move || Ok(events.pop_front().expect("bounded starter update events")),
        )
        .expect("starter update loop");
        let output = String::from_utf8(output).expect("starter update VT output");

        for text in [
            "2026-08-10.1",
            "Release starter update staged; preview before commit",
            "Config draft validated",
        ] {
            assert!(output.contains(text), "missing {text}: {output}");
        }
        assert!(!output.contains("private-terminal-starter-reference"));
        let preset = config.model_preset("frontier").expect("updated starter");
        assert_eq!(preset.model, "gpt-5.6-sol");
        assert_eq!(preset.dialect, ProviderDialect::Responses);
        assert_eq!(
            preset.starter.expect("updated provenance").seed_revision,
            "2026-08-10.2"
        );
        assert!(!ledger.exists());
        assert!(!terminal_sidecar_path(&ledger, "team").exists());
        assert!(!terminal_sidecar_path(&ledger, "tool").exists());
        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty())
            .expect("reopen starter update Config");
        assert_eq!(
            reopened
                .model_preset("frontier")
                .expect("reopened starter")
                .model,
            "gpt-5.6-sol"
        );
        std::fs::remove_dir_all(root).expect("remove starter update fixture");
    }

    #[test]
    fn terminal_model_selection_without_current_agent_fails_without_creating_state() {
        let root = terminal_test_root("model-selection-no-agent");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create missing Agent fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-missing-agent-credential-reference"

[model_presets.fast]
provider = "edge"
model = "gpt-5.6-fast"
dialect = "responses"
"#,
        )
        .expect("write missing Agent config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("Config Runtime");
        let before = std::fs::read(paths.user()).expect("read missing Agent config");
        let view = build_terminal_view(&ledger, &config, "/").expect("Model selector view");
        let mut events = "model"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(120, 24).expect("missing Agent viewport"),
            move || Ok(events.pop_front().expect("bounded missing Agent events")),
        )
        .expect("missing Agent selection loop");
        let output = String::from_utf8(output).expect("missing Agent VT output");

        assert!(
            output.contains("Current Agent is unavailable; start a Turn before selecting a Preset"),
            "{output}"
        );
        assert!(!output.contains("synthetic-missing-agent-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("reread missing Agent config"),
            before
        );
        assert!(!ledger.exists());
        assert!(!terminal_sidecar_path(&ledger, "team").exists());
        assert!(!terminal_sidecar_path(&ledger, "tool").exists());
        std::fs::remove_dir_all(root).expect("remove missing Agent fixture");
    }

    #[test]
    fn terminal_loop_refreshes_models_and_statusline_from_external_config() {
        let root = terminal_test_root("model-refresh");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create model refresh fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-model-refresh-credential-reference"

[model_presets.fast]
provider = "edge"
model = "gpt-5.4-mini"
dialect = "responses"
"#,
        )
        .expect("write initial model refresh config");
        let refreshed = r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-refreshed"

[providers.edge]
template = "openai"
credential = "synthetic-model-refresh-credential-reference"

[model_presets.fast]
provider = "edge"
model = "gpt-5.4-mini"
dialect = "responses"

[model_presets.refreshed]
provider = "edge"
model = "gpt-5.6-refreshed"
dialect = "responses"
favorite = true
"#
        .as_bytes()
        .to_vec();
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("initial model view");
        let mut events = "model"
            .chars()
            .chain("refreshed".chars())
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.insert(
            "model".len(),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let user_path = paths.user().to_path_buf();
        let refreshed_for_event = refreshed.clone();

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(160, 24).expect("model refresh viewport"),
            move || {
                let event = events.pop_front().expect("bounded model refresh events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::F(6)) {
                    std::fs::write(&user_path, &refreshed_for_event)
                        .expect("write refreshed model config");
                }
                Ok(event)
            },
        )
        .expect("model refresh loop");
        let output = String::from_utf8(output).expect("model refresh VT output");

        assert!(output.contains("Snapshot refreshed"));
        assert!(output.contains("[configured] refreshed / edge / gpt-5.6-refreshed"));
        assert!(!output.contains("synthetic-model-refresh-credential-reference"));
        let refreshed_view =
            build_terminal_view(&ledger, &config, "/").expect("refreshed statusline view");
        let statusline = TerminalSession::new("/", 160, 24)
            .expect("refreshed statusline session")
            .layout(Some(&config), &refreshed_view)
            .expect("refreshed statusline layout");
        assert!(
            statusline
                .statusline_rows()
                .iter()
                .any(|row| row.text().contains("model gpt-5.6-refreshed"))
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("reread refreshed model config"),
            refreshed
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove model refresh fixture");
    }

    #[test]
    fn terminal_loop_browses_frozen_stats_without_mutating_the_ledger() {
        let root = terminal_test_root("stats-browser");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create stats browser fixture directory");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 1)
                .expect("open stats runtime");
        let root_commit = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("record usage", TaskScope::default()),
                budget: ResourceBudget::new(1_000, 2),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("admit stats root Agent");
        let root_session = match root_commit.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        runtime
            .acknowledge_team_operation(root_commit.operation)
            .expect("acknowledge stats root admission");
        let usage_days = [
            UsageWeekday::Mon,
            UsageWeekday::Tue,
            UsageWeekday::Wed,
            UsageWeekday::Thu,
            UsageWeekday::Fri,
            UsageWeekday::Sat,
            UsageWeekday::Sun,
        ];
        let usage_windows = vec![
            UsageWindow::resolve("day-am", "00:00", "12:00", usage_days, "Etc/UTC")
                .expect("resolve morning usage window"),
            UsageWindow::resolve("day-pm", "12:00", "00:00", usage_days, "Etc/UTC")
                .expect("resolve evening usage window"),
        ];
        let mut provider = CompleteStatsUsageProvider;
        for input in ["first usage attempt", "second usage attempt"] {
            let output = match runtime
                .execute_provider_turn_with_usage_windows(
                    root_session,
                    &ConfigLayers::default(),
                    usage_windows.clone(),
                    input,
                    &mut provider,
                    |_| unreachable!("deterministic provider does not request a Tool"),
                )
                .expect("execute stats fixture turn")
            {
                ProviderTurnOutcome::Prepared(output) => output,
                ProviderTurnOutcome::ApprovalRequired(_) => {
                    panic!("deterministic provider unexpectedly requested approval")
                }
            };
            runtime
                .acknowledge(output.delivery())
                .expect("acknowledge stats fixture turn");
        }
        drop(runtime);
        let before = std::fs::read(&ledger).expect("read stats ledger");
        let team_before = std::fs::read(&team_ledger).expect("read stats Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read stats Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("stats browser view");
        let mode = FakeTerminalMode::default();
        let mut events = "stats"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 23),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 23),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 22),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 21),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 20),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 20),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 20),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 20),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(Vec::new(), mode, &mut config, &view, 80, 24, move || {
            Ok(events.pop_front().expect("bounded stats browser events"))
        })
        .expect("stats browser loop");
        let output = String::from_utf8(output).expect("stats browser VT output");

        assert!(output.contains("Stats / Attempt"));
        assert!(output.contains("attempt 1"));
        assert!(output.contains("turn 2"));
        assert!(output.contains("outcome succeeded"));
        assert!(output.contains("deterministic-v1"));
        assert!(output.contains("Stats / Turn"));
        assert!(output.contains("turn 1"));
        assert!(output.contains("Stats / Provider & Model"));
        assert!(output.contains("Stats / Dialect & Policy"));
        assert!(output.contains("Stats / Thread"));
        assert!(output.contains("thread 1"));
        assert!(output.contains("attempts 2"));
        assert!(output.contains("Stats / Agent"));
        assert!(output.contains("agent 1"));
        assert!(output.contains("Stats / Team"));
        assert!(output.contains("team usage"));
        assert!(output.contains("Stats / Named Window"));
        assert!(output.contains("window day-"));
        assert!(output.contains("Stats / Token & Cache"));
        assert!(output.contains("period 1h"));
        let mut provider_model =
            TerminalSession::new("/stats", 80, 24).expect("provider and model session");
        provider_model
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("activate Stats for provider and model distributions");
        for _ in 0..2 {
            provider_model
                .handle_with_view_and_connection_tester(
                    TerminalInputEvent::Tab,
                    Some(&mut config),
                    Some(&view),
                    None,
                )
                .expect("advance to provider and model distributions");
        }
        let provider_model_layout = provider_model
            .layout(Some(&config), &view)
            .expect("provider and model layout");
        let provider_model_rows = provider_model_layout
            .body()
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>();
        assert!(
            provider_model_rows
                .iter()
                .any(|row| row.contains("provider simulator"))
        );
        assert!(
            provider_model_rows
                .iter()
                .any(|row| row.contains("requested model deterministic-v1"))
        );
        assert!(
            provider_model_rows
                .iter()
                .any(|row| row.contains("observed model ?"))
        );
        let mut policy = TerminalSession::new("/stats", 80, 24).expect("policy session");
        policy
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("activate Stats for policy distributions");
        for _ in 0..3 {
            policy
                .handle_with_view_and_connection_tester(
                    TerminalInputEvent::Tab,
                    Some(&mut config),
                    Some(&view),
                    None,
                )
                .expect("advance to policy distributions");
        }
        let policy_layout = policy
            .layout(Some(&config), &view)
            .expect("policy distribution layout");
        let policy_rows = policy_layout
            .body()
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>();
        for expected in [
            "dialect ?",
            "requested context mode canonical",
            "requested reasoning ?",
            "observed reasoning ?",
            "requested service tier ?",
            "observed service tier ?",
        ] {
            assert!(policy_rows.iter().any(|row| row.contains(expected)));
        }
        let mut token_detail =
            TerminalSession::new("/stats", 80, 24).expect("token detail session");
        token_detail
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("activate Stats for token detail");
        for _ in 0..8 {
            token_detail
                .handle_with_view_and_connection_tester(
                    TerminalInputEvent::Tab,
                    Some(&mut config),
                    Some(&view),
                    None,
                )
                .expect("advance Stats group");
        }
        token_detail
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("open token detail");
        let token_layout = token_detail
            .layout(Some(&config), &view)
            .expect("token detail layout");
        let token_rows = token_layout
            .body()
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>();
        assert!(token_rows.contains(&"cached input 20"));
        assert!(token_rows.contains(&"cache write input 10"));
        assert!(token_rows.contains(&"cache read ratio 10%"));
        assert!(token_rows.contains(&"cache write ratio 5%"));
        assert!(token_rows.contains(&"reasoning output 4"));
        let mut thread_detail =
            TerminalSession::new("/stats", 80, 24).expect("thread detail session");
        thread_detail
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("activate Stats for thread detail");
        for _ in 0..4 {
            thread_detail
                .handle_with_view_and_connection_tester(
                    TerminalInputEvent::Tab,
                    Some(&mut config),
                    Some(&view),
                    None,
                )
                .expect("advance to Thread group");
        }
        thread_detail
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("open Thread detail");
        let thread_layout = thread_detail
            .layout(Some(&config), &view)
            .expect("thread detail layout");
        let thread_rows = thread_layout
            .body()
            .iter()
            .map(|row| row.text())
            .collect::<Vec<_>>();
        assert!(thread_rows.contains(&"cache read ratio 10%"));
        assert!(thread_rows.contains(&"cache write ratio 5%"));
        assert_eq!(std::fs::read(&ledger).expect("reread stats ledger"), before);
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread stats Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread stats Tool Ledger"),
            tool_before
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove stats browser fixture");
    }

    #[test]
    fn terminal_loop_refreshes_stats_after_an_external_turn() {
        let root = terminal_test_root("stats-refresh");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create stats refresh fixture directory");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("initial stats view");
        let mut events = "stats"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let runtime_path = ledger.clone();
        let team_path = team_ledger.clone();
        let tool_path = tool_ledger.clone();
        let external_bytes = Rc::new(RefCell::new(None));
        let observed_bytes = Rc::clone(&external_bytes);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("stats refresh viewport"),
            move || {
                let event = events.pop_front().expect("bounded stats refresh events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::F(6)) {
                    let (mut runtime, _) = RuntimeKernel::open_with_team_and_tools(
                        &runtime_path,
                        &team_path,
                        &tool_path,
                        1,
                    )
                    .expect("open external stats runtime");
                    let root = runtime
                        .dispatch_team(TeamCommand::AdmitRoot {
                            task: TaskSpec::new("record refreshed usage", TaskScope::default()),
                            budget: ResourceBudget::new(1_000, 2),
                            capabilities: CapabilitySnapshot::default(),
                        })
                        .expect("admit external stats root");
                    let session = match root.commit.outcome {
                        CommandOutcome::RootAdmitted { session, .. } => session,
                        other => panic!("unexpected root outcome: {other:?}"),
                    };
                    runtime
                        .acknowledge_team_operation(root.operation)
                        .expect("acknowledge external root admission");
                    let mut provider = CompleteStatsUsageProvider;
                    let output = match runtime
                        .execute_provider_turn(
                            session,
                            &ConfigLayers::default(),
                            "refreshed usage attempt",
                            &mut provider,
                            |_| unreachable!("refresh provider does not request a Tool"),
                        )
                        .expect("execute external stats turn")
                    {
                        ProviderTurnOutcome::Prepared(output) => output,
                        ProviderTurnOutcome::ApprovalRequired(_) => {
                            panic!("refresh provider unexpectedly requested approval")
                        }
                    };
                    runtime
                        .acknowledge(output.delivery())
                        .expect("acknowledge external stats turn");
                    drop(runtime);
                    *observed_bytes.borrow_mut() = Some((
                        std::fs::read(&runtime_path).expect("read external Runtime Ledger"),
                        std::fs::read(&team_path).expect("read external Team Ledger"),
                        std::fs::read(&tool_path).expect("read external Tool Ledger"),
                    ));
                }
                Ok(event)
            },
        )
        .expect("stats refresh loop");
        let output = String::from_utf8(output).expect("stats refresh VT output");

        assert!(output.contains("Snapshot refreshed"));
        assert!(output.contains("attempt 1"));
        assert!(output.contains("input 100"));
        assert!(output.contains("cached input 10"));
        assert!(output.contains("10%"));
        assert!(output.contains("5%"));
        let external_bytes = external_bytes
            .borrow()
            .clone()
            .expect("captured external Ledger bytes");
        assert_eq!(
            std::fs::read(&ledger).expect("reread refreshed Runtime Ledger"),
            external_bytes.0
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread refreshed Team Ledger"),
            external_bytes.1
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread refreshed Tool Ledger"),
            external_bytes.2
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove stats refresh fixture");
    }

    #[test]
    fn terminal_loop_browses_agents_without_repairing_or_mutating_ledgers() {
        let root = terminal_test_root("agent-browser");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create agent browser fixture directory");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 1)
                .expect("open agent browser runtime");
        let root_commit = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new(
                    "coordinate browser fixture",
                    TaskScope::from_labels(["private-root-scope"]),
                ),
                budget: ResourceBudget::new(1_000, 8),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::WorkspaceRead,
                    Capability::Tool("private-root-capability".into()),
                ]),
            })
            .expect("admit root Agent");
        let root_session = match root_commit.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        runtime
            .acknowledge_team_operation(root_commit.operation)
            .expect("acknowledge root operation");
        let child_commit = runtime
            .dispatch_team(TeamCommand::Delegate {
                parent: root_session,
                task: TaskSpec::new(
                    "private-child-task-title",
                    TaskScope::from_labels(["private-root-scope"]),
                ),
                budget: ResourceBudget::new(200, 1),
                capabilities: CapabilitySnapshot::from_capabilities([Capability::Tool(
                    "private-root-capability".into(),
                )]),
            })
            .expect("delegate browser Agent");
        runtime
            .acknowledge_team_operation(child_commit.operation)
            .expect("acknowledge child operation");
        drop(runtime);

        OpenOptions::new()
            .append(true)
            .open(&team_ledger)
            .expect("open Team Ledger tail")
            .write_all(b"xyz")
            .expect("append incomplete Team frame");
        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("agent browser view");
        let mode = FakeTerminalMode::default();
        let mut events = "agent"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(Vec::new(), mode, &mut config, &view, 80, 24, move || {
            Ok(events.pop_front().expect("bounded agent browser events"))
        })
        .expect("agent browser loop");
        let output = String::from_utf8(output).expect("agent browser VT output");

        assert!(output.contains("agent 2"));
        assert!(output.contains("dormant"));
        assert!(output.contains("task 2"));
        assert!(output.contains("recovery"));
        assert!(output.contains("incomplete"));
        assert!(output.contains("tail"));
        assert!(!output.contains("private-root-capability"));
        assert!(!output.contains("private-root-scope"));
        assert!(!output.contains("private-child-task-title"));
        assert!(!output.contains("coordinate browser fixture"));
        assert_eq!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove agent browser fixture");
    }

    #[test]
    fn terminal_agent_actions_cancel_then_acknowledge_durably() {
        let root = terminal_test_root("agent-actions-cancel");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let held_team_ledger = root.join("runtime.ledger.team.held");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create Agent action fixture directory");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 2)
                .expect("open Agent action runtime");
        let root_commit = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("private-action-root", TaskScope::default()),
                budget: ResourceBudget::new(1_000, 8),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("admit Agent action root");
        let root_session = match root_commit.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root outcome: {other:?}"),
        };
        runtime
            .acknowledge_team_operation(root_commit.operation)
            .expect("acknowledge root operation");
        let child = runtime
            .dispatch_team(TeamCommand::Delegate {
                parent: root_session,
                task: TaskSpec::new("private-action-child", TaskScope::default()),
                budget: ResourceBudget::new(100, 1),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("delegate Agent action child");
        runtime
            .acknowledge_team_operation(child.operation)
            .expect("acknowledge child operation");
        drop(runtime);

        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("Agent action view");
        let mut events = "agent"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let cancel_team_path = team_ledger.clone();
        let cancel_held_path = held_team_ledger.clone();
        let mut cancel_enter_count = 0;

        let cancelled_output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("Agent action viewport"),
            move || {
                let event = events.pop_front().expect("bounded Agent action events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::Enter) {
                    cancel_enter_count += 1;
                    if cancel_enter_count == 3 {
                        std::fs::rename(&cancel_team_path, &cancel_held_path)
                            .expect("hide Team Ledger for failed cancellation");
                    } else if cancel_enter_count == 4 {
                        std::fs::rename(&cancel_held_path, &cancel_team_path)
                            .expect("restore Team Ledger before cancellation retry");
                    }
                }
                Ok(event)
            },
        )
        .expect("Agent action loop");
        let cancelled_output = String::from_utf8(cancelled_output).expect("Agent action VT output");

        assert!(
            cancelled_output.contains("Agent 2 / Actions"),
            "Agent action output: {cancelled_output:?}"
        );
        assert!(cancelled_output.contains("> Cancel Agent"));
        assert!(
            cancelled_output.contains("Agent cancellation failed; confirmation remains available")
        );
        assert!(!cancelled_output.contains("private-action"));

        let pending = inspect_product_team(&ledger)
            .expect("inspect pending Team operation")
            .expect("pending Team state");
        assert_eq!(
            pending
                .operations
                .iter()
                .filter(|operation| {
                    operation.status
                        == greentyper_core::agent_team::TeamOperationStatus::CommittedAwaitingAcknowledgement
                })
                .map(|operation| operation.operation.get())
                .collect::<Vec<_>>(),
            [3]
        );

        let view = build_terminal_view(&ledger, &config, "/").expect("reopened Agent action view");
        let mut events = "agent"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let team_path = team_ledger.clone();
        let held_path = held_team_ledger.clone();
        let mut enter_count = 0;
        let acknowledged_output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("reopened Agent action viewport"),
            move || {
                let event = events.pop_front().expect("bounded acknowledgement events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::Enter) {
                    enter_count += 1;
                    if enter_count == 3 {
                        std::fs::rename(&team_path, &held_path)
                            .expect("hide Team Ledger for failed acknowledgement");
                    } else if enter_count == 4 {
                        std::fs::rename(&held_path, &team_path)
                            .expect("restore Team Ledger before acknowledgement retry");
                    }
                }
                Ok(event)
            },
        )
        .expect("reopened acknowledgement loop");
        let acknowledged_output =
            String::from_utf8(acknowledged_output).expect("acknowledgement VT output");
        assert!(acknowledged_output.contains("> Acknowledge operation 3"));
        assert!(!acknowledged_output.contains("> Cancel Agent"));
        assert!(
            acknowledged_output
                .contains("Team operation acknowledgement failed; operation remains pending")
        );
        assert!(!acknowledged_output.contains("private-action"));
        assert_eq!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        let team = inspect_product_team(&ledger)
            .expect("inspect final Team Ledger")
            .expect("Team state");
        let child = team
            .projection
            .agents
            .iter()
            .find(|agent| agent.id.get() == 2)
            .expect("cancelled child");
        assert_eq!(
            child.status,
            greentyper_core::agent_team::AgentStatus::Cancelled
        );
        assert!(team.operations.iter().all(|operation| {
            operation.status == greentyper_core::agent_team::TeamOperationStatus::Acknowledged
        }));
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        assert!(!held_team_ledger.exists());
        std::fs::remove_dir_all(root).expect("remove Agent action fixture");
    }

    #[test]
    fn terminal_loop_browses_pending_tool_approval_without_mutating_ledgers() {
        let root = terminal_test_root("tool-approval-browser");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create Tool approval browser directory");
        let mut interaction = InterruptToolApproval;
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            LocalProcessExecutor::current().expect("local process executor"),
            &mut interaction,
        )
        .expect("open Product driver");
        assert!(
            driver
                .execute(
                    &ConfigLayers::default(),
                    "request Tool approval",
                    &mut PendingApprovalProvider,
                    &mut interaction,
                )
                .is_err()
        );
        drop(driver);

        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("Tool approval browser view");
        let mode = FakeTerminalMode::default();
        let mut events = "blockers"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let mut product =
            RecordingTerminalProductActions::new(std::iter::empty::<TerminalToolResolution>());
        let output = run_terminal_loop_with_product_actions(
            Vec::new(),
            mode,
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut product,
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded Tool approval browser events"))
            },
        )
        .expect("Tool approval browser loop");
        let output = String::from_utf8(output).expect("Tool approval browser VT output");

        assert!(product.loads.is_empty());
        assert!(output.contains("Blockers"));
        assert!(output.contains("tool call 1"));
        assert!(output.contains("local.echo"));
        assert!(output.contains("required"));
        assert!(output.contains("approval"));
        assert!(output.contains("recover"));
        assert!(output.contains("credentials"));
        assert!(output.contains("cost"));
        assert!(output.contains("billing"));
        assert!(!output.contains("private-terminal-approval"));
        assert_eq!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove Tool approval browser fixture");
    }

    #[test]
    fn terminal_loop_approves_or_denies_the_selected_tool_call() {
        for (label, choose_deny) in [("approve", false), ("deny", true)] {
            let root = terminal_test_root(&format!("tool-approval-{label}"));
            let ledger = root.join("runtime.ledger");
            let team_ledger = terminal_sidecar_path(&ledger, "team");
            let tool_ledger = terminal_sidecar_path(&ledger, "tool");
            let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
            std::fs::create_dir_all(&root).expect("create Tool approval action directory");
            let mut interaction = InterruptToolApproval;
            let mut driver = ProductDriver::open_with_executor(
                &ledger,
                LocalProcessExecutor::current().expect("local process executor"),
                &mut interaction,
            )
            .expect("open Product driver");
            assert!(
                driver
                    .execute(
                        &ConfigLayers::default(),
                        "request Tool approval",
                        &mut PendingApprovalProvider,
                        &mut interaction,
                    )
                    .is_err()
            );
            drop(driver);
            let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
            let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
            let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
            let mut config = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
                .expect("config runtime");
            let view =
                build_terminal_view(&ledger, &config, "/").expect("Tool approval action view");
            let mode = FakeTerminalMode::default();
            let mut events = "blockers"
                .chars()
                .map(|character| {
                    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                })
                .collect::<VecDeque<_>>();
            events.extend([
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ]);
            for _ in 0..9 {
                events.push_back(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
            }
            if choose_deny {
                events.push_back(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
            }
            events.push_back(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )));
            if !choose_deny {
                events.extend([
                    Event::Resize(80, 24),
                    Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                ]);
            }
            events.push_back(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            )));
            let mut product = if choose_deny {
                RecordingTerminalProductActions::new([TerminalToolResolution::Denied])
            } else {
                RecordingTerminalProductActions::new([TerminalToolResolution::Prepared {
                    delivery: 42,
                    text: "Echoed: approved terminal result".to_owned(),
                }])
                .fail_acknowledgement_once()
            };

            let output = run_terminal_loop_with_product_actions(
                Vec::new(),
                mode,
                &mut config,
                TerminalSnapshotSource {
                    initial: &view,
                    refresh_ledger: Some(&ledger),
                    viewport: Viewport::new(80, 24).expect("viewport"),
                },
                &mut product,
                move || {
                    Ok(events
                        .pop_front()
                        .expect("bounded Tool approval action events"))
                },
            )
            .expect("Tool approval action loop");
            let output = String::from_utf8(output).expect("Tool approval action VT output");

            assert_eq!(product.loads, [1]);
            assert_eq!(
                product.decisions,
                [(
                    1,
                    if choose_deny {
                        ProductToolDecision::Deny
                    } else {
                        ProductToolDecision::Approve
                    }
                )]
            );
            assert!(output.contains("Review the exact Tool request"));
            assert!(output.contains("terminal-approval-call"));
            assert!(output.contains(r#"{"message":"private-terminal-approval"}"#));
            assert!(output.contains("filesystem read none"));
            assert!(output.contains("process local.echo"));
            if choose_deny {
                assert!(output.contains("Tool call denied; Turn blocked"));
                assert!(product.acknowledgements.is_empty());
            } else {
                assert!(output.contains("Provider Output"));
                assert!(output.contains("approved terminal result"));
                assert!(output.contains("Acknowledge delivery 42"));
                assert!(output.contains("Output acknowledgement failed; delivery remains pending"));
                assert!(output.contains("Provider output acknowledged"));
                assert_eq!(product.acknowledgements, [42, 42]);
            }
            assert_eq!(
                std::fs::read(&ledger).expect("reread Runtime Ledger"),
                runtime_before
            );
            assert_eq!(
                std::fs::read(&team_ledger).expect("reread Team Ledger"),
                team_before
            );
            assert_eq!(
                std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
                tool_before
            );
            assert!(!paths.user().exists());
            assert!(!paths.project().exists());
            std::fs::remove_dir_all(root).expect("remove Tool approval action fixture");
        }
    }

    #[test]
    fn terminal_loop_cancels_a_rendered_tool_approval_without_mutating_state() {
        let root = terminal_test_root("tool-approval-cancel");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create Tool approval cancel directory");
        let mut interaction = InterruptToolApproval;
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            LocalProcessExecutor::current().expect("local process executor"),
            &mut interaction,
        )
        .expect("open Product driver");
        assert!(
            driver
                .execute(
                    &ConfigLayers::default(),
                    "request cancellable Tool approval",
                    &mut PendingApprovalProvider,
                    &mut interaction,
                )
                .is_err()
        );
        drop(driver);
        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("Tool approval cancel view");
        let mode = FakeTerminalMode::default();
        let mut events = "blockers"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut product =
            RecordingTerminalProductActions::new(std::iter::empty::<TerminalToolResolution>());

        let output = run_terminal_loop_with_product_actions(
            Vec::new(),
            mode,
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut product,
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded Tool approval cancel events"))
            },
        )
        .expect("Tool approval cancel loop");
        let output = String::from_utf8(output).expect("Tool approval cancel VT output");

        assert_eq!(product.loads, [1]);
        assert_eq!(product.cancellations, 1);
        assert!(product.decisions.is_empty());
        assert!(product.acknowledgements.is_empty());
        assert!(output.contains(r#"{"message":"private-terminal-approval"}"#));
        assert!(output.contains("Tool approval left pending"));
        assert_eq!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove Tool approval cancel fixture");
    }

    #[test]
    fn terminal_loop_keeps_failed_tool_approval_recoverable() {
        let root = terminal_test_root("tool-approval-retry");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create Tool approval retry directory");
        let mut interaction = InterruptToolApproval;
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            LocalProcessExecutor::current().expect("local process executor"),
            &mut interaction,
        )
        .expect("open Product driver");
        assert!(
            driver
                .execute(
                    &ConfigLayers::default(),
                    "request retryable Tool approval",
                    &mut PendingApprovalProvider,
                    &mut interaction,
                )
                .is_err()
        );
        drop(driver);
        let runtime_before = std::fs::read(&ledger).expect("read Runtime Ledger");
        let team_before = std::fs::read(&team_ledger).expect("read Team Ledger");
        let tool_before = std::fs::read(&tool_ledger).expect("read Tool Ledger");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("Tool approval retry view");
        let mode = FakeTerminalMode::default();
        let mut events = "blockers"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        for _ in 0..9 {
            events.push_back(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        for _ in 0..9 {
            events.push_back(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut product =
            RecordingTerminalProductActions::new([TerminalToolResolution::Prepared {
                delivery: 77,
                text: "Recovered Provider output".to_owned(),
            }])
            .fail_resolution_once();

        let output = run_terminal_loop_with_product_actions(
            Vec::new(),
            mode,
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut product,
            move || {
                Ok(events
                    .pop_front()
                    .expect("bounded Tool approval retry events"))
            },
        )
        .expect("Tool approval retry loop");
        let output = String::from_utf8(output).expect("Tool approval retry VT output");

        assert!(output.contains("Tool approval did not complete; inspect current blockers"));
        assert!(output.contains("Recovered Provider output"));
        assert!(output.contains("Provider output acknowledged"));
        assert_eq!(product.loads, [1, 1]);
        assert_eq!(product.cancellations, 1);
        assert_eq!(
            product.decisions,
            [
                (1, ProductToolDecision::Approve),
                (1, ProductToolDecision::Approve)
            ]
        );
        assert_eq!(product.acknowledgements, [77]);
        assert_eq!(
            std::fs::read(&ledger).expect("reread Runtime Ledger"),
            runtime_before
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread Team Ledger"),
            team_before
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread Tool Ledger"),
            tool_before
        );
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove Tool approval retry fixture");
    }

    #[test]
    fn terminal_loop_refreshes_agents_and_recovers_from_a_failed_refresh() {
        let root = terminal_test_root("agent-refresh");
        let ledger = root.join("runtime.ledger");
        let team_ledger = terminal_sidecar_path(&ledger, "team");
        let tool_ledger = terminal_sidecar_path(&ledger, "tool");
        let held_team_ledger = root.join("runtime.ledger.team.held");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create agent refresh fixture directory");
        let (mut runtime, _) =
            RuntimeKernel::open_with_team_and_tools(&ledger, &team_ledger, &tool_ledger, 1)
                .expect("open initial agent refresh runtime");
        let admitted = runtime
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("private-agent-refresh-root", TaskScope::default()),
                budget: ResourceBudget::new(1_000, 8),
                capabilities: CapabilitySnapshot::default(),
            })
            .expect("admit initial refresh root");
        runtime
            .acknowledge_team_operation(admitted.operation)
            .expect("acknowledge initial refresh root");
        drop(runtime);

        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("initial agent view");
        let mut events = "agent"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect::<VecDeque<_>>();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let runtime_path = ledger.clone();
        let team_path = team_ledger.clone();
        let tool_path = tool_ledger.clone();
        let held_path = held_team_ledger.clone();
        let external_bytes = Rc::new(RefCell::new(None));
        let observed_bytes = Rc::clone(&external_bytes);
        let mut refresh_step = 0_u8;

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("Agent refresh viewport"),
            move || {
                let event = events.pop_front().expect("bounded agent refresh events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::F(6)) {
                    refresh_step += 1;
                    match refresh_step {
                        1 => {
                            let (mut runtime, recovery) = RuntimeKernel::open_with_team_and_tools(
                                &runtime_path,
                                &team_path,
                                &tool_path,
                                1,
                            )
                            .expect("reopen agent refresh runtime");
                            let root_session = recovery
                                .into_sessions()
                                .into_iter()
                                .next()
                                .expect("rebind refresh root");
                            let child = runtime
                                .dispatch_team(TeamCommand::Delegate {
                                    parent: root_session,
                                    task: TaskSpec::new(
                                        "private-agent-refresh-child-one",
                                        TaskScope::default(),
                                    ),
                                    budget: ResourceBudget::new(200, 1),
                                    capabilities: CapabilitySnapshot::default(),
                                })
                                .expect("delegate first refreshed Agent");
                            runtime
                                .acknowledge_team_operation(child.operation)
                                .expect("acknowledge first refreshed Agent");
                        }
                        2 => {
                            std::fs::rename(&team_path, &held_path)
                                .expect("hold Team Ledger during failed refresh");
                        }
                        3 => {
                            std::fs::rename(&held_path, &team_path)
                                .expect("restore Team Ledger after failed refresh");
                            let (mut runtime, recovery) = RuntimeKernel::open_with_team_and_tools(
                                &runtime_path,
                                &team_path,
                                &tool_path,
                                1,
                            )
                            .expect("reopen restored agent refresh runtime");
                            let root_session = recovery
                                .into_sessions()
                                .into_iter()
                                .next()
                                .expect("rebind restored refresh root");
                            let child = runtime
                                .dispatch_team(TeamCommand::Delegate {
                                    parent: root_session,
                                    task: TaskSpec::new(
                                        "private-agent-refresh-child-two",
                                        TaskScope::default(),
                                    ),
                                    budget: ResourceBudget::new(100, 1),
                                    capabilities: CapabilitySnapshot::default(),
                                })
                                .expect("delegate second refreshed Agent");
                            runtime
                                .acknowledge_team_operation(child.operation)
                                .expect("acknowledge second refreshed Agent");
                            drop(runtime);
                            *observed_bytes.borrow_mut() = Some((
                                std::fs::read(&runtime_path)
                                    .expect("read final external Runtime Ledger"),
                                std::fs::read(&team_path).expect("read final external Team Ledger"),
                                std::fs::read(&tool_path).expect("read final external Tool Ledger"),
                            ));
                        }
                        _ => unreachable!("bounded refresh steps"),
                    }
                }
                Ok(event)
            },
        )
        .expect("agent refresh loop");
        let output = String::from_utf8(output).expect("agent refresh VT output");

        assert!(output.contains("agent 2"));
        assert!(output.contains("Snapshot refresh failed; showing previous snapshot"));
        assert!(output.contains("parent 1"));
        assert!(!output.contains("private-agent-refresh"));
        let refreshed =
            refresh_terminal_view(&ledger, &config, "/").expect("refresh final Agent snapshot");
        config = refreshed.config;
        let final_view = refreshed.view;
        let mut final_session =
            TerminalSession::new("/agent", 80, 24).expect("final Agent session");
        final_session
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&final_view),
                None,
            )
            .expect("open final Agent view");
        let final_layout = final_session
            .layout(Some(&config), &final_view)
            .expect("final Agent layout");
        assert!(
            final_layout
                .body()
                .iter()
                .any(|row| row.text().contains("agent 3"))
        );
        let external_bytes = external_bytes
            .borrow()
            .clone()
            .expect("captured final external Ledger bytes");
        assert_eq!(
            std::fs::read(&ledger).expect("reread final Runtime Ledger"),
            external_bytes.0
        );
        assert_eq!(
            std::fs::read(&team_ledger).expect("reread final Team Ledger"),
            external_bytes.1
        );
        assert_eq!(
            std::fs::read(&tool_ledger).expect("reread final Tool Ledger"),
            external_bytes.2
        );
        assert!(!held_team_ledger.exists());
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove agent refresh fixture");
    }

    fn terminal_sidecar_path(runtime: &Path, kind: &str) -> PathBuf {
        let mut path = OsString::from(runtime.as_os_str());
        path.push(".");
        path.push(kind);
        PathBuf::from(path)
    }

    #[test]
    fn terminal_statusline_choice_previews_and_commits_through_the_config_runtime() {
        let root = terminal_test_root("config-commit");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config statusline preset", 80, 24).expect("session");

        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("open editor"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Character('c'), Some(&mut config))
                .expect("reject no-op commit"),
            TerminalLoopOutcome::Redraw
        );
        assert!(!root.join("user.toml").exists());
        assert_eq!(
            session
                .handle(TerminalInputEvent::Down, Some(&mut config))
                .expect("choose diagnostic"),
            TerminalLoopOutcome::Redraw
        );
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("choice layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| { row.is_selected() && row.text() == "> diagnostic" })
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("preview"),
            TerminalLoopOutcome::Redraw
        );
        let recovered_screen = session.controller.screen(Some(&config)).expect("screen");
        assert!(matches!(
            recovered_screen,
            PresentationScreenView::ConfigEditor {
                dirty: true,
                validated: true,
                ..
            }
        ));
        assert_eq!(
            session
                .handle(TerminalInputEvent::Character('c'), Some(&mut config))
                .expect("commit"),
            TerminalLoopOutcome::Redraw
        );
        assert!(session.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("diagnostic")
        );

        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_eq!(
            config_string_target(&reopened, "ui.statusline.preset").as_deref(),
            Some("diagnostic")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_statusline_expansion_uses_schema_choice_metadata_and_reopens() {
        let root = terminal_test_root("config-expansion");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config statusline expansion", 80, 24).expect("session");

        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("open editor"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(session.input_context(), TerminalInputContext::ConfigChoice);
        assert_eq!(
            session
                .handle(TerminalInputEvent::Down, Some(&mut config))
                .expect("choose compact"),
            TerminalLoopOutcome::Redraw
        );
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("choice layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| { row.is_selected() && row.text() == "> compact" })
        );
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("preview");
        session
            .handle(TerminalInputEvent::Character('c'), Some(&mut config))
            .expect("commit");
        assert!(session.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "ui.statusline.expand").as_deref(),
            Some("compact")
        );

        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_eq!(
            config_string_target(&reopened, "ui.statusline.expand").as_deref(),
            Some("compact")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_statusline_choice_keeps_failed_preview_live_and_recovers() {
        let root = terminal_test_root("config-preview");
        let mut config = ConfigRuntime::open(
            ConfigPaths::new(root.join("user.toml"), root.join("project.toml")),
            ConfigDocument::empty(),
        )
        .expect("config runtime");
        let mut session =
            TerminalSession::new("/config statusline preset", 80, 24).expect("session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open editor");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("choose diagnostic");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("choose custom");
        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("failed preview stays live"),
            TerminalLoopOutcome::Redraw
        );
        let screen = session.controller.screen(Some(&config)).expect("screen");
        assert!(
            matches!(
                screen,
                PresentationScreenView::ConfigEditor {
                    dirty: true,
                    validated: false,
                    ..
                }
            ),
            "{screen:?}"
        );
        let smoke = build_smoke_view("/").expect("view");
        let frame = session
            .frame(Some(&config), smoke.view())
            .expect("error frame");
        let mut renderer = DirectVtRenderer::new(80, 24).expect("renderer");
        let output = String::from_utf8(renderer.draw(&frame).expect("draw")).expect("UTF-8");
        assert!(output.contains("\x1b[0;38;5;3m"), "{output:?}");
        assert!(output.contains("ui.statusline.custom"), "{output:?}");
        session
            .handle(TerminalInputEvent::Resize(80, 1), Some(&mut config))
            .expect("narrow resize");
        let frame = session
            .frame(Some(&config), smoke.view())
            .expect("one-row error frame");
        let mut renderer = DirectVtRenderer::new(80, 1).expect("one-row renderer");
        let output = String::from_utf8(renderer.draw(&frame).expect("draw")).expect("UTF-8");
        assert!(output.contains("\x1b[0;38;5;3m"), "{output:?}");
        assert!(output.contains("ui.statusline.custom"), "{output:?}");
        assert!(!output.contains("ready"), "{output:?}");
        session
            .handle(TerminalInputEvent::Resize(80, 24), Some(&mut config))
            .expect("restore viewport");

        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("recover choice");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("valid preview");
        session
            .handle(TerminalInputEvent::Character('c'), Some(&mut config))
            .expect("commit recovered choice");
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("diagnostic")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_statusline_choice_requires_explicit_discard_and_survives_cas_conflict() {
        let root = terminal_test_root("config-conflict");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config statusline preset", 80, 24).expect("session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open editor");
        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("choose diagnostic");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("preview diagnostic");

        winner.stage_raw("minimal").expect("stage winner");
        winner.commit(&mut config).expect("commit winner");
        assert_eq!(
            session
                .handle(TerminalInputEvent::Character('c'), Some(&mut config))
                .expect("conflict stays live"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        let recovered_screen = session.controller.screen(Some(&config)).expect("screen");
        assert!(matches!(
            recovered_screen,
            PresentationScreenView::ConfigEditor { dirty: true, .. }
        ));
        assert_eq!(
            session
                .handle(TerminalInputEvent::Escape, Some(&mut config))
                .expect("dirty escape is blocked"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty quit is blocked"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Character('d'), Some(&mut config))
                .expect("discard"),
            TerminalLoopOutcome::Redraw
        );
        assert!(session.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn crossterm_events_map_only_pressed_keys_and_resizes() {
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))),
            TerminalInputEvent::Quit
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            ))),
            TerminalInputEvent::Character('a')
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::Backspace,
                KeyModifiers::NONE,
            ))),
            TerminalInputEvent::Backspace
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::Delete,
                KeyModifiers::NONE,
            ))),
            TerminalInputEvent::Delete
        );
        assert_eq!(
            map_crossterm_event(Event::Key(
                KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE,)
            )),
            TerminalInputEvent::TestProviderConnection
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::F(5),
                KeyModifiers::CONTROL,
            ))),
            TerminalInputEvent::Ignore
        );
        assert_eq!(
            map_crossterm_event(Event::Key(
                KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE,)
            )),
            TerminalInputEvent::CredentialActions
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::F(7),
                KeyModifiers::CONTROL,
            ))),
            TerminalInputEvent::Ignore
        );
        assert_eq!(
            map_crossterm_event(Event::Key(
                KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE,)
            )),
            TerminalInputEvent::RefreshSnapshot
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            ))),
            TerminalInputEvent::RefreshSnapshot
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE,))),
            TerminalInputEvent::Tab
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new(
                KeyCode::BackTab,
                KeyModifiers::SHIFT,
            ))),
            TerminalInputEvent::BackTab
        );
        assert_eq!(
            map_crossterm_event(Event::Key(
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT,)
            )),
            TerminalInputEvent::BackTab
        );
        assert_eq!(
            map_crossterm_event(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Down,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ))),
            TerminalInputEvent::Ignore
        );
        assert_eq!(
            map_crossterm_event(Event::Resize(120, 40)),
            TerminalInputEvent::Resize(120, 40)
        );
    }

    struct RecordingConnectionTester {
        calls: Vec<(String, u64)>,
    }

    impl ProviderConnectionTester for RecordingConnectionTester {
        fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
            self.calls
                .push((profile.profile().to_owned(), profile.fingerprint()));
            ProviderConnectionTestStatus::Succeeded {
                profile: profile.profile().to_owned(),
                fingerprint: profile.fingerprint(),
                models: Vec::new(),
            }
        }
    }

    struct FailingConnectionTester {
        calls: usize,
    }

    impl ProviderConnectionTester for FailingConnectionTester {
        fn test(&mut self, _profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
            self.calls += 1;
            ProviderConnectionTestStatus::Failed {
                category: ProviderConnectionFailureCategory::Unavailable,
                retryable: true,
            }
        }
    }

    enum DiscoveryTestResult {
        Success(&'static str),
        Failure,
    }

    struct ScriptedDiscoveryTester {
        results: VecDeque<DiscoveryTestResult>,
        calls: Vec<(String, u64)>,
    }

    impl ProviderConnectionTester for ScriptedDiscoveryTester {
        fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
            self.calls
                .push((profile.profile().to_owned(), profile.fingerprint()));
            match self.results.pop_front().expect("scripted discovery result") {
                DiscoveryTestResult::Success(model) => ProviderConnectionTestStatus::Succeeded {
                    profile: profile.profile().to_owned(),
                    fingerprint: profile.fingerprint(),
                    models: vec![ObservedProviderModel {
                        id: model.to_owned(),
                        release_catalog_key: None,
                    }],
                },
                DiscoveryTestResult::Failure => ProviderConnectionTestStatus::Failed {
                    category: ProviderConnectionFailureCategory::Unavailable,
                    retryable: true,
                },
            }
        }
    }

    struct SharedScriptedDiscoveryTester {
        results: Arc<Mutex<VecDeque<DiscoveryTestResult>>>,
        calls: Arc<Mutex<Vec<(String, u64)>>>,
    }

    impl ProviderConnectionTester for SharedScriptedDiscoveryTester {
        fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
            self.calls
                .lock()
                .expect("lock discovery calls")
                .push((profile.profile().to_owned(), profile.fingerprint()));
            match self
                .results
                .lock()
                .expect("lock discovery results")
                .pop_front()
                .expect("scripted on-demand discovery result")
            {
                DiscoveryTestResult::Success(model) => ProviderConnectionTestStatus::Succeeded {
                    profile: profile.profile().to_owned(),
                    fingerprint: profile.fingerprint(),
                    models: vec![ObservedProviderModel {
                        id: model.to_owned(),
                        release_catalog_key: None,
                    }],
                },
                DiscoveryTestResult::Failure => ProviderConnectionTestStatus::Failed {
                    category: ProviderConnectionFailureCategory::Unavailable,
                    retryable: true,
                },
            }
        }
    }

    #[derive(Clone, Default)]
    struct SharedCredentialVault(Rc<RefCell<InMemoryCredentialVault>>);

    impl CredentialVault for SharedCredentialVault {
        fn bind(
            &mut self,
            scope: &ProviderCredentialScope,
            secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            self.0.borrow_mut().bind(scope, secret)
        }

        fn replace(
            &mut self,
            scope: &ProviderCredentialScope,
            secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            self.0.borrow_mut().replace(scope, secret)
        }

        fn resolve(
            &self,
            scope: &ProviderCredentialScope,
        ) -> Result<SecretValue, CredentialVaultError> {
            self.0.borrow().resolve(scope)
        }

        fn forget(
            &mut self,
            scope: &ProviderCredentialScope,
        ) -> Result<bool, CredentialVaultError> {
            self.0.borrow_mut().forget(scope)
        }
    }

    struct VaultConnectionTester {
        vault: SharedCredentialVault,
        calls: usize,
    }

    impl ProviderConnectionTester for VaultConnectionTester {
        fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
            self.calls += 1;
            let scope = match ProviderCredentialScope::from_profile(profile) {
                Ok(scope) => scope,
                Err(_) => {
                    return ProviderConnectionTestStatus::Failed {
                        category: ProviderConnectionFailureCategory::InvalidConfiguration,
                        retryable: false,
                    };
                }
            };
            match self.vault.resolve(&scope) {
                Ok(secret) => {
                    drop(secret);
                    ProviderConnectionTestStatus::Succeeded {
                        profile: profile.profile().to_owned(),
                        fingerprint: profile.fingerprint(),
                        models: Vec::new(),
                    }
                }
                Err(CredentialVaultError::NotFound) => ProviderConnectionTestStatus::Failed {
                    category: ProviderConnectionFailureCategory::CredentialMissing,
                    retryable: false,
                },
                Err(_) => ProviderConnectionTestStatus::Failed {
                    category: ProviderConnectionFailureCategory::CredentialUnavailable,
                    retryable: true,
                },
            }
        }
    }

    #[test]
    fn terminal_provider_connection_f5_is_scoped_to_the_provider_wizard() {
        let mut session = TerminalSession::new("/", 80, 24).expect("terminal session");
        let mut tester = FailingConnectionTester { calls: 0 };

        assert_eq!(
            session
                .handle_with_connection_tester(
                    TerminalInputEvent::TestProviderConnection,
                    None,
                    Some(&mut tester),
                )
                .expect("unrelated F5 is ignored"),
            TerminalLoopOutcome::Noop
        );
        assert_eq!(tester.calls, 0);
        assert!(session.notice.is_none());
    }

    #[test]
    fn terminal_loop_tests_provider_connection_from_f5_without_mutating_config() {
        let root = terminal_test_root("provider-connection-loop");
        std::fs::create_dir_all(&root).expect("create provider config directory");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before test");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut tester = RecordingConnectionTester { calls: Vec::new() };
        let mut events: VecDeque<_> = "config provider url"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_connection_tester(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            Viewport::new(80, 24).expect("viewport"),
            &mut tester,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        assert_eq!(tester.calls.len(), 1);
        assert_eq!(tester.calls[0].0, "edge");
        assert!(String::from_utf8_lossy(&output).contains("succeeded"));
        assert!(!String::from_utf8_lossy(&output).contains("synthetic-edge-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after test"),
            before
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    include!("terminal/credential_tests.rs");
    #[test]
    fn terminal_provider_connection_failure_keeps_draft_and_edit_resets_status() {
        let root = terminal_test_root("provider-connection-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before test");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config provider url", 80, 24).expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider URL editor");
        for character in "https://candidate.example.com/v1".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage candidate URL");
        }

        let mut tester = FailingConnectionTester { calls: 0 };
        session
            .handle_with_connection_tester(
                TerminalInputEvent::TestProviderConnection,
                Some(&mut config),
                Some(&mut tester),
            )
            .expect("test candidate connection");

        assert_eq!(tester.calls, 1);
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                connection: ProviderConnectionTestStatus::Failed {
                    category: ProviderConnectionFailureCategory::Unavailable,
                    retryable: true,
                },
                ..
            }
        ));
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("failed connection layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.text() == "connection failed (retryable)")
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after test"),
            before
        );

        session
            .handle(TerminalInputEvent::Character('x'), Some(&mut config))
            .expect("continue editing candidate");
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                connection: ProviderConnectionTestStatus::Untested,
                ..
            }
        ));
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_commits_statusline_choice_from_real_key_events() {
        let root = terminal_test_root("config-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config statusline preset"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("diagnostic")
        );
        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_eq!(
            config_string_target(&reopened, "ui.statusline.preset").as_deref(),
            Some("diagnostic")
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_edits_top_level_config_fields_and_reopens() {
        let root = terminal_test_root("top-level-config-fields");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");

        for (query, value) in [
            ("/config provider selected", "edge"),
            ("/config model selected", "fixture-model-v2"),
            ("/config runtime max-output", "131072"),
        ] {
            commit_terminal_config_text(&mut config, query, value);
        }

        let assert_values = |config: &ConfigRuntime| {
            let resolved = config
                .config_layers()
                .expect("resolved Config layers")
                .resolve()
                .expect("resolve Config values");
            assert_eq!(resolved.provider_profile().value(), "edge");
            assert_eq!(resolved.provider_model().value(), "fixture-model-v2");
            assert_eq!(*resolved.max_output_bytes().value(), 131_072);
        };
        assert_values(&config);
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_values(&reopened);
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_edits_statusline_reference_and_segment_lists_and_reopens() {
        let root = terminal_test_root("statusline-local-fields");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(paths.user().parent().expect("statusline config parent"))
            .expect("create statusline config directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[ui.statusline]
preset = "custom"
primary_usage_window = "workday"

[ui.statusline.custom]
left = ["mode"]
right = ["cache"]

[[stats.windows]]
id = "workday"
start = "09:00"
end = "17:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "local"

[[stats.windows]]
id = "after-hours"
start = "17:00"
end = "23:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "local"
"#,
        )
        .expect("write statusline config");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");

        for (query, value) in [
            ("/config statusline usage-window", "after-hours"),
            ("/config statusline left", "[\"mode\",\"agent\",\"task\"]"),
            (
                "/config statusline right",
                "[\"model\",\"context\",\"thread_cost\"]",
            ),
        ] {
            commit_terminal_config_text(&mut config, query, value);
        }

        let assert_values = |config: &ConfigRuntime| {
            assert_eq!(
                config_string_target(config, "ui.statusline.primary_usage_window").as_deref(),
                Some("after-hours")
            );
            for (path, expected) in [
                ("ui.statusline.custom.left", &["mode", "agent", "task"][..]),
                (
                    "ui.statusline.custom.right",
                    &["model", "context", "thread_cost"][..],
                ),
            ] {
                let field = config
                    .inspect_field(ConfigScope::User, path)
                    .expect("inspect statusline list");
                let ConfigFieldContents::Value {
                    target: Some(ConfigValue::StringList(values)),
                    ..
                } = field.contents
                else {
                    panic!("expected statusline list target")
                };
                assert_eq!(
                    values.iter().map(String::as_str).collect::<Vec<_>>(),
                    expected
                );
            }
        };
        assert_values(&config);
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_values(&reopened);
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_edits_existing_model_pricing_and_usage_objects_and_reopens() {
        let root = terminal_test_root("existing-config-objects");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(paths.user().parent().expect("Config fixture parent"))
            .expect("create Config fixture directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-edge-credential-reference"

[providers.edge.pricing]
source = "manual"

[model_presets.fast]
provider = "edge"
model = "fixture-model"
dialect = "responses"

[price_schedules.manual]
version = "2026-08-11.1"
currency = "USD"
provider = "edge"
model = "fixture-model"
minimum_context_tokens = 0
effective_from = "2026-08-11T00:00:00Z"
source = "manual"
source_ref = "synthetic-manual-rate-card"

[price_schedules.manual.rates]
input_micros_per_million = 1
cached_input_micros_per_million = 2
cache_write_micros_per_million = 3
output_micros_per_million = 4
reasoning_output_micros_per_million = 5

[[stats.windows]]
id = "workday"
start = "09:00"
end = "17:00"
days = ["mon", "tue", "wed", "thu", "fri"]
timezone = "local"
"#,
        )
        .expect("write Config fixture");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");

        for (query, value) in [
            ("/config model model", "fixture-model-v2"),
            ("/config pricing currency", "EUR"),
            ("/config stats-window timezone", "Asia/Hong_Kong"),
        ] {
            commit_existing_object_config_text(&mut config, query, value);
        }

        let assert_values = |config: &ConfigRuntime| {
            assert_eq!(
                config_string_target(config, "model_presets.fast.model").as_deref(),
                Some("fixture-model-v2")
            );
            assert_eq!(
                config_string_target(config, "price_schedules.manual.currency").as_deref(),
                Some("EUR")
            );
            assert_eq!(
                config_string_target(config, "stats.windows.workday.timezone").as_deref(),
                Some("Asia/Hong_Kong")
            );
        };
        assert_values(&config);
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_values(&reopened);
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_selects_provider_and_commits_url_from_real_key_events() {
        let root = terminal_test_root("provider-url-loop");
        std::fs::create_dir_all(&root).expect("create provider config directory");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config provider url"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(
            "https://new-gateway.example.com/v2"
                .chars()
                .map(|character| {
                    Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                }),
        );
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        assert_eq!(
            config_string_target(&config, "providers.edge.base_url").as_deref(),
            Some("https://new-gateway.example.com/v2")
        );
        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_eq!(
            config_string_target(&reopened, "providers.edge.base_url").as_deref(),
            Some("https://new-gateway.example.com/v2")
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_edits_all_non_secret_provider_fields_from_real_key_events() {
        let root = terminal_test_root("provider-non-secret-fields-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config provider url"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        for value in [
            "http://127.0.0.1:43123/v1",
            "/v2/responses",
            "/v2/chat/completions",
            "/v2/messages",
            "/v2/models",
            "[\"responses\",\"messages\"]",
        ] {
            events.extend(value.chars().map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            }));
            events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let assert_profile = |config: &ConfigRuntime| {
            let profile = config
                .provider_profile("edge")
                .expect("resolve Provider Profile")
                .expect("external Provider Profile");
            assert_eq!(profile.base_url(), Some("http://127.0.0.1:43123/v1"));
            assert_eq!(
                profile.route(ProviderDialect::Responses),
                Some("/v2/responses")
            );
            assert_eq!(
                profile.route(ProviderDialect::ChatCompletions),
                Some("/v2/chat/completions")
            );
            assert_eq!(
                profile.route(ProviderDialect::Messages),
                Some("/v2/messages")
            );
            assert_eq!(profile.models_route(), Some("/v2/models"));
            assert_eq!(
                profile.dialects().collect::<Vec<_>>(),
                vec![ProviderDialect::Responses, ProviderDialect::Messages]
            );
            assert_eq!(
                profile.pricing_source(),
                Some(ProviderPricingSource::Manual)
            );
            assert!(profile.allow_insecure_loopback());
            assert_eq!(
                config_string_target(config, "providers.edge.catalog.mode").as_deref(),
                Some("manual")
            );
        };
        assert_profile(&config);
        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_profile(&reopened);
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_provider_loopback_permission_rejects_remote_origin_and_recovers() {
        let root = terminal_test_root("provider-loopback-policy-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session = TerminalSession::new("/config provider insecure-loopback", 80, 24)
            .expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open loopback permission");
        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("enable insecure loopback");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("invalid preview remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at providers.edge.allow_insecure_loopback")
        );
        assert!(session.controller.has_unsaved_config_draft());

        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("disable insecure loopback");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("validate repaired policy");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));
        session
            .handle(TerminalInputEvent::Character('c'), Some(&mut config))
            .expect("commit repaired policy");
        assert!(
            !config
                .provider_profile("edge")
                .expect("resolve Provider Profile")
                .expect("external Provider Profile")
                .allow_insecure_loopback()
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_creates_provider_profile_from_real_key_events() {
        let root = terminal_test_root("provider-create-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let credential_reference = "hidden-binding-ref";
        let mut events: VecDeque<_> = "config provider add"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("openai-main".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        ]);
        events.extend(credential_reference.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let profile = config
            .provider_profile("openai-main")
            .expect("resolve profile")
            .expect("created profile");
        assert_eq!(profile.template(), "openai");
        assert_eq!(profile.credential_reference(), Some(credential_reference));
        assert!(!String::from_utf8_lossy(&output).contains(credential_reference));
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        let profile = reopened
            .provider_profile("openai-main")
            .expect("resolve reopened profile")
            .expect("reopened profile");
        assert_eq!(profile.template(), "openai");
        assert_eq!(profile.credential_reference(), Some(credential_reference));
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_creates_model_preset_from_real_key_events() {
        let root = terminal_test_root("model-preset-create-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config model add"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("fast".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("edge".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.extend("fixture-model".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        ]);
        events.extend("2048".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let preset = config
            .model_preset("fast")
            .expect("resolve created Model Preset");
        assert_eq!(preset.provider, "edge");
        assert_eq!(preset.model, "fixture-model");
        assert_eq!(preset.dialect, ProviderDialect::Responses);
        assert_eq!(preset.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(preset.service_tier, Some(ServiceTier::Fast));
        assert_eq!(preset.max_output_tokens, Some(2_048));
        assert_eq!(preset.context_mode, Some(ContextMode::Canonical));
        assert!(preset.favorite);
        assert!(preset.fallback.is_empty());
        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        let preset = reopened
            .model_preset("fast")
            .expect("resolve reopened Model Preset");
        assert_eq!(preset.provider, "edge");
        assert_eq!(preset.model, "fixture-model");
        assert_eq!(preset.dialect, ProviderDialect::Responses);
        assert_eq!(preset.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(preset.service_tier, Some(ServiceTier::Fast));
        assert_eq!(preset.max_output_tokens, Some(2_048));
        assert_eq!(preset.context_mode, Some(ContextMode::Canonical));
        assert!(preset.favorite);
        assert!(preset.fallback.is_empty());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_creates_usage_window_from_real_key_events() {
        let root = terminal_test_root("usage-window-create-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config stats-window add"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("work".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        for value in [
            "09:00",
            "17:00",
            "[\"mon\",\"tue\",\"wed\",\"thu\",\"fri\"]",
            "Asia/Hong_Kong",
        ] {
            events.extend(value.chars().map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            }));
            if value != "Asia/Hong_Kong" {
                events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            }
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let windows = config
            .resolved_usage_windows()
            .expect("resolve created Usage Window");
        let window = windows.first().expect("created Usage Window");
        assert_eq!(windows.len(), 1);
        assert_eq!(window.id(), "work");
        assert_eq!(window.start_minute(), 9 * 60);
        assert_eq!(window.end_minute(), 17 * 60);
        assert_eq!(
            window.days().collect::<Vec<_>>(),
            vec![
                UsageWeekday::Mon,
                UsageWeekday::Tue,
                UsageWeekday::Wed,
                UsageWeekday::Thu,
                UsageWeekday::Fri,
            ]
        );
        assert_eq!(window.timezone(), "Asia/Hong_Kong");
        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        let windows = reopened
            .resolved_usage_windows()
            .expect("resolve reopened Usage Window");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id(), "work");
        assert_eq!(windows[0].timezone(), "Asia/Hong_Kong");
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_creates_price_schedule_from_real_key_events() {
        let root = terminal_test_root("price-schedule-create-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_manual_pricing_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config pricing add"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend("openai-sol".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        for value in ["2026-08-11.1", "USD", "edge", "gpt-5.6-sol"] {
            events.extend(value.chars().map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            }));
            events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        ]);
        events.extend("priority".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.extend("0".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.extend("200000".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.extend("2026-08-11T00:00:00Z".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        events.extend("2026-09-11T00:00:00Z".chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        ]);
        for value in [
            "synthetic-manual-rate-card",
            "1000000",
            "500000",
            "0",
            "2000000",
            "3000000",
        ] {
            events.extend(value.chars().map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            }));
            if value != "3000000" {
                events.push_back(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            }
        }
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let schedules = config
            .resolved_price_schedules()
            .expect("resolve created Price Schedule");
        let schedule = schedules
            .schedules()
            .first()
            .expect("created Price Schedule");
        assert_eq!(schedules.schedules().len(), 1);
        assert_eq!(schedule.id(), "openai-sol");
        assert_eq!(schedule.version(), "2026-08-11.1");
        assert_eq!(schedule.currency(), "USD");
        assert_eq!(schedule.provider_profile(), "edge");
        assert_eq!(schedule.model(), "gpt-5.6-sol");
        assert_eq!(schedule.dialect(), Some(ProviderDialect::Responses));
        assert_eq!(schedule.service_tier(), Some("priority"));
        assert_eq!(schedule.minimum_context_tokens(), 0);
        assert_eq!(schedule.maximum_context_tokens(), Some(200_000));
        assert_eq!(schedule.effective_from().unix_millis(), 1_786_406_400_000);
        assert_eq!(
            schedule.effective_until().unwrap().unix_millis(),
            1_789_084_800_000
        );
        assert_eq!(schedule.source(), PriceScheduleSource::Manual);
        assert_eq!(schedule.source_ref(), "synthetic-manual-rate-card");
        let rates = schedule.rates();
        assert_eq!(rates.input_micros_per_million(), 1_000_000);
        assert_eq!(rates.cached_input_micros_per_million(), 500_000);
        assert_eq!(rates.cache_write_micros_per_million(), 0);
        assert_eq!(rates.output_micros_per_million(), 2_000_000);
        assert_eq!(rates.reasoning_output_micros_per_million(), 3_000_000);
        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        let schedules = reopened
            .resolved_price_schedules()
            .expect("resolve reopened Price Schedule");
        assert_eq!(schedules.schedules().len(), 1);
        assert_eq!(schedules.schedules()[0].id(), "openai-sol");
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_price_schedule_recovers_from_validation_and_cas_conflict() {
        let root = terminal_test_root("price-schedule-create-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_manual_pricing_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config pricing add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Price Schedule ID prompt");
        type_terminal_config_text(&mut session, &mut config, "openai-sol");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Price Schedule version field");
        for value in ["2026-08-11.1", "USD", "edge", "gpt-5.6-sol"] {
            type_terminal_config_text(&mut session, &mut config, value);
            session
                .handle(TerminalInputEvent::Tab, Some(&mut config))
                .expect("focus next Price Schedule field");
        }
        for _ in 0..2 {
            session
                .handle(TerminalInputEvent::Tab, Some(&mut config))
                .expect("skip optional Price Schedule field");
        }
        for value in ["0", "0", "2026-08-11T00:00:00Z"] {
            type_terminal_config_text(&mut session, &mut config, value);
            session
                .handle(TerminalInputEvent::Tab, Some(&mut config))
                .expect("focus next Price Schedule field");
        }
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("skip optional effective-until field");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("select manual Price Schedule source");
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("focus Price Schedule source reference");
        for value in [
            "synthetic-manual-rate-card",
            "1000000",
            "500000",
            "0",
            "2000000",
            "3000000",
        ] {
            type_terminal_config_text(&mut session, &mut config, value);
            if value != "3000000" {
                session
                    .handle(TerminalInputEvent::Tab, Some(&mut config))
                    .expect("focus next Price Schedule rate");
            }
        }

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("invalid Price Schedule remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at price_schedules.openai-sol")
        );
        assert!(session.controller.has_unsaved_config_draft());

        for _ in 0..9 {
            session
                .handle(TerminalInputEvent::BackTab, Some(&mut config))
                .expect("return to maximum context field");
        }
        assert_eq!(
            session
                .controller
                .config_editor_field()
                .expect("maximum context field")
                .path,
            "price_schedules.openai-sol.maximum_context_tokens"
        );
        type_terminal_config_text(&mut session, &mut config, "1");
        for _ in 0..9 {
            session
                .handle(TerminalInputEvent::Tab, Some(&mut config))
                .expect("return to final Price Schedule rate");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("validate repaired Price Schedule");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));

        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        winner.stage_raw("minimal").expect("stage winning change");
        winner.commit(&mut config).expect("commit winning change");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("stale Price Schedule remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert!(session.controller.has_unsaved_config_draft());
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty Price Schedule blocks quit"),
            TerminalLoopOutcome::Redraw
        );
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("request Price Schedule discard");
        assert_eq!(session.notice.as_deref(), Some("Discard Config draft?"));
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("discard stale Price Schedule");
        assert!(session.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );
        assert!(
            config
                .resolved_price_schedules()
                .unwrap()
                .schedules()
                .is_empty()
        );
        drop(config);

        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert_eq!(
            config_string_target(&reopened, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );
        assert!(
            reopened
                .resolved_price_schedules()
                .unwrap()
                .schedules()
                .is_empty()
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_usage_window_moves_across_all_editable_fields_and_commits() {
        let root = terminal_test_root("usage-window-create-fields");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config stats-window add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window ID prompt");
        for character in "work".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage Usage Window ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window start field");

        for (path, value) in [
            ("stats.windows.work.start", "09:00"),
            ("stats.windows.work.end", "17:00"),
            (
                "stats.windows.work.days",
                "[\"mon\",\"tue\",\"wed\",\"thu\",\"fri\"]",
            ),
            ("stats.windows.work.timezone", "Asia/Hong_Kong"),
        ] {
            assert_eq!(
                session
                    .controller
                    .config_editor_field()
                    .expect("focused Usage Window field")
                    .path,
                path
            );
            assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
            for character in value.chars() {
                session
                    .handle(TerminalInputEvent::Character(character), Some(&mut config))
                    .expect("stage Usage Window field");
            }
            if path != "stats.windows.work.timezone" {
                session
                    .handle(TerminalInputEvent::Tab, Some(&mut config))
                    .expect("focus next Usage Window field");
            }
        }

        session
            .handle(TerminalInputEvent::BackTab, Some(&mut config))
            .expect("return to Usage Window days");
        assert_eq!(
            session
                .config_text
                .as_ref()
                .expect("Usage Window days input")
                .value,
            "[\"mon\",\"tue\",\"wed\",\"thu\",\"fri\"]"
        );
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("return to Usage Window timezone");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("preview Usage Window");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("commit Usage Window");
        assert!(session.controller.is_slash_panel());
        assert_eq!(config.resolved_usage_windows().unwrap().len(), 1);

        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_usage_window_buffers_partial_days_and_requires_discard() {
        let root = terminal_test_root("usage-window-partial-days");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config stats-window add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window ID prompt");
        for character in "work".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage Usage Window ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window start field");
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("focus Usage Window end");
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("focus Usage Window days");
        session
            .handle(TerminalInputEvent::Character('['), Some(&mut config))
            .expect("buffer partial Usage Window days");

        assert!(!session.controller.has_unsaved_config_draft());
        assert!(session.has_unsaved_config_input());
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("partial Usage Window layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.is_selected() && row.text() == "> target [")
        );
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.text() == "draft pending")
        );
        for _ in 1..MAX_CONFIG_STRING_BYTES {
            session
                .handle(TerminalInputEvent::Character('a'), Some(&mut config))
                .expect("fill Usage Window days input");
        }
        session
            .handle(TerminalInputEvent::Character('a'), Some(&mut config))
            .expect("reject oversized Usage Window days input");
        assert_eq!(
            session
                .config_text
                .as_ref()
                .expect("bounded Usage Window days input")
                .value
                .len(),
            MAX_CONFIG_STRING_BYTES
        );
        assert_eq!(
            session.notice.as_deref(),
            Some("Config value exceeds its input limit")
        );
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("partial Usage Window blocks quit"),
            TerminalLoopOutcome::Redraw
        );
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("invalid list remains focused");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at stats.windows.work.days")
        );
        assert_eq!(
            session
                .controller
                .config_editor_field()
                .expect("Usage Window days remains focused")
                .path,
            "stats.windows.work.days"
        );
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("request partial Usage Window discard");
        assert_eq!(session.notice.as_deref(), Some("Discard Config draft?"));
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("discard partial Usage Window");
        assert!(session.controller.is_slash_panel());
        assert!(config.resolved_usage_windows().unwrap().is_empty());

        if root.exists() {
            std::fs::remove_dir_all(root).expect("remove test config");
        }
    }

    #[test]
    fn terminal_usage_window_recovers_from_validation_and_cas_conflict() {
        let root = terminal_test_root("usage-window-create-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config stats-window add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window ID prompt");
        for character in "work".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage Usage Window ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Usage Window start field");
        for (value, move_next) in [
            ("09:00", true),
            ("09:00", true),
            ("[\"mon\"]", true),
            ("Asia/Hong_Kong", false),
        ] {
            for character in value.chars() {
                session
                    .handle(TerminalInputEvent::Character(character), Some(&mut config))
                    .expect("stage Usage Window field");
            }
            if move_next {
                session
                    .handle(TerminalInputEvent::Tab, Some(&mut config))
                    .expect("focus next Usage Window field");
            }
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("invalid Usage Window remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at stats.windows.work")
        );

        session
            .handle(TerminalInputEvent::BackTab, Some(&mut config))
            .expect("return to Usage Window days");
        session
            .handle(TerminalInputEvent::BackTab, Some(&mut config))
            .expect("return to Usage Window end");
        for character in "17:00".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("repair Usage Window end");
        }
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("return to Usage Window days");
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("return to Usage Window timezone");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("validate repaired Usage Window");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));

        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        winner.stage_raw("minimal").expect("stage winning change");
        winner.commit(&mut config).expect("commit winning change");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("stale Usage Window remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("editor"),
            PresentationScreenView::ConfigEditor {
                dirty: true,
                validated: true,
                ..
            }
        ));
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty quit is blocked"),
            TerminalLoopOutcome::Redraw
        );
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("request stale Usage Window discard");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("discard stale Usage Window");
        assert!(session.controller.is_slash_panel());
        assert!(config.resolved_usage_windows().unwrap().is_empty());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );

        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_model_preset_recovers_from_missing_field_and_cas_conflict() {
        let root = terminal_test_root("model-preset-create-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config model add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Model Preset ID prompt");
        for character in "fast".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage Model Preset ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Model Preset provider field");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
        for character in "edge".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage provider");
        }
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("focus model");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("focus dialect");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigChoice);
        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("start dialect navigation from the end");
        assert_eq!(session.config_choice(), Some("messages"));
        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("select Chat Completions dialect");
        session
            .handle(TerminalInputEvent::Up, Some(&mut config))
            .expect("select Responses dialect");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("missing model remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at model_presets.fast.model")
        );

        session
            .handle(TerminalInputEvent::BackTab, Some(&mut config))
            .expect("return to model");
        for character in "fixture-model".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("repair model");
        }
        session
            .handle(TerminalInputEvent::Tab, Some(&mut config))
            .expect("return to dialect");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("validate repaired Model Preset");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));

        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        winner.stage_raw("minimal").expect("stage winning change");
        winner.commit(&mut config).expect("commit winning change");

        session
            .handle(TerminalInputEvent::Character('c'), Some(&mut config))
            .expect("stale Model Preset remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("editor"),
            PresentationScreenView::ConfigEditor {
                dirty: true,
                validated: true,
                ..
            }
        ));
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty quit is blocked"),
            TerminalLoopOutcome::Redraw
        );
        session
            .handle(TerminalInputEvent::Character('d'), Some(&mut config))
            .expect("discard stale Model Preset");
        assert!(session.controller.is_slash_panel());
        assert!(config.model_preset("fast").is_err());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_confirms_provider_deletion_and_reopens() {
        let root = terminal_test_root("provider-delete-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config provider remove"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.ends_with(LEAVE_TERMINAL));
        assert!(!config.addressable_objects().unwrap().contains(&object));
        drop(config);
        let reopened = ConfigRuntime::open(paths, ConfigDocument::empty()).expect("reopen config");
        assert!(!reopened.addressable_objects().unwrap().contains(&object));
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_provider_deletion_cancels_and_survives_cas_conflict() {
        let root = terminal_test_root("provider-delete-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config provider remove", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider deletion confirmation");
        assert_eq!(
            session.input_context(),
            TerminalInputContext::ConfigDeleteConfirmation
        );
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("deletion layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.text() == "Delete provider edge")
        );
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("cancel provider deletion");
        assert!(session.controller.is_slash_panel());
        assert!(config.addressable_objects().unwrap().contains(&object));

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("reopen provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("reopen provider deletion confirmation");
        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        winner
            .stage_raw("diagnostic")
            .expect("stage winning change");
        winner.commit(&mut config).expect("commit winning change");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("stale deletion remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert_eq!(
            session.input_context(),
            TerminalInputContext::ConfigDeleteConfirmation
        );
        assert!(config.addressable_objects().unwrap().contains(&object));
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty quit is blocked"),
            TerminalLoopOutcome::Redraw
        );
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("discard stale deletion");
        assert!(session.controller.is_slash_panel());
        assert!(config.addressable_objects().unwrap().contains(&object));
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_provider_create_recovers_from_invalid_id_and_cas_conflict() {
        let root = terminal_test_root("provider-create-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session =
            TerminalSession::new("/config provider add", 80, 24).expect("terminal session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider ID prompt");
        assert_eq!(
            session.input_context(),
            TerminalInputContext::ConfigObjectId
        );
        for character in "Bad".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage invalid Provider ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("invalid Provider ID remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at <id>")
        );
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("ID prompt"),
            PresentationScreenView::ConfigObjectCreate {
                kind: ConfigObjectKind::ProviderProfile,
                ref id,
            } if id == "Bad"
        ));

        session
            .handle(TerminalInputEvent::Delete, Some(&mut config))
            .expect("clear invalid Provider ID");
        for character in "edge".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage corrected Provider ID");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider template picker");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigChoice);
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("select OpenAI template");
        assert_eq!(
            session
                .handle(TerminalInputEvent::Quit, Some(&mut config))
                .expect("dirty quit is blocked"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session.notice.as_deref(),
            Some("Config draft must be committed or discarded")
        );

        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config statusline preset",
            0,
            None,
        )
        .expect("winner editor");
        winner.stage_raw("minimal").expect("stage winning change");
        winner.commit(&mut config).expect("commit winning change");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("stale preview remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("wizard"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                validated: false,
                ..
            }
        ));
        session
            .handle(TerminalInputEvent::Character('d'), Some(&mut config))
            .expect("discard stale Provider draft");
        assert!(session.controller.is_slash_panel());
        assert!(config.provider_profile("edge").is_err());
        assert_eq!(
            config_string_target(&config, "ui.statusline.preset").as_deref(),
            Some("minimal")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_provider_url_keeps_invalid_draft_live_and_recovers() {
        let root = terminal_test_root("provider-url-recovery");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths, ConfigDocument::empty()).expect("config runtime");
        let mut session = TerminalSession::new("/config provider url", 80, 24).expect("session");

        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider selector");
        let smoke = build_smoke_view("/").expect("view");
        let layout = session
            .layout(Some(&config), smoke.view())
            .expect("provider selector layout");
        assert!(
            layout
                .body()
                .iter()
                .any(|row| row.is_selected() && row.text() == "> provider edge")
        );
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open provider URL editor");
        let editor_layout = session
            .layout(Some(&config), smoke.view())
            .expect("provider URL editor layout");
        assert!(editor_layout.body().iter().any(|row| {
            row.is_selected() && row.text() == "> target https://gateway.example.com/v1"
        }));
        session
            .handle(TerminalInputEvent::Backspace, Some(&mut config))
            .expect("reset existing URL with backspace");
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                editor: greentyper_core::config::ConfigEditorView {
                    field: greentyper_core::config::ConfigFieldView {
                        contents: ConfigFieldContents::Value { target: None, .. },
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        for character in "http://provider.invalid/v1".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage invalid URL");
        }
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("invalid preview remains live");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config validation failed at providers.edge.base_url")
        );
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                validated: false,
                ..
            }
        ));
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("request discard");
        assert_eq!(session.notice.as_deref(), Some("Discard Config draft?"));
        session
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("cancel discard");
        assert!(session.notice.is_none());
        assert!(!session.confirming_discard);
        assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
        let delete = map_crossterm_event(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::NONE,
        )));
        assert_eq!(delete, TerminalInputEvent::Delete);
        session
            .handle(delete, Some(&mut config))
            .expect("clear invalid URL");
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                editor: greentyper_core::config::ConfigEditorView {
                    field: greentyper_core::config::ConfigFieldView {
                        contents: ConfigFieldContents::Value { target: None, .. },
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        for character in "https://recovered.example.com/v1".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage recovered URL");
        }
        let recovered_screen = session.controller.screen(Some(&config)).expect("screen");
        assert!(
            matches!(
                recovered_screen,
                PresentationScreenView::ProviderWizard {
                    editor: greentyper_core::config::ConfigEditorView {
                        field: greentyper_core::config::ConfigFieldView {
                            contents: ConfigFieldContents::Value {
                                target: Some(ConfigValue::String(ref target)),
                                ..
                            },
                            ..
                        },
                        ..
                    },
                    ..
                } if target == "https://recovered.example.com/v1"
            ),
            "{recovered_screen:?}"
        );
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("preview recovered URL");
        assert_eq!(session.notice.as_deref(), Some("Config draft validated"));
        assert!(matches!(
            session.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard {
                dirty: true,
                validated: true,
                editor: greentyper_core::config::ConfigEditorView {
                    field: greentyper_core::config::ConfigFieldView {
                        contents: ConfigFieldContents::Value {
                            target: Some(ConfigValue::String(ref target)),
                            ..
                        },
                        ..
                    },
                    ..
                },
                ..
            } if target == "https://recovered.example.com/v1"
        ));
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("commit recovered URL");
        assert!(session.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "providers.edge.base_url").as_deref(),
            Some("https://recovered.example.com/v1")
        );

        let mut conflict =
            TerminalSession::new("/config provider url", 80, 24).expect("conflict session");
        conflict
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open conflict provider selector");
        conflict
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open conflict provider URL editor");
        for character in "https://loser.example.com/v1".chars() {
            conflict
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("stage losing URL");
        }
        conflict
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("preview losing URL");

        let object = ConfigObjectRef::new(ConfigObjectKind::ProviderProfile, "edge");
        let mut winner = ConfigEditorSession::open_from_query(
            &config,
            ConfigScope::User,
            "/config provider url",
            0,
            Some(&object),
        )
        .expect("winner editor");
        winner
            .stage_raw("https://winner.example.com/v1")
            .expect("stage winning URL");
        winner.commit(&mut config).expect("commit winning URL");

        let mut tester = RecordingConnectionTester { calls: Vec::new() };
        conflict
            .handle_with_connection_tester(
                TerminalInputEvent::TestProviderConnection,
                Some(&mut config),
                Some(&mut tester),
            )
            .expect("stale connection test remains live");
        assert!(tester.calls.is_empty());
        assert_eq!(
            conflict.notice.as_deref(),
            Some("Config changed; discard and reopen the editor")
        );
        assert!(matches!(
            conflict.controller.screen(Some(&config)).expect("screen"),
            PresentationScreenView::ProviderWizard { dirty: true, .. }
        ));
        conflict
            .handle(TerminalInputEvent::Escape, Some(&mut config))
            .expect("request conflict discard");
        assert_eq!(conflict.notice.as_deref(), Some("Discard Config draft?"));
        conflict
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("confirm conflict discard");
        assert!(conflict.controller.is_slash_panel());
        assert_eq!(
            config_string_target(&config, "providers.edge.base_url").as_deref(),
            Some("https://winner.example.com/v1")
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[derive(Clone, Default)]
    struct FakeTerminalMode {
        enabled: Rc<Cell<u32>>,
        disabled: Rc<Cell<u32>>,
    }

    impl TerminalMode for FakeTerminalMode {
        fn enable_raw_mode(&mut self) -> std::io::Result<()> {
            self.enabled.set(self.enabled.get() + 1);
            Ok(())
        }

        fn disable_raw_mode(&mut self) -> std::io::Result<()> {
            self.disabled.set(self.disabled.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn terminal_surface_restores_mode_cursor_and_screen() {
        let mode = FakeTerminalMode::default();
        let observed = mode.clone();
        let mut surface = TerminalSurface::enter(Vec::new(), mode).expect("enter terminal");
        surface.write_frame(b"frame").expect("write frame");
        let output = surface.finish().expect("finish terminal");

        assert_eq!(observed.enabled.get(), 1);
        assert_eq!(observed.disabled.get(), 1);
        assert_eq!(
            output,
            b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[Hframe\x1b[0m\x1b[?25h\x1b[?1049l"
        );
    }

    #[test]
    fn terminal_view_inspects_missing_state_without_creating_it() {
        let root = std::env::temp_dir().join(format!(
            "greentyper-terminal-view-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let ledger = root.join("runtime.ledger");
        let mut config = ConfigRuntime::open(
            ConfigPaths::new(root.join("user.toml"), root.join("project.toml")),
            ConfigDocument::empty(),
        )
        .expect("config runtime");

        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let session = TerminalSession::new("/", 80, 24).expect("session");
        let frame =
            TerminalFrame::from_layout(&session.layout(Some(&config), &view).expect("layout"))
                .expect("frame");
        let mut renderer = DirectVtRenderer::new(80, 24).expect("renderer");
        let output = String::from_utf8(renderer.draw(&frame).expect("draw")).expect("UTF-8");

        assert!(output.contains("ready"));
        assert!(output.contains("model deterministic-v1"));

        let mut agent_session = TerminalSession::new("/agent", 80, 24).expect("Agent session");
        agent_session
            .handle_with_view_and_connection_tester(
                TerminalInputEvent::Enter,
                Some(&mut config),
                Some(&view),
                None,
            )
            .expect("open missing Agent Team view");
        let agent_layout = agent_session
            .layout(Some(&config), &view)
            .expect("missing Agent Team layout");
        assert!(
            agent_layout
                .body()
                .iter()
                .any(|row| row.text() == "Agent Team unavailable")
        );
        assert!(!ledger.exists());
        assert!(!terminal_sidecar_path(&ledger, "team").exists());
        assert!(!terminal_sidecar_path(&ledger, "tool").exists());
    }

    #[test]
    fn terminal_loop_refreshes_an_unchanged_snapshot_without_writing_state() {
        let root = terminal_test_root("snapshot-refresh");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("initial view");
        let mut events = VecDeque::from([
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("snapshot refresh viewport"),
            move || Ok(events.pop_front().expect("bounded refresh events")),
        )
        .expect("refresh loop");

        assert!(String::from_utf8_lossy(&output).contains("Snapshot refreshed"));
        assert!(!ledger.exists());
        assert!(!paths.user().exists());
        assert!(!paths.project().exists());
    }

    #[test]
    fn terminal_loop_failed_refresh_keeps_the_active_config_candidate() {
        let root = terminal_test_root("snapshot-refresh-config-failure");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        std::fs::create_dir_all(&root).expect("create refresh failure fixture");
        std::fs::write(
            paths.user(),
            "schema_version = 1\n[provider]\nmodel = \"before-refresh\"\n",
        )
        .expect("write initial refresh config");
        let invalid = b"schema_version = 1\n[model_presets.broken]\nprovider = \"simulator\"\n";
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("initial view");
        let user_path = paths.user().to_path_buf();
        let mut events = VecDeque::from([
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_snapshot_refresh(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            &ledger,
            Viewport::new(80, 24).expect("refresh failure viewport"),
            move || {
                let event = events.pop_front().expect("bounded refresh failure events");
                if matches!(&event, Event::Key(key) if key.code == KeyCode::F(6)) {
                    std::fs::write(&user_path, invalid).expect("write invalid refresh config");
                }
                Ok(event)
            },
        )
        .expect("failed refresh loop");
        let output = String::from_utf8(output).expect("refresh failure VT output");

        assert!(output.contains("Snapshot refresh failed; showing previous snapshot"));
        assert!(output.contains("model before-refresh"));
        assert!(config.status().ready);
        assert_eq!(
            config
                .get_effective("provider.model")
                .expect("active model")
                .expect("model exists")
                .value,
            ConfigValue::String("before-refresh".to_owned())
        );
        assert_eq!(
            std::fs::read(paths.user()).expect("read invalid external config"),
            invalid
        );
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove refresh failure fixture");
    }

    #[test]
    fn terminal_loop_blocks_on_events_resizes_and_restores() {
        let root = std::env::temp_dir().join(format!(
            "greentyper-terminal-loop-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let ledger = root.join("runtime.ledger");
        let mut config = ConfigRuntime::open(
            ConfigPaths::new(root.join("user.toml"), root.join("project.toml")),
            ConfigDocument::empty(),
        )
        .expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("view");
        let mode = FakeTerminalMode::default();
        let observed = mode.clone();
        let reads = Rc::new(Cell::new(0));
        let observed_reads = Rc::clone(&reads);
        let mut events = VecDeque::from([
            Event::Resize(40, 12),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ]);

        let output = run_terminal_loop(Vec::new(), mode, &mut config, &view, 80, 24, move || {
            observed_reads.set(observed_reads.get() + 1);
            Ok(events.pop_front().expect("bounded event sequence"))
        })
        .expect("terminal loop");

        assert_eq!(reads.get(), 2);
        assert_eq!(observed.enabled.get(), 1);
        assert_eq!(observed.disabled.get(), 1);
        assert!(output.starts_with(ENTER_TERMINAL));
        assert!(output.windows(7).any(|bytes| bytes == b"\x1b[2J\x1b[H"));
        assert!(output.ends_with(LEAVE_TERMINAL));
        assert!(!ledger.exists());
    }

    fn terminal_test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "greentyper-terminal-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn write_on_demand_discovery_config(path: &Path, credential: &str, base_url: &str) {
        std::fs::write(
            path,
            format!(
                r#"schema_version = 1

[provider]
profile = "edge"
model = "gpt-5.6-sol"

[providers.edge]
template = "openai"
credential = "{credential}"
base_url = "{base_url}"
dialects = ["responses"]

[providers.edge.routes]
responses = "/responses"
models = "/models"

[providers.edge.catalog]
mode = "template_and_discovery"

[providers.edge.pricing]
source = "unknown"
"#
            ),
        )
        .expect("write on-demand discovery Config");
    }

    fn type_terminal_config_text(
        session: &mut TerminalSession,
        config: &mut ConfigRuntime,
        value: &str,
    ) {
        for character in value.chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(config))
                .expect("type terminal Config value");
        }
    }

    fn commit_terminal_config_text(config: &mut ConfigRuntime, query: &str, value: &str) {
        let mut session = TerminalSession::new(query, 80, 24).expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("open Config text editor");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
        type_terminal_config_text(&mut session, config, value);
        assert_eq!(
            session
                .config_text
                .as_ref()
                .map(|input| input.value.as_str()),
            Some(value),
            "failed to buffer {query}: {:?}",
            session.notice
        );
        assert!(
            session.controller.has_unsaved_config_draft()
                || session
                    .config_text
                    .as_ref()
                    .is_some_and(|input| input.pending),
            "failed to stage {query}: {:?}",
            session.notice
        );
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("preview Config text value");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config draft validated"),
            "failed to preview {query}"
        );
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("commit Config text value");
        assert!(session.controller.is_slash_panel());
    }

    fn commit_existing_object_config_text(config: &mut ConfigRuntime, query: &str, value: &str) {
        let mut session = TerminalSession::new(query, 80, 24).expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("open Config Object selector");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigObject);
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("open selected Config Object field");
        assert_eq!(session.input_context(), TerminalInputContext::ConfigText);
        type_terminal_config_text(&mut session, config, value);
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("preview Config Object field");
        assert_eq!(
            session.notice.as_deref(),
            Some("Config draft validated"),
            "failed to preview {query}"
        );
        session
            .handle(TerminalInputEvent::Enter, Some(config))
            .expect("commit Config Object field");
        assert!(session.controller.is_slash_panel());
    }

    fn write_terminal_provider_config(paths: &ConfigPaths) {
        std::fs::create_dir_all(paths.user().parent().expect("provider config parent"))
            .expect("create provider config directory");
        std::fs::write(
            paths.user(),
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
        .expect("write provider config");
    }

    fn write_terminal_manual_pricing_provider_config(paths: &ConfigPaths) {
        std::fs::create_dir_all(paths.user().parent().expect("provider config parent"))
            .expect("create provider config directory");
        std::fs::write(
            paths.user(),
            r#"schema_version = 1

[providers.edge]
template = "openai"
credential = "synthetic-edge-credential-reference"

[providers.edge.pricing]
source = "manual"
"#,
        )
        .expect("write manual-pricing provider config");
    }

    fn config_string_target(config: &ConfigRuntime, path: &str) -> Option<String> {
        let field = config
            .inspect_field(ConfigScope::User, path)
            .expect("inspect config field");
        let ConfigFieldContents::Value { target, .. } = field.contents else {
            panic!("expected value field")
        };
        match target {
            Some(ConfigValue::String(value)) => Some(value),
            None => None,
            Some(_) => panic!("expected string value"),
        }
    }
}
