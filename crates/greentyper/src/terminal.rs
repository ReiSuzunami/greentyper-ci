//! Product terminal adapter.

use std::error::Error;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal as crossterm_terminal;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use greentyper_core::config::{
    ConfigEditorError, ConfigError, ConfigFieldContents, ConfigFieldInteraction, ConfigRuntime,
    ConfigRuntimeError, ConfigScope, ConfigValue, ConfigValueKind, MAX_COMMAND_QUERY_BYTES,
    MAX_CONFIG_ID_BYTES,
};
use greentyper_core::runtime::{RuntimeError, RuntimeKernel};
use greentyper_core::usage::{UsageError, UsageTimestamp};

use crate::credential_vault::PlatformCredentialVault;
use crate::presentation::{
    PresentationController, PresentationControllerError, PresentationError, PresentationLayoutView,
    PresentationSources, TuiViewModel, Viewport, ViewportError,
};
use crate::provider_connection::{ModelsHttpConnectionTester, ProviderConnectionTester};

pub(crate) fn require_interactive() -> Result<(), TerminalError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(TerminalError::NonInteractive);
    }
    Ok(())
}

pub(crate) fn run(ledger: &Path, config: &mut ConfigRuntime) -> Result<(), TerminalError> {
    let view = build_terminal_view(ledger, config, "/")?;
    let (width, height) = crossterm_terminal::size()?;
    let stdout = io::stdout();
    let _writer = run_terminal_loop(
        stdout.lock(),
        CrosstermTerminalMode,
        config,
        &view,
        width,
        height,
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
    let status = config.status();
    let resolved = config.config_layers()?.resolve()?;
    let model_presets = config.model_presets()?;
    let catalog_models = config.catalog_models()?;
    TuiViewModel::build(
        query,
        "",
        0,
        PresentationSources {
            runtime: &runtime,
            usage: Some(&usage),
            team: None,
            tools: None,
            config: &status,
            provider_profile: Some(resolved.provider_profile().value()),
            model: Some(resolved.provider_model().value()),
            context_pressure: None,
            model_presets: &model_presets,
            catalog_models: &catalog_models,
        },
    )
    .map_err(TerminalError::PresentationModel)
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
    EditConfigObjectId(char),
    BackspaceConfigObjectId,
    ClearConfigObjectId,
    SubmitConfigObjectId,
    MoveConfigObjectSelection(isize),
    ActivateConfigObject,
    MoveConfigField(isize),
    MoveConfigChoice(isize),
    TestProviderConnection,
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
    ConfigObjectId,
    ConfigObject,
    ConfigChoice,
    ConfigText,
    ConfigCredentialReference,
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
            TerminalInputEvent::TestProviderConnection => TerminalIntent::TestProviderConnection,
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
            | TerminalInputEvent::Ignore => TerminalIntent::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalLoopOutcome {
    Redraw,
    Resize(u16, u16),
    Quit,
    Noop,
}

struct TerminalSession {
    controller: PresentationController,
    input: TerminalInputState,
    viewport: Viewport,
    validated_config_choice: Option<String>,
    config_text: Option<ConfigTextInput>,
    validated_config_text: Option<String>,
    confirming_discard: bool,
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

    fn handle_with_connection_tester(
        &mut self,
        event: TerminalInputEvent,
        runtime: Option<&mut ConfigRuntime>,
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
                let runtime = runtime.ok_or(TerminalError::ConfigRuntimeRequired)?;
                match self.controller.activate(runtime, ConfigScope::User, None) {
                    Ok(()) => {
                        self.validated_config_choice = None;
                        self.clear_config_text();
                        self.notice = None;
                    }
                    Err(source) => self.notice = Some(presentation_notice(&source)),
                }
                Ok(TerminalLoopOutcome::Redraw)
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
                self.viewport = Viewport::new(width, height)?;
                Ok(TerminalLoopOutcome::Resize(width, height))
            }
            TerminalIntent::Quit if self.has_unsaved_config_input() => {
                self.notice = Some("Config draft must be committed or discarded".to_owned());
                Ok(TerminalLoopOutcome::Redraw)
            }
            TerminalIntent::Quit => Ok(TerminalLoopOutcome::Quit),
            TerminalIntent::None => Ok(TerminalLoopOutcome::Noop),
        }
    }

    fn input_context(&self) -> TerminalInputContext {
        if self.confirming_discard {
            TerminalInputContext::DiscardConfirmation
        } else if self.controller.is_slash_panel() {
            TerminalInputContext::SlashPanel
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
    let vault = PlatformCredentialVault;
    let mut tester = ModelsHttpConnectionTester::new(&vault);
    let viewport = Viewport::new(width, height)?;
    run_terminal_loop_with_connection_tester(
        writer,
        mode,
        config,
        view,
        viewport,
        &mut tester,
        read_event,
    )
}

fn run_terminal_loop_with_connection_tester<W, M, T, F>(
    writer: W,
    mode: M,
    config: &mut ConfigRuntime,
    view: &TuiViewModel,
    viewport: Viewport,
    tester: &mut T,
    mut read_event: F,
) -> Result<W, TerminalError>
where
    W: Write,
    M: TerminalMode,
    T: ProviderConnectionTester,
    F: FnMut() -> io::Result<Event>,
{
    let width = viewport.width();
    let height = viewport.height();
    let mut surface = TerminalSurface::enter(writer, mode)?;
    let mut renderer = DirectVtRenderer::new(width, height)?;
    let mut session = TerminalSession::new("/", width, height)?;

    let frame = session.frame(Some(config), view)?;
    surface.write_frame(&renderer.draw(&frame)?)?;

    loop {
        match session.handle_with_connection_tester(
            map_crossterm_event(read_event()?),
            Some(config),
            Some(tester),
        )? {
            TerminalLoopOutcome::Quit => break,
            TerminalLoopOutcome::Resize(width, height) => {
                surface.write_frame(&renderer.resize(width, height)?)?;
                let frame = session.frame(Some(config), view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::Redraw => {
                let frame = session.frame(Some(config), view)?;
                surface.write_frame(&renderer.draw(&frame)?)?;
            }
            TerminalLoopOutcome::Noop => {}
        }
    }

    Ok(surface.finish()?)
}

#[derive(Debug)]
pub(crate) enum TerminalError {
    NonInteractive,
    InvalidDimensions,
    DimensionMismatch,
    UnsupportedCellWidth,
    InvalidQuery,
    ConfigRuntimeRequired,
    Presentation(PresentationControllerError),
    PresentationModel(PresentationError),
    Viewport(ViewportError),
    Config(ConfigError),
    ConfigRuntime(ConfigRuntimeError),
    Runtime(RuntimeError),
    Usage(UsageError),
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
            Self::Presentation(source) => write!(formatter, "{source}"),
            Self::PresentationModel(source) => write!(formatter, "{source}"),
            Self::Viewport(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::ConfigRuntime(source) => write!(formatter, "{source}"),
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::Usage(source) => write!(formatter, "{source}"),
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
            Self::InvalidDimensions
            | Self::DimensionMismatch
            | Self::UnsupportedCellWidth
            | Self::InvalidQuery
            | Self::ConfigRuntimeRequired
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use greentyper_core::config::{
        ConfigDocument, ConfigEditorSession, ConfigFieldContents, ConfigObjectKind,
        ConfigObjectRef, ConfigPaths, ConfigRuntime, ConfigScope, ConfigValue,
        MAX_CONFIG_STRING_BYTES,
    };
    use greentyper_core::provider::{ProviderDialect, ProviderProfileSnapshot};
    use greentyper_core::usage::UsageWeekday;

    use crate::presentation::{PresentationScreenView, build_smoke_view};
    use crate::provider_connection::{
        ProviderConnectionFailureCategory, ProviderConnectionTestStatus, ProviderConnectionTester,
    };

    use super::{
        DirectVtRenderer, ENTER_TERMINAL, LEAVE_TERMINAL, TerminalFrame, TerminalInputContext,
        TerminalInputEvent, TerminalInputState, TerminalIntent, TerminalLoopOutcome, TerminalMode,
        TerminalSession, TerminalSurface, Viewport, build_terminal_view, map_crossterm_event,
        run_terminal_loop, run_terminal_loop_with_connection_tester,
    };

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

        let preset = config
            .model_preset("fast")
            .expect("resolve created Model Preset");
        assert_eq!(preset.provider, "edge");
        assert_eq!(preset.model, "fixture-model");
        assert_eq!(preset.dialect, ProviderDialect::Responses);
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
        let config = ConfigRuntime::open(
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
        assert!(!ledger.exists());
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
