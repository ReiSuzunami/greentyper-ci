use super::*;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell as RatatuiCell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::io::{BufWriter, Write};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

const TERMINAL_FIXTURE_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/bench/terminal/v1/render-matrix.json"
));

pub(super) fn catalog_entry() -> serde_json::Value {
    serde_json::json!({
        "id": "terminal",
        "version": 1,
        "implementations": ["direct-vt", "ratatui-crossterm"],
        "workloads": [{"id": "render-matrix", "version": 1}],
        "purpose": "candidate evidence; not a terminal selection"
    })
}

pub(super) fn target(implementation: &str, workload: &str) -> AppResult<Box<dyn BenchmarkTarget>> {
    if workload != "render-matrix" {
        return Err(cli_error(format!(
            "benchmark workload terminal/{workload} is not compiled into this runner"
        )));
    }
    let engine = match implementation {
        "direct-vt" => TerminalEngine::DirectVt,
        "ratatui-crossterm" => TerminalEngine::RatatuiCrossterm,
        _ => {
            return Err(cli_error(format!(
                "benchmark implementation terminal/{implementation} is not compiled into this runner"
            )));
        }
    };
    let fixture: TerminalFixture = serde_json::from_str(TERMINAL_FIXTURE_JSON)?;
    validate_fixture(&fixture)?;
    Ok(Box::new(TerminalTarget { engine, fixture }))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct TerminalFixture {
    schema_version: u16,
    comparison_id: String,
    workload_id: String,
    workload_version: u16,
    screen_sizes: Vec<[u16; 2]>,
    stream_updates: u16,
    expected_frames: u16,
    expected_digest: String,
}

fn validate_fixture(fixture: &TerminalFixture) -> AppResult<()> {
    SchemaKind::DeterministicFixture.require_current(fixture.schema_version)?;
    if fixture.comparison_id != "terminal"
        || fixture.workload_id != "render-matrix"
        || fixture.workload_version != 1
        || fixture.screen_sizes != [[40, 12], [80, 24], [160, 50]]
        || fixture.stream_updates != 4
        || fixture.expected_frames != 27
        || fixture.expected_digest.len() != 64
        || !fixture
            .expected_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(cli_error("terminal benchmark fixture is invalid"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalEngine {
    DirectVt,
    RatatuiCrossterm,
}

impl TerminalEngine {
    const fn implementation(self) -> &'static str {
        match self {
            Self::DirectVt => "direct-vt",
            Self::RatatuiCrossterm => "ratatui-crossterm",
        }
    }

    const fn dependencies(self) -> &'static str {
        match self {
            Self::DirectVt => {
                "candidate=direct-vt;feature=bench-terminal;crossterm=0.29.0;ratatui=0.30.2[crossterm_0_29];unicode-width=0.2.2;vt100=0.16.2"
            }
            Self::RatatuiCrossterm => {
                "candidate=ratatui-crossterm;feature=bench-terminal;crossterm=0.29.0;ratatui=0.30.2[crossterm_0_29];unicode-width=0.2.2;vt100=0.16.2"
            }
        }
    }
}

struct TerminalTarget {
    engine: TerminalEngine,
    fixture: TerminalFixture,
}

impl BenchmarkTarget for TerminalTarget {
    fn descriptor(&self) -> BenchmarkDescriptor {
        BenchmarkDescriptor {
            comparison_id: "terminal",
            comparison_version: 1,
            implementation: self.engine.implementation(),
            implementation_revision: "1",
            dependencies: self.engine.dependencies(),
            workload_id: "render-matrix",
            workload_version: self.fixture.workload_version,
            input_shape: "40x12, 80x24, 160x50; baseline, no-op, status, four stream updates, Slash Panel, clear",
            unit: "verified terminal frames",
            boundary: "construct backend, render and resize canonical grids, replay ANSI, verify every cell",
            process_mode: "in-process",
            fixture_bytes: TERMINAL_FIXTURE_JSON.as_bytes(),
        }
    }

    fn run_once(&mut self) -> AppResult<BenchmarkObservation> {
        run_render_matrix(self.engine, &self.fixture)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FramePhase {
    Baseline,
    Status,
    Stream(u16),
    SlashPanel,
    Cleared,
}

impl FramePhase {
    fn label(self) -> String {
        match self {
            Self::Baseline => "baseline".into(),
            Self::Status => "status".into(),
            Self::Stream(index) => format!("stream-{index}"),
            Self::SlashPanel => "slash-panel".into(),
            Self::Cleared => "cleared".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CellStyle {
    #[default]
    Plain,
    Header,
    Accent,
    Dim,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridCell {
    symbol: char,
    style: CellStyle,
    continuation: bool,
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            symbol: ' ',
            style: CellStyle::Plain,
            continuation: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Grid {
    width: u16,
    height: u16,
    cells: Vec<GridCell>,
}

impl Grid {
    fn blank(width: u16, height: u16) -> AppResult<Self> {
        if width == 0 || height == 0 {
            return Err(cli_error("terminal grid dimensions must be non-zero"));
        }
        let cell_count = usize::from(width)
            .checked_mul(usize::from(height))
            .ok_or_else(|| cli_error("terminal grid dimensions overflow"))?;
        Ok(Self {
            width,
            height,
            cells: vec![GridCell::default(); cell_count],
        })
    }

    fn index(&self, x: u16, y: u16) -> usize {
        usize::from(y) * usize::from(self.width) + usize::from(x)
    }

    fn cell(&self, x: u16, y: u16) -> GridCell {
        self.cells[self.index(x, y)]
    }

    fn put_text(&mut self, mut x: u16, y: u16, text: &str, style: CellStyle) -> AppResult<()> {
        if y >= self.height {
            return Ok(());
        }
        for symbol in text.chars() {
            let display_width = symbol
                .width()
                .ok_or_else(|| cli_error("terminal fixture contains a zero-width character"))?;
            if !(1..=2).contains(&display_width) {
                return Err(cli_error(
                    "terminal fixture contains an unsupported glyph width",
                ));
            }
            let display_width = u16::try_from(display_width)?;
            if x.checked_add(display_width)
                .is_none_or(|right| right > self.width)
            {
                break;
            }
            let index = self.index(x, y);
            let cell_style = if symbol == ' ' {
                CellStyle::Plain
            } else {
                style
            };
            self.cells[index] = GridCell {
                symbol,
                style: cell_style,
                continuation: false,
            };
            if display_width == 2 {
                self.cells[index + 1] = GridCell {
                    symbol: ' ',
                    style: CellStyle::Plain,
                    continuation: true,
                };
            }
            x += display_width;
        }
        Ok(())
    }

    fn changed_cells(&self, previous: Option<&Self>) -> AppResult<u64> {
        match previous {
            Some(previous) if previous.width == self.width && previous.height == self.height => {
                Ok(u64::try_from(
                    self.cells
                        .iter()
                        .zip(&previous.cells)
                        .filter(|(current, previous)| current != previous)
                        .count(),
                )?)
            }
            Some(_) => Err(cli_error("terminal grid diff dimensions do not match")),
            None => Ok(u64::try_from(
                self.cells
                    .iter()
                    .filter(|cell| **cell != GridCell::default())
                    .count(),
            )?),
        }
    }
}

fn build_frame(width: u16, height: u16, phase: FramePhase) -> AppResult<Grid> {
    let mut grid = Grid::blank(width, height)?;
    grid.put_text(0, 0, "GreenTyper 双", CellStyle::Header)?;
    let thread_line = if width < 60 {
        "thread main | agent 1"
    } else {
        "thread main | agent 1 | model gpt | effort high"
    };
    grid.put_text(0, 2, thread_line, CellStyle::Dim)?;
    grid.put_text(
        0,
        4,
        "You: Optimize the old Windows laptop.",
        CellStyle::Plain,
    )?;

    let response = match phase {
        FramePhase::Baseline | FramePhase::Status | FramePhase::Cleared => "Agent:",
        FramePhase::Stream(1) => "Agent: Fast,",
        FramePhase::Stream(2) => "Agent: Fast, bounded,",
        FramePhase::Stream(3) => "Agent: Fast, bounded, deterministic,",
        FramePhase::Stream(_) | FramePhase::SlashPanel => {
            "Agent: Fast, bounded, deterministic, and durable."
        }
    };
    grid.put_text(0, 6, response, CellStyle::Accent)?;

    if phase == FramePhase::SlashPanel {
        grid.put_text(0, 8, "> /config pro", CellStyle::Plain)?;
        grid.put_text(2, 9, "/config provider", CellStyle::Header)?;
        grid.put_text(2, 10, "/config provider-url", CellStyle::Plain)?;
        if height > 13 {
            grid.put_text(2, 11, "/config model", CellStyle::Plain)?;
        }
    }

    let status = match (width < 60, phase) {
        (true, FramePhase::Baseline) => "ctx 35% | $0.12 | cache 82%",
        (true, _) => "ctx 36% | $0.13 | cache 84%",
        (false, FramePhase::Baseline) => {
            "ctx 35% | cost $0.12 | cache read 82% | write unknown | 1h 18k"
        }
        (false, _) => "ctx 36% | cost $0.13 | cache read 84% | write unknown | 1h 19k",
    };
    grid.put_text(0, height - 1, status, CellStyle::Dim)?;
    Ok(grid)
}

#[derive(Default)]
struct MatrixMetrics {
    ansi_bytes: u64,
    changed_cells: u64,
    draw_calls: u64,
    logical_frames: u64,
    no_op_bytes: u64,
    replay_ns: u64,
    render_ns: u64,
    resize_bytes: u64,
    resize_calls: u64,
    resize_ns: u64,
    skipped_noop_frames: u64,
    terminal_write_calls: u64,
    verified_cells: u64,
    wide_cells: u64,
}

fn run_render_matrix(
    engine: TerminalEngine,
    fixture: &TerminalFixture,
) -> AppResult<BenchmarkObservation> {
    let [initial_width, initial_height] = fixture.screen_sizes[0];
    let setup_started = Instant::now();
    let mut renderer = Renderer::new(engine, initial_width, initial_height)?;
    let mut parser = vt100::Parser::new(initial_height, initial_width, 0);
    let setup_ns = elapsed_ns(setup_started)?;
    let mut metrics = MatrixMetrics::default();
    let mut digest = Sha256::new();
    let mut previous_grid = None;

    for (size_index, [width, height]) in fixture.screen_sizes.iter().copied().enumerate() {
        if size_index > 0 {
            let emission = renderer.resize(width, height)?;
            metrics.resize_calls += 1;
            metrics.resize_bytes = metrics
                .resize_bytes
                .checked_add(u64::try_from(emission.bytes.len())?)
                .ok_or_else(|| cli_error("terminal resize byte count overflow"))?;
            metrics.ansi_bytes = metrics
                .ansi_bytes
                .checked_add(u64::try_from(emission.bytes.len())?)
                .ok_or_else(|| cli_error("terminal byte count overflow"))?;
            metrics.terminal_write_calls = metrics
                .terminal_write_calls
                .checked_add(emission.write_calls)
                .ok_or_else(|| cli_error("terminal write count overflow"))?;
            metrics.resize_ns = metrics
                .resize_ns
                .checked_add(emission.elapsed_ns)
                .ok_or_else(|| cli_error("terminal resize timing overflow"))?;
            parser.screen_mut().set_size(height, width);
            let replay_started = Instant::now();
            parser.process(&emission.bytes);
            metrics.replay_ns = metrics
                .replay_ns
                .checked_add(elapsed_ns(replay_started)?)
                .ok_or_else(|| cli_error("terminal replay timing overflow"))?;
            previous_grid = None;
        }

        let mut phases = vec![
            FramePhase::Baseline,
            FramePhase::Baseline,
            FramePhase::Status,
        ];
        phases.extend((1..=fixture.stream_updates).map(FramePhase::Stream));
        phases.push(FramePhase::SlashPanel);
        phases.push(FramePhase::Cleared);

        for (phase_index, phase) in phases.into_iter().enumerate() {
            let grid = build_frame(width, height, phase)?;
            metrics.changed_cells = metrics
                .changed_cells
                .checked_add(grid.changed_cells(previous_grid.as_ref())?)
                .ok_or_else(|| cli_error("terminal changed-cell count overflow"))?;
            let emission = renderer.draw(&grid)?;
            metrics.logical_frames += 1;
            metrics.draw_calls = metrics
                .draw_calls
                .checked_add(u64::from(!emission.skipped_noop))
                .ok_or_else(|| cli_error("terminal draw count overflow"))?;
            metrics.skipped_noop_frames = metrics
                .skipped_noop_frames
                .checked_add(u64::from(emission.skipped_noop))
                .ok_or_else(|| cli_error("terminal no-op count overflow"))?;
            if phase_index == 1 {
                metrics.no_op_bytes = metrics
                    .no_op_bytes
                    .checked_add(u64::try_from(emission.bytes.len())?)
                    .ok_or_else(|| cli_error("terminal no-op byte count overflow"))?;
            }
            metrics.ansi_bytes = metrics
                .ansi_bytes
                .checked_add(u64::try_from(emission.bytes.len())?)
                .ok_or_else(|| cli_error("terminal byte count overflow"))?;
            metrics.terminal_write_calls = metrics
                .terminal_write_calls
                .checked_add(emission.write_calls)
                .ok_or_else(|| cli_error("terminal write count overflow"))?;
            metrics.render_ns = metrics
                .render_ns
                .checked_add(emission.elapsed_ns)
                .ok_or_else(|| cli_error("terminal render timing overflow"))?;

            let replay_started = Instant::now();
            parser.process(&emission.bytes);
            if let Err(error) = verify_and_hash_screen(
                parser.screen(),
                &grid,
                size_index,
                phase_index,
                phase,
                &mut digest,
                &mut metrics,
            ) {
                return Err(cli_error(format!(
                    "{error}; ANSI bytes: {:?}",
                    String::from_utf8_lossy(&emission.bytes)
                )));
            }
            metrics.replay_ns = metrics
                .replay_ns
                .checked_add(elapsed_ns(replay_started)?)
                .ok_or_else(|| cli_error("terminal replay timing overflow"))?;
            previous_grid = Some(grid);
        }
    }

    if metrics.logical_frames != u64::from(fixture.expected_frames)
        || metrics.no_op_bytes != 0
        || metrics.skipped_noop_frames != u64::try_from(fixture.screen_sizes.len())?
    {
        return Err(cli_error(
            "terminal benchmark frame or no-op invariant failed",
        ));
    }
    let output_digest = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if output_digest != fixture.expected_digest {
        return Err(cli_error(format!(
            "terminal benchmark output digest {output_digest} differs from fixture {}",
            fixture.expected_digest
        )));
    }

    let [final_width, final_height] = fixture.screen_sizes[fixture.screen_sizes.len() - 1];
    Ok(BenchmarkObservation {
        operation_units: metrics.logical_frames,
        output_digest,
        timings_ns: BTreeMap::from([
            ("render".into(), metrics.render_ns),
            ("replay_and_verify".into(), metrics.replay_ns),
            ("resize".into(), metrics.resize_ns),
            ("setup".into(), setup_ns),
        ]),
        gauges: BTreeMap::from([
            ("ansi_bytes".into(), metrics.ansi_bytes),
            ("changed_cells".into(), metrics.changed_cells),
            ("draw_calls".into(), metrics.draw_calls),
            ("final_height".into(), u64::from(final_height)),
            ("final_width".into(), u64::from(final_width)),
            ("logical_frames".into(), metrics.logical_frames),
            ("no_op_bytes".into(), metrics.no_op_bytes),
            ("resize_bytes".into(), metrics.resize_bytes),
            ("resize_calls".into(), metrics.resize_calls),
            ("skipped_noop_frames".into(), metrics.skipped_noop_frames),
            ("terminal_write_calls".into(), metrics.terminal_write_calls),
            ("verified_cells".into(), metrics.verified_cells),
            ("wide_cells".into(), metrics.wide_cells),
        ]),
    })
}

fn elapsed_ns(started: Instant) -> AppResult<u64> {
    Ok(u64::try_from(started.elapsed().as_nanos())?)
}

struct Emission {
    bytes: Vec<u8>,
    write_calls: u64,
    elapsed_ns: u64,
    skipped_noop: bool,
}

impl Emission {
    fn skipped() -> Self {
        Self {
            bytes: Vec::new(),
            write_calls: 0,
            elapsed_ns: 0,
            skipped_noop: true,
        }
    }
}

enum Renderer {
    Direct(DirectRenderer),
    Ratatui(RatatuiRenderer),
}

impl Renderer {
    fn new(engine: TerminalEngine, width: u16, height: u16) -> AppResult<Self> {
        match engine {
            TerminalEngine::DirectVt => Ok(Self::Direct(DirectRenderer::new(width, height)?)),
            TerminalEngine::RatatuiCrossterm => {
                Ok(Self::Ratatui(RatatuiRenderer::new(width, height)?))
            }
        }
    }

    fn resize(&mut self, width: u16, height: u16) -> AppResult<Emission> {
        match self {
            Self::Direct(renderer) => renderer.resize(width, height),
            Self::Ratatui(renderer) => renderer.resize(width, height),
        }
    }

    fn draw(&mut self, grid: &Grid) -> AppResult<Emission> {
        match self {
            Self::Direct(renderer) => renderer.draw(grid),
            Self::Ratatui(renderer) => renderer.draw(grid),
        }
    }
}

struct DirectRenderer {
    previous: Grid,
    has_frame: bool,
}

impl DirectRenderer {
    fn new(width: u16, height: u16) -> AppResult<Self> {
        Ok(Self {
            previous: Grid::blank(width, height)?,
            has_frame: false,
        })
    }

    fn resize(&mut self, width: u16, height: u16) -> AppResult<Emission> {
        let started = Instant::now();
        self.previous = Grid::blank(width, height)?;
        self.has_frame = false;
        let bytes = b"\x1b[2J\x1b[H".to_vec();
        Ok(Emission {
            bytes,
            write_calls: 1,
            elapsed_ns: elapsed_ns(started)?,
            skipped_noop: false,
        })
    }

    fn draw(&mut self, grid: &Grid) -> AppResult<Emission> {
        if self.has_frame && self.previous == *grid {
            return Ok(Emission::skipped());
        }
        if self.previous.width != grid.width || self.previous.height != grid.height {
            return Err(cli_error("direct VT renderer dimensions do not match"));
        }
        let started = Instant::now();
        let bytes = encode_grid_diff(&self.previous, grid)?;
        self.previous = grid.clone();
        self.has_frame = true;
        Ok(Emission {
            write_calls: u64::from(!bytes.is_empty()),
            bytes,
            elapsed_ns: elapsed_ns(started)?,
            skipped_noop: false,
        })
    }
}

fn encode_grid_diff(previous: &Grid, current: &Grid) -> AppResult<Vec<u8>> {
    if previous.width != current.width || previous.height != current.height {
        return Err(cli_error("direct VT diff dimensions do not match"));
    }
    let mut output = Vec::new();
    for y in 0..current.height {
        let mut changed = vec![false; usize::from(current.width)];
        for x in 0..current.width {
            if previous.cell(x, y) != current.cell(x, y) {
                changed[usize::from(x)] = true;
                if (current.cell(x, y).continuation || previous.cell(x, y).continuation) && x > 0 {
                    changed[usize::from(x - 1)] = true;
                }
                if (current.cell(x, y).symbol.width() == Some(2)
                    || previous.cell(x, y).symbol.width() == Some(2))
                    && x + 1 < current.width
                {
                    changed[usize::from(x + 1)] = true;
                }
            }
        }
        let mut x = 0;
        while x < current.width {
            if !changed[usize::from(x)] {
                x += 1;
                continue;
            }
            let start = x;
            while x < current.width && changed[usize::from(x)] {
                x += 1;
            }
            let end = x;
            write!(output, "\x1b[{};{}H", y + 1, start + 1)?;
            let mut active_style = None;
            for column in start..end {
                let cell = current.cell(column, y);
                if cell.continuation {
                    continue;
                }
                if active_style != Some(cell.style) {
                    output.extend_from_slice(direct_style(cell.style));
                    active_style = Some(cell.style);
                }
                write!(output, "{}", cell.symbol)?;
            }
            output.extend_from_slice(b"\x1b[0m");
        }
    }
    Ok(output)
}

const fn direct_style(style: CellStyle) -> &'static [u8] {
    match style {
        CellStyle::Plain => b"\x1b[0m",
        CellStyle::Header => b"\x1b[0;1;38;5;6m",
        CellStyle::Accent => b"\x1b[0;38;5;2m",
        CellStyle::Dim => b"\x1b[0;2;38;5;8m",
    }
}

#[derive(Clone, Default)]
struct CaptureWriter {
    state: Rc<RefCell<CaptureState>>,
}

#[derive(Default)]
struct CaptureState {
    bytes: Vec<u8>,
    write_calls: u64,
}

impl CaptureWriter {
    fn reset(&self) {
        *self.state.borrow_mut() = CaptureState::default();
    }

    fn take(&self) -> CaptureState {
        std::mem::take(&mut *self.state.borrow_mut())
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut state = self.state.borrow_mut();
        state.bytes.extend_from_slice(bytes);
        state.write_calls = state.write_calls.saturating_add(1);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FixedSizeBackend<B> {
    inner: B,
    size: Size,
}

impl<B> FixedSizeBackend<B> {
    const fn new(inner: B, size: Size) -> Self {
        Self { inner, size }
    }

    fn set_size(&mut self, size: Size) {
        self.size = size;
    }
}

impl<B: Backend> Backend for FixedSizeBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a RatatuiCell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

type CapturedBackend = FixedSizeBackend<CrosstermBackend<BufWriter<CaptureWriter>>>;
type CapturedTerminal = Terminal<CapturedBackend>;

struct RatatuiRenderer {
    terminal: CapturedTerminal,
    capture: CaptureWriter,
    previous: Option<Grid>,
    _color_output_guard: ColorOutputGuard,
}

static COLOR_OUTPUT_LOCK: Mutex<()> = Mutex::new(());

struct ColorOutputGuard {
    restore_enabled: bool,
    _lock: MutexGuard<'static, ()>,
}

impl ColorOutputGuard {
    fn force_enabled() -> Self {
        let lock = COLOR_OUTPUT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::force_enabled_with_lock(lock)
    }

    fn force_enabled_with_lock(lock: MutexGuard<'static, ()>) -> Self {
        let restore_enabled = !crossterm::style::Colored::ansi_color_disabled_memoized();
        crossterm::style::force_color_output(true);
        Self {
            restore_enabled,
            _lock: lock,
        }
    }
}

impl Drop for ColorOutputGuard {
    fn drop(&mut self) {
        crossterm::style::force_color_output(self.restore_enabled);
    }
}

impl RatatuiRenderer {
    fn new(width: u16, height: u16) -> AppResult<Self> {
        let color_output_guard = ColorOutputGuard::force_enabled();
        let capture = CaptureWriter::default();
        let writer = BufWriter::with_capacity(16 * 1024, capture.clone());
        let backend =
            FixedSizeBackend::new(CrosstermBackend::new(writer), Size::new(width, height));
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )?;
        Ok(Self {
            terminal,
            capture,
            previous: None,
            _color_output_guard: color_output_guard,
        })
    }

    fn resize(&mut self, width: u16, height: u16) -> AppResult<Emission> {
        self.capture.reset();
        let started = Instant::now();
        self.terminal
            .backend_mut()
            .set_size(Size::new(width, height));
        self.terminal.resize(Rect::new(0, 0, width, height))?;
        self.terminal.backend_mut().flush()?;
        let elapsed_ns = elapsed_ns(started)?;
        self.previous = None;
        let captured = self.capture.take();
        Ok(Emission {
            bytes: captured.bytes,
            write_calls: captured.write_calls,
            elapsed_ns,
            skipped_noop: false,
        })
    }

    fn draw(&mut self, grid: &Grid) -> AppResult<Emission> {
        if self.previous.as_ref() == Some(grid) {
            return Ok(Emission::skipped());
        }
        self.capture.reset();
        let started = Instant::now();
        self.terminal
            .draw(|frame| render_ratatui_grid(frame, grid))?;
        let elapsed_ns = elapsed_ns(started)?;
        self.previous = Some(grid.clone());
        let captured = self.capture.take();
        Ok(Emission {
            bytes: captured.bytes,
            write_calls: captured.write_calls,
            elapsed_ns,
            skipped_noop: false,
        })
    }
}

fn render_ratatui_grid(frame: &mut ratatui::Frame<'_>, grid: &Grid) {
    let buffer = frame.buffer_mut();
    for y in 0..grid.height {
        for x in 0..grid.width {
            let cell = grid.cell(x, y);
            if cell.continuation || cell.symbol == ' ' {
                continue;
            }
            buffer.set_string(x, y, cell.symbol.to_string(), ratatui_style(cell.style));
        }
    }
}

fn ratatui_style(style: CellStyle) -> Style {
    match style {
        CellStyle::Plain => Style::default(),
        CellStyle::Header => Style::default()
            .fg(Color::Indexed(6))
            .add_modifier(Modifier::BOLD),
        CellStyle::Accent => Style::default().fg(Color::Indexed(2)),
        CellStyle::Dim => Style::default()
            .fg(Color::Indexed(8))
            .add_modifier(Modifier::DIM),
    }
}

fn verify_and_hash_screen(
    screen: &vt100::Screen,
    grid: &Grid,
    size_index: usize,
    phase_index: usize,
    phase: FramePhase,
    digest: &mut Sha256,
    metrics: &mut MatrixMetrics,
) -> AppResult<()> {
    if screen.size() != (grid.height, grid.width) {
        return Err(cli_error(
            "terminal replay dimensions differ from canonical grid",
        ));
    }
    digest.update(u64::try_from(size_index)?.to_le_bytes());
    digest.update(u64::try_from(phase_index)?.to_le_bytes());
    digest.update(phase.label().as_bytes());
    digest.update(grid.width.to_le_bytes());
    digest.update(grid.height.to_le_bytes());

    for y in 0..grid.height {
        if screen.row_wrapped(y) {
            return Err(cli_error("terminal replay unexpectedly wrapped a row"));
        }
        for x in 0..grid.width {
            let expected = grid.cell(x, y);
            let observed = screen
                .cell(y, x)
                .ok_or_else(|| cli_error("terminal replay omitted a visible cell"))?;
            verify_cell(observed, expected, x, y)?;
            if expected.continuation {
                digest.update([0xff, style_tag(expected.style)]);
            } else {
                let symbol = if expected.symbol == ' ' {
                    ' '
                } else {
                    expected.symbol
                };
                let mut encoded = [0; 4];
                digest.update(symbol.encode_utf8(&mut encoded).as_bytes());
                digest.update([style_tag(expected.style)]);
                if expected.symbol.width() == Some(2) {
                    metrics.wide_cells = metrics
                        .wide_cells
                        .checked_add(1)
                        .ok_or_else(|| cli_error("terminal wide-cell count overflow"))?;
                }
            }
            metrics.verified_cells = metrics
                .verified_cells
                .checked_add(1)
                .ok_or_else(|| cli_error("terminal verified-cell count overflow"))?;
        }
    }
    Ok(())
}

fn verify_cell(observed: &vt100::Cell, expected: GridCell, x: u16, y: u16) -> AppResult<()> {
    if expected.continuation {
        if !observed.is_wide_continuation()
            || observed.has_contents()
            || observed.is_wide()
            || observed.bgcolor() != vt100::Color::Default
            || !style_matches(observed, expected.style)
            || observed.italic()
            || observed.underline()
            || observed.inverse()
        {
            return Err(cli_error(format!(
                "terminal replay cell {x},{y} is not the expected wide continuation"
            )));
        }
        return Ok(());
    }
    let observed_symbol = match observed.contents() {
        "" | " " => ' ',
        contents => {
            let mut symbols = contents.chars();
            let symbol = symbols
                .next()
                .ok_or_else(|| cli_error("terminal replay produced an empty symbol"))?;
            if symbols.next().is_some() {
                return Err(cli_error(format!(
                    "terminal replay cell {x},{y} contains multiple symbols"
                )));
            }
            symbol
        }
    };
    if observed_symbol != expected.symbol
        || observed.is_wide() != (expected.symbol.width() == Some(2))
        || observed.is_wide_continuation()
        || observed.bgcolor() != vt100::Color::Default
        || !style_matches(observed, expected.style)
        || observed.italic()
        || observed.underline()
        || observed.inverse()
    {
        return Err(cli_error(format!(
            "terminal replay cell {x},{y} differs from the canonical grid: observed={:?} fg={:?} bg={:?} bold={} dim={} wide={} continuation={}, expected={expected:?}",
            observed.contents(),
            observed.fgcolor(),
            observed.bgcolor(),
            observed.bold(),
            observed.dim(),
            observed.is_wide(),
            observed.is_wide_continuation(),
        )));
    }
    Ok(())
}

fn style_matches(cell: &vt100::Cell, style: CellStyle) -> bool {
    match style {
        CellStyle::Plain => cell.fgcolor() == vt100::Color::Default && !cell.bold() && !cell.dim(),
        CellStyle::Header => cell.fgcolor() == vt100::Color::Idx(6) && cell.bold() && !cell.dim(),
        CellStyle::Accent => cell.fgcolor() == vt100::Color::Idx(2) && !cell.bold() && !cell.dim(),
        CellStyle::Dim => cell.fgcolor() == vt100::Color::Idx(8) && !cell.bold() && cell.dim(),
    }
}

const fn style_tag(style: CellStyle) -> u8 {
    match style {
        CellStyle::Plain => 0,
        CellStyle::Header => 1,
        CellStyle::Accent => 2,
        CellStyle::Dim => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_fixture_shape_is_frozen() {
        let fixture: TerminalFixture =
            serde_json::from_str(TERMINAL_FIXTURE_JSON).expect("terminal fixture");
        validate_fixture(&fixture).expect("frozen fixture");
        let mut changed = fixture.clone();
        changed.screen_sizes[0] = [41, 12];
        assert!(validate_fixture(&changed).is_err());
        let mut changed = fixture.clone();
        changed.stream_updates = 5;
        assert!(validate_fixture(&changed).is_err());
        let mut changed = fixture.clone();
        changed.expected_digest = "A".repeat(64);
        assert!(validate_fixture(&changed).is_err());
    }

    #[test]
    fn canonical_grid_tracks_wide_cells_without_overlap() {
        let grid = build_frame(40, 12, FramePhase::SlashPanel).expect("grid");
        let marker = "GreenTyper ".chars().count() as u16;
        assert_eq!(grid.cell(marker, 0).symbol, '双');
        assert!(grid.cell(marker + 1, 0).continuation);
        assert_eq!(grid.cell(marker + 2, 0).symbol, ' ');
    }

    #[test]
    fn direct_and_ratatui_replay_the_same_matrix() {
        let fixture: TerminalFixture =
            serde_json::from_str(TERMINAL_FIXTURE_JSON).expect("terminal fixture");
        let direct = run_render_matrix(TerminalEngine::DirectVt, &fixture);
        let ratatui = run_render_matrix(TerminalEngine::RatatuiCrossterm, &fixture);
        let direct = direct.expect("direct matrix");
        let ratatui = ratatui.expect("ratatui matrix");
        assert_eq!(direct.output_digest, ratatui.output_digest);
        assert_eq!(direct.operation_units, u64::from(fixture.expected_frames));
        assert_eq!(direct.operation_units, ratatui.operation_units);
        assert_eq!(direct.gauges["no_op_bytes"], 0);
        assert_eq!(ratatui.gauges["no_op_bytes"], 0);
        assert_eq!(direct.gauges["skipped_noop_frames"], 3);
        assert_eq!(ratatui.gauges["skipped_noop_frames"], 3);
        assert_eq!(
            direct.gauges.keys().collect::<Vec<_>>(),
            ratatui.gauges.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            direct.timings_ns.keys().collect::<Vec<_>>(),
            ratatui.timings_ns.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn terminal_target_descriptor_is_explicit() {
        let direct_target = target("direct-vt", "render-matrix").expect("direct target");
        let descriptor = direct_target.descriptor();
        assert_eq!(descriptor.comparison_id, "terminal");
        assert_eq!(descriptor.implementation, "direct-vt");
        assert_eq!(descriptor.workload_id, "render-matrix");
        assert_eq!(descriptor.process_mode, "in-process");
        assert!(target("unknown", "render-matrix").is_err());
        assert!(target("direct-vt", "unknown").is_err());
    }

    #[test]
    fn color_output_guard_restores_the_exact_crossterm_state() {
        let original_enabled = !crossterm::style::Colored::ansi_color_disabled_memoized();
        for prior_enabled in [false, true] {
            let lock = COLOR_OUTPUT_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crossterm::style::force_color_output(prior_enabled);
            {
                let _guard = ColorOutputGuard::force_enabled_with_lock(lock);
                assert!(!crossterm::style::Colored::ansi_color_disabled_memoized());
            }
            assert_eq!(
                !crossterm::style::Colored::ansi_color_disabled_memoized(),
                prior_enabled
            );
        }
        crossterm::style::force_color_output(original_enabled);
    }

    #[test]
    fn fixed_size_backend_never_queries_the_host_terminal_size() {
        let configured = Size::new(123, 45);
        let mut backend =
            FixedSizeBackend::new(CrosstermBackend::new(Vec::<u8>::new()), configured);
        assert_eq!(backend.size().expect("synthetic terminal size"), configured);
        assert_eq!(
            backend
                .window_size()
                .expect("synthetic terminal window size"),
            WindowSize {
                columns_rows: configured,
                pixels: Size::new(0, 0),
            }
        );

        let mut renderer = RatatuiRenderer::new(40, 12).expect("fixed viewport renderer");
        renderer.resize(80, 24).expect("first fixed resize");
        renderer.resize(160, 50).expect("second fixed resize");
    }
}
