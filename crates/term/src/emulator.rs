//! The terminal emulator core: `alacritty_terminal`'s `Term` + vte's ANSI
//! `Processor` wrapped as a pure state machine.
//!
//! Bytes in ([`Emulator::feed`] — the decoded `SubscribeTerminal` Data frames),
//! grid snapshots out ([`Emulator::line`], [`Emulator::cursor`]). No I/O, no
//! timers, no gpui: the panel owns RPC and scheduling, the view owns paint, and
//! the engine feeds the same type from its PTY pump. That split makes the whole
//! escape-sequence surface unit-testable with scripted byte strings.
//!
//! Selection lives here too ([`Emulator::start_selection`] and friends) rather
//! than in the panel, because `Term` is what knows how to keep anchors on their
//! text as output scrolls the grid underneath them.
//!
//! API notes for the pinned `alacritty_terminal 0.26` / `vte 0.15`:
//! - `Processor::advance` consumes a byte slice; `Term` implements the
//!   `vte::ansi::Handler` trait directly, so no event-loop machinery is needed.
//! - `Term::new` takes any `grid::Dimensions` impl — [`GridSize`] here (the
//!   crate's own `TermSize` lives in a `term::test` helper module).
//! - Query responses (DSR/DA/…) surface as `Event::PtyWrite` on the listener;
//!   [`Emulator::feed`] returns them so the panel can write them back.
//!
//! Grid coordinates and selection granularity (`Point`, `Side`, `SelectionType`)
//! are re-exported at the crate root so the panel and the view speak the
//! emulator's vocabulary without depending on `alacritty_terminal` directly — the
//! same seam [`CellColor`] draws for colors.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb as AnsiRgb,
};

use crate::markers::{
    BlockStatus, CommandBlock, CommandMarker, CommandSource, CommandTracker, Mark, MarkerKind,
    block_after, block_before, scan_markers,
};

/// Scrollback history kept client-side (lines) when the caller does not pick
/// a capacity. The engine's replay window is bounded separately (1 MiB); this
/// only caps what stays scrollable in the UI. [`Emulator::with_scrollback`]
/// is the setting-driven form — this constant is just the default.
pub const SCROLLBACK_LINES: usize = 10_000;

/// Viewport dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A cell's paint color, decoupled from the palette: the view resolves these
/// against the theme (default fg/bg, 256-color index, or direct RGB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    /// Default foreground.
    Foreground,
    /// Default background.
    Background,
    /// Indexed color: 0-15 ANSI, 16-231 color cube, 232-255 grayscale ramp.
    Indexed(u8),
    /// Direct 24-bit color.
    Rgb(u8, u8, u8),
}

fn map_color(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Spec(AnsiRgb { r, g, b }) => CellColor::Rgb(r, g, b),
        AnsiColor::Indexed(ix) => CellColor::Indexed(ix),
        AnsiColor::Named(named) => {
            let ix = named as usize;
            if ix < 16 {
                return CellColor::Indexed(ix as u8);
            }
            match named {
                NamedColor::Background => CellColor::Background,
                // Dim named colors fold onto their base index; the DIM flag
                // still travels on the cell for paint-time dimming.
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    CellColor::Indexed((ix - NamedColor::DimBlack as usize) as u8)
                }
                _ => CellColor::Foreground,
            }
        }
    }
}

/// One rendered cell: char + colors + the flags paint cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSnapshot {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// A double-width char (occupies this cell plus the next spacer cell).
    pub wide: bool,
    /// The spacer half of a wide char — never shaped, only background-painted.
    pub wide_spacer: bool,
    /// Inside the active selection: the view paints a wash over this cell.
    pub selected: bool,
}

impl CellSnapshot {
    /// Effective paint colors after INVERSE/HIDDEN resolution.
    pub fn display_colors(&self) -> (CellColor, CellColor) {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if self.hidden { (bg, bg) } else { (fg, bg) }
    }
}

/// Cursor position in viewport coordinates (row 0 = top of the visible grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
}

/// Which mouse-reporting protocol (DECSET 1000/1002/1003) an application asked
/// for. They are cumulative in the terminal's mode bits but mutually exclusive
/// in intent, so the highest one set wins — the same resolution alacritty uses
/// when it decides whether a pointer move is worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No mouse mode: the UI keeps the pointer (selection, scrollback).
    Off,
    /// DECSET 1000 — presses and releases only.
    Click,
    /// DECSET 1002 — presses, releases, and motion while a button is held.
    Drag,
    /// DECSET 1003 — every motion, button held or not (any-event tracking).
    Any,
}

impl MouseProtocol {
    pub fn is_active(self) -> bool {
        self != MouseProtocol::Off
    }
}

/// The mouse-related terminal modes the pointer router consults to decide
/// whether a gesture belongs to the PTY or to the UI. `alacritty_terminal`
/// already tracks all of these bits; they simply were not surfaced, which is
/// why every TUI that requests mouse reporting was unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModes {
    pub protocol: MouseProtocol,
    /// DECSET 1006 — SGR (`CSI < b;x;yM`) instead of the legacy X10 encoding.
    pub sgr: bool,
    /// DECSET 1049/47/1041 — alt screen, which turns the wheel into AltScroll
    /// for full-screen apps that do NOT request mouse reporting (vim, less).
    pub alt_screen: bool,
    /// DECCKM — application cursor keys; picks SS3 vs CSI for AltScroll arrows.
    pub app_cursor: bool,
}

impl MouseModes {
    /// Everything off: the pointer is the UI's, exactly as before mouse relay
    /// existed (the no-regression baseline).
    pub const NONE: MouseModes = MouseModes {
        protocol: MouseProtocol::Off,
        sgr: false,
        alt_screen: false,
        app_cursor: false,
    };
}

/// Captures `Term` callbacks. Interior-mutable because `EventListener::send_event`
/// takes `&self`; `Arc<Mutex<…>>` rather than `Rc<RefCell<…>>` so [`Emulator`]
/// stays `Send` — the engine owns one per tracked terminal inside its session
/// mutex, which crosses threads. Contention is impossible (the capture lives
/// inside the emulator), so lock poisoning is folded back into the value.
#[derive(Default, Clone)]
struct EventCapture {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventCapture {
    fn take(&self) -> Vec<Event> {
        let mut events: MutexGuard<'_, Vec<Event>> =
            self.events.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut events)
    }
}

impl EventListener for EventCapture {
    fn send_event(&self, event: Event) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

/// The emulator: a pure fold of PTY bytes into a renderable grid.
pub struct Emulator {
    term: Term<EventCapture>,
    parser: Processor,
    capture: EventCapture,
    title: Option<String>,
    bell: bool,
    /// OSC 133 command-boundary state: the bytes of the current batch that
    /// could not be classified yet (an OSC string split across two PTY reads),
    /// and the blocks the markers found so far describe.
    held: Vec<u8>,
    blocks: CommandTracker,
}

impl Emulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, SCROLLBACK_LINES)
    }

    /// Build a grid with an explicit scrollback capacity (the user setting).
    /// `alacritty_terminal` fixes `scrolling_history` at `Term` construction,
    /// so changing the capacity of a live grid means rebuilding it — callers
    /// apply a new value to *freshly opened* terminals, never to running ones.
    pub fn with_scrollback(cols: u16, rows: u16, scrolling_history: usize) -> Self {
        let capture = EventCapture::default();
        let config = Config {
            scrolling_history,
            ..Config::default()
        };
        let term = Term::new(config, &GridSize::new(cols, rows), capture.clone());
        Self {
            term,
            parser: Processor::new(),
            capture,
            title: None,
            bell: false,
            held: Vec::new(),
            blocks: CommandTracker::default(),
        }
    }

    /// Advance the state machine over decoded PTY output. Returns bytes the
    /// terminal wants written back to the PTY (DSR/DA query responses etc.).
    ///
    /// The batch is fed to the ANSI parser in segments split at every OSC 133
    /// marker, so each boundary is recorded against the grid state the shell was
    /// actually in — the whole batch cannot simply be parsed first, because by
    /// then a command's entire output would already be on the screen and the
    /// block would have nothing left to locate. The bytes still all reach `vte`
    /// in their original order (a marker it does not know is an OSC it ignores),
    /// and the few trailing bytes of an OSC string that has not terminated yet
    /// wait in [`Self::held`] for the next batch instead of being parsed
    /// half.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut buffer = std::mem::take(&mut self.held);
        buffer.extend_from_slice(bytes);
        let scan = scan_markers(&buffer);
        let mut responses = Vec::new();
        let mut cursor = 0usize;
        for found in scan.markers {
            self.parse(&mut responses, &buffer[cursor..found.end]);
            cursor = found.end;
            self.note_marker(found.marker);
        }
        let through = scan.consumable.max(cursor);
        self.parse(&mut responses, &buffer[cursor..through]);
        self.held = buffer[through..].to_vec();
        responses
    }

    /// [`Self::feed`] with the engine's output seq folded in on both sides of the
    /// batch. Stamping only *after* would miss a block this batch closes (its
    /// range would stop at the last batch that added output, not the one that
    /// ended the command); stamping only *before* would miss a block this batch
    /// creates and closes inside one read. Both stamps make a block's range mean
    /// exactly "the batches whose bytes are inside this block", which is what a
    /// delta read is priced against.
    pub fn feed_with_seq(&mut self, bytes: &[u8], seq: u64) -> Vec<u8> {
        self.blocks.attach_seq(seq);
        let responses = self.feed(bytes);
        self.blocks.attach_seq(seq);
        responses
    }

    /// One segment of a batch: parse it and drain what the parser reported.
    fn parse(&mut self, responses: &mut Vec<u8>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
        for event in self.capture.take() {
            match event {
                Event::PtyWrite(text) => responses.extend_from_slice(text.as_bytes()),
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::Bell => self.bell = true,
                _ => {}
            }
        }
    }

    /// Act on one marker. Every position is read from the grid *now*, which is
    /// why [`Self::feed`] parses up to the marker before getting here.
    fn note_marker(&mut self, marker: CommandMarker) {
        let mark = self.mark();
        match marker.kind {
            MarkerKind::Property => self.blocks.note_property(&marker),
            MarkerKind::PromptStart => self.blocks.note_prompt_start(mark, marker.cwd),
            MarkerKind::PromptEnd => self.blocks.note_prompt_end(mark),
            MarkerKind::ExecStart => {
                let prompt_end = self.blocks.prompt_end_mark();
                let (command, source) = self.command_text(prompt_end, marker.command, mark);
                self.blocks
                    .note_exec_start(mark, command, source, marker.cwd);
            }
            MarkerKind::ExecDone => {
                self.blocks
                    .note_exec_done(mark, marker.exit_code, marker.cwd);
            }
        }
    }

    /// The line the user typed, read back out of the grid.
    ///
    /// `B` gives the exact start (its line and column), so the result is the
    /// input and nothing else. Without `B` — a shell that emits `A`/`C`/`D` but
    /// no prompt-end marker — the whole line above the output is taken, prompt
    /// included, and that is reported through [`CommandSource::LineWithPrompt`]
    /// rather than quietly stripped by guessing where the prompt ended.
    fn command_text(
        &self,
        prompt_end: Option<Mark>,
        payload: Option<String>,
        exec: Mark,
    ) -> (Option<String>, CommandSource) {
        if let Some(payload) = payload {
            let trimmed = payload.trim();
            if !trimmed.is_empty() {
                return (Some(trimmed.to_string()), CommandSource::Marker);
            }
        }
        let output_start = exec.after();
        let last = output_start.saturating_sub(1);
        if last == 0 {
            return (None, CommandSource::None);
        }
        let (from, source) = match prompt_end {
            Some(end) => (end.line, CommandSource::Typed),
            None => (last, CommandSource::LineWithPrompt),
        };
        if from > last || from == 0 {
            return (None, CommandSource::None);
        }
        let mut text = String::new();
        for abs in from..=last {
            let Some(line) = self.line_at_abs(abs) else {
                continue;
            };
            let line = if abs == from {
                match prompt_end {
                    // Drop the prompt: `B` arrived at this column of this line.
                    Some(end) => line.chars().skip(end.col).collect::<String>(),
                    None => line,
                }
            } else {
                line
            };
            if text.is_empty() {
                text = line;
                continue;
            }
            // A soft-wrapped continuation is the same logical line; a hard
            // newline (a shell's `>` continuation) is a real one.
            if self.prev_line_wrapped(abs) {
                text.push_str(&line);
            } else {
                text.push('\n');
                text.push_str(&line);
            }
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            (None, source)
        } else {
            (Some(text), source)
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(GridSize::new(cols, rows));
    }

    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    /// OSC title, if the running program set one.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True once a BEL arrived; reading clears it.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell)
    }

    /// Arrow keys should send SS3 (`ESC O A`) instead of CSI.
    pub fn app_cursor_mode(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Pastes should be wrapped in `ESC [200~` / `ESC [201~`.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// The mouse-reporting modes an application has requested. The pointer
    /// router relays presses/drags/releases/wheel to the PTY when a protocol is
    /// active, and keeps them local (selection, scrollback) otherwise.
    pub fn mouse_modes(&self) -> MouseModes {
        let mode = self.term.mode();
        let protocol = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProtocol::Any
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProtocol::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProtocol::Click
        } else {
            MouseProtocol::Off
        };
        MouseModes {
            protocol,
            sgr: mode.contains(TermMode::SGR_MOUSE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    /// Lines scrolled back into history (0 = pinned to the live bottom).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines available above the viewport.
    pub fn history_lines(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Scroll the view: positive = up into history, negative = toward live.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    // ---- selection ----
    //
    // `Term` owns the selection outright, which is what makes this cheap: it
    // rotates the anchors when output scrolls the grid and drops them on clear
    // and resize, so a selection tracks live output without any bookkeeping
    // here. The panel supplies pointer positions; everything below is a thin
    // translation into grid coordinates.

    /// The grid point under a viewport cell (row 0 = top of the visible area).
    ///
    /// Viewport rows are what the pointer hits; grid lines are what a selection
    /// anchors to, and the two differ by the scrollback offset. Anchoring in
    /// grid space is what lets a selection stay on its text while the view
    /// scrolls out from under it.
    pub fn grid_point(&self, viewport_row: usize, col: usize) -> Point {
        Point::new(
            Line(viewport_row as i32 - self.display_offset() as i32),
            Column(col.min(self.cols().saturating_sub(1))),
        )
    }

    /// Begin a selection. `ty` picks the granularity: [`SelectionType::Simple`]
    /// for a drag, `Semantic` for a double-click word, `Lines` for a triple-
    /// click row.
    pub fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    /// Extend the in-progress selection to `point`. No-op without one.
    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Select everything the user can see and scroll to: the whole history
    /// plus the live viewport, from the first history line's left edge to the
    /// last viewport cell's right edge. The context-menu "Select All" and the
    /// keyboard path both go through here.
    pub fn select_all(&mut self) {
        let top = Line(-(self.history_lines() as i32));
        let bottom = Line(self.rows() as i32 - 1);
        let last_col = Column(self.cols().saturating_sub(1));
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(top, Column(0)),
            Side::Left,
        );
        selection.update(Point::new(bottom, last_col), Side::Right);
        self.term.selection = Some(selection);
    }

    /// The selected text, or `None` when there is no selection or it covers
    /// nothing (a click without a drag leaves an empty one behind).
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Whether a non-empty selection is active — drives the copy action and
    /// the "clear it" branch on the next click.
    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        self.term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
    }

    /// Snapshot one viewport row (0 = top) honoring the scrollback offset.
    pub fn line(&self, viewport_row: usize) -> Vec<CellSnapshot> {
        self.line_inner(viewport_row, self.selection_range())
    }

    /// The shared body of [`Self::line`], taking the selection range as an
    /// argument so [`Self::lines`] resolves it once per frame rather than once
    /// per row — `to_range` re-walks the grid for semantic and line selections.
    fn line_inner(
        &self,
        viewport_row: usize,
        selection: Option<SelectionRange>,
    ) -> Vec<CellSnapshot> {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        let grid = self.term.grid();
        let row = &grid[line];
        (0..self.cols())
            .map(|col| {
                let cell = &row[Column(col)];
                CellSnapshot {
                    ch: cell.c,
                    fg: map_color(cell.fg),
                    bg: map_color(cell.bg),
                    bold: cell.flags.intersects(Flags::BOLD),
                    dim: cell.flags.intersects(Flags::DIM),
                    italic: cell.flags.intersects(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    inverse: cell.flags.intersects(Flags::INVERSE),
                    hidden: cell.flags.intersects(Flags::HIDDEN),
                    wide: cell.flags.intersects(Flags::WIDE_CHAR),
                    wide_spacer: cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                    selected: selection
                        .is_some_and(|range| range.contains(Point::new(line, Column(col)))),
                }
            })
            .collect()
    }

    /// All viewport rows, top to bottom.
    pub fn lines(&self) -> Vec<Vec<CellSnapshot>> {
        let selection = self.selection_range();
        (0..self.rows())
            .map(|r| self.line_inner(r, selection))
            .collect()
    }

    /// Cursor in viewport coordinates; `None` when hidden or scrolled out.
    pub fn cursor(&self) -> Option<CursorSnapshot> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let Point { line, column } = content.cursor.point;
        let row = line.0 + self.display_offset() as i32;
        if row < 0 || row >= self.rows() as i32 {
            return None;
        }
        Some(CursorSnapshot {
            row: row as usize,
            col: column.0,
        })
    }

    /// Test/diagnostic helper: a viewport row as trimmed text (wide-char
    /// spacers skipped). Viewport rows honor the scrollback offset, so this
    /// reads what is *on screen at that row*, scrolled back or not.
    pub fn row_text(&self, viewport_row: usize) -> String {
        let line = Line(viewport_row as i32 - self.display_offset() as i32);
        self.line_text(line)
    }

    // ---- plain-text reads ----
    //
    // A caller without a palette or a screen — the engine, and past it an agent
    // reading a terminal — wants characters, not cells. These reads are the
    // emulator's text surface and live next to the grid they read, so both
    // callers share one definition of "what does this line say".

    /// One grid line as text. `Line` is negative in scrollback and `0..` in the
    /// viewport, which is the coordinate a selection anchors to as well.
    /// Wide-char spacers are dropped (the glyph already occupies its cell) and
    /// the grid's trailing padding spaces are trimmed.
    pub fn line_text(&self, line: Line) -> String {
        let row = &self.term.grid()[line];
        let mut text: String = (0..self.cols())
            .map(|col| &row[Column(col)])
            .filter(|cell| {
                !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            })
            .map(|cell| cell.c)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }

    /// The current screen: every viewport row as text, top to bottom. For a
    /// full-screen app (vim, `less`, `htop`) this is the read that means
    /// something — its scrollback is escape drafts, not its display. Reading
    /// while scrolled back returns the visible window, matching the user.
    pub fn screen_text(&self) -> Vec<String> {
        (0..self.rows()).map(|row| self.row_text(row)).collect()
    }

    /// The last `lines` lines of the terminal's text — scrollback tail first,
    /// then the viewport, oldest first. Asking for more than exists returns
    /// everything there is, so a fresh prompt gives its `rows` lines and no more.
    pub fn tail_text(&self, lines: usize) -> Vec<String> {
        let offset = self.display_offset() as i32;
        let last = self.rows() as i32 - 1 - offset;
        let first = -(self.history_lines() as i32 + offset);
        let total = (last - first + 1).max(0) as usize;
        let take = lines.min(total);
        let start = last - take as i32 + 1;
        (start..=last)
            .map(|line| self.line_text(Line(line)))
            .collect()
    }

    /// Total text lines available to [`Self::tail_text`] (history + viewport).
    pub fn text_lines(&self) -> usize {
        self.history_lines() + self.rows()
    }

    // ---- OSC 133 command blocks ----
    //
    // A block is anchored to an *absolute content line*, not to a grid row:
    // `history_lines() + cursor_line + 1`, where `cursor_line` is the grid
    // index alacritty keeps the cursor at. Grid rows are viewport-relative and
    // the viewport moves, but that sum names the same line of text whichever way
    // the display is scrolled, and only stops being monotone once the scrollback
    // is full — which is exactly when the oldest block has been dropped anyway.
    // Callers therefore get a *distance*, and [`Self::scroll_offset_for_line`]
    // turns it back into a scroll position on the grid they are looking at.

    /// The absolute content line the cursor is on (1-based).
    pub fn content_line(&self) -> usize {
        self.history_lines() + self.term.grid().cursor.point.line.0 as usize + 1
    }

    /// Where the cursor is, in the coordinates a marker is recorded against.
    fn mark(&self) -> Mark {
        Mark {
            line: self.content_line(),
            col: self.term.grid().cursor.point.column.0,
        }
    }

    /// The text of absolute line `abs`, or `None` when the scrollback no longer
    /// holds it (trimmed away, or the grid moved past it).
    fn line_at_abs(&self, abs: usize) -> Option<String> {
        if abs == 0 || abs > self.text_lines() {
            return None;
        }
        let history = self.history_lines();
        let line = Line(abs as i32 - 1 - history as i32);
        if line.0 < -(history as i32) || line.0 >= self.rows() as i32 {
            return None;
        }
        Some(self.line_text(line))
    }

    /// Was absolute line `abs - 1` soft-wrapped into this one (as opposed to
    /// ending in a real newline)? Used to join a typed command that wrapped.
    fn prev_line_wrapped(&self, abs: usize) -> bool {
        if abs <= 1 || abs > self.text_lines() {
            return false;
        }
        let history = self.history_lines();
        let line = Line(abs as i32 - 2 - history as i32);
        if line.0 < -(history as i32) || line.0 >= self.rows() as i32 {
            return false;
        }
        let last = Column(self.cols().saturating_sub(1));
        let flags = self.term.grid()[line][last].flags;
        flags.intersects(Flags::WRAPLINE)
    }

    /// The command blocks this grid describes. A block between `C` and `D` is
    /// reported with its output end at the live line, so a reader watching a
    /// running command sees it grow.
    pub fn command_blocks(&self) -> Vec<CommandBlock> {
        self.blocks.live(self.mark())
    }

    /// The text a block's output region holds, oldest line first. Lines the
    /// scrollback has already dropped are simply absent — a block whose output
    /// scrolled away reads as what is left, never as an error.
    pub fn block_lines(&self, block: &CommandBlock) -> Vec<String> {
        let start = block.output_start_line.max(1);
        let end = block.output_end_line.max(start);
        (start..end)
            .filter_map(|abs| self.line_at_abs(abs))
            .collect()
    }

    /// The absolute line at the top of the viewport right now — where the UI
    /// stands when it walks commands.
    pub fn top_visible_line(&self) -> usize {
        self.history_lines()
            .saturating_sub(self.display_offset())
            .saturating_add(1)
    }

    /// The last command whose output starts at or above `line` (backwards
    /// navigation), or `None` when there is nothing above.
    pub fn command_before(&self, line: usize) -> Option<CommandBlock> {
        block_before(&self.command_blocks(), line).cloned()
    }

    /// The next command whose output starts below `line` (forwards navigation).
    pub fn command_after(&self, line: usize) -> Option<CommandBlock> {
        block_after(&self.command_blocks(), line).cloned()
    }

    /// The newest block that IS a command: the running one, or the last one
    /// that finished/settled. A bare prompt block (`A` with no `C` yet) is not
    /// a command — it is the shell waiting — so it is skipped: after `D;A`
    /// the answer to "what ran last" is the finished block, not the empty
    /// prompt that followed it.
    pub fn last_command_block(&self) -> Option<CommandBlock> {
        self.command_blocks()
            .into_iter()
            .rev()
            .find(|block| block.status != BlockStatus::Prompt)
    }

    /// Did a valid OSC 133 marker ever arrive? The silence fallback only speaks
    /// when it did not — with markers present, a missing `D` means the command
    /// is genuinely still running.
    pub fn saw_markers(&self) -> bool {
        self.blocks.saw_markers()
    }

    /// Stamp the engine's output seq onto the blocks made from this batch (see
    /// [`CommandTracker::attach_seq`]). The UI grid never calls it, and its
    /// blocks keep `seq == 0`, which is honest: a consumer without seqs cannot
    /// ask for a delta.
    pub fn attach_seq(&mut self, seq: u64) {
        self.blocks.attach_seq(seq);
    }

    /// Close output printed with no markers as one labelled-unknown block.
    /// Returns `true` when a block was created.
    pub fn settle_silence(&mut self) -> bool {
        let mark = self.mark();
        self.blocks.settle_silence(mark)
    }

    /// The display offset that would put absolute line `abs` at the top of the
    /// viewport — what the UI scrolls by to jump to a command's output. Clamped
    /// to what the grid can actually show.
    pub fn scroll_offset_for_line(&self, abs: usize) -> usize {
        let history = self.history_lines();
        history.saturating_add(1).saturating_sub(abs).min(history)
    }
}

impl std::fmt::Debug for Emulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emulator")
            .field("cols", &self.cols())
            .field("rows", &self.rows())
            .field("display_offset", &self.display_offset())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu(cols: u16, rows: u16) -> Emulator {
        Emulator::new(cols, rows)
    }

    #[test]
    fn plain_text_lands_on_row_zero() {
        let mut e = emu(20, 5);
        e.feed(b"hello");
        assert_eq!(e.row_text(0), "hello");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 5 }));
    }

    #[test]
    fn crlf_moves_lines_and_cr_returns_to_column_zero() {
        let mut e = emu(20, 5);
        e.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(e.row_text(0), "one");
        assert_eq!(e.row_text(1), "two");
        assert_eq!(e.row_text(2), "three");
        e.feed(b"\rXX");
        assert_eq!(e.row_text(2), "XXree");
    }

    #[test]
    fn long_line_wraps_at_the_grid_width() {
        let mut e = emu(10, 4);
        e.feed(b"abcdefghijKLM");
        assert_eq!(e.row_text(0), "abcdefghij");
        assert_eq!(e.row_text(1), "KLM");
    }

    #[test]
    fn sgr_colors_and_attributes() {
        let mut e = emu(40, 4);
        e.feed(b"\x1b[31mred\x1b[0m plain \x1b[1;44mboldbg\x1b[0m");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(1));
        assert_eq!(line[0].bg, CellColor::Background);
        // After reset: defaults.
        assert_eq!(line[4].fg, CellColor::Foreground);
        // Bold + blue background segment starts at col 10 ("red plain " = 10).
        let bold_cell = line[10];
        assert!(bold_cell.bold);
        assert_eq!(bold_cell.bg, CellColor::Indexed(4));
    }

    #[test]
    fn bright_256_and_truecolor_sgr() {
        let mut e = emu(40, 2);
        e.feed(b"\x1b[95mA\x1b[38;5;196mB\x1b[38;2;10;20;30mC");
        let line = e.line(0);
        assert_eq!(line[0].fg, CellColor::Indexed(13)); // bright magenta
        assert_eq!(line[1].fg, CellColor::Indexed(196));
        assert_eq!(line[2].fg, CellColor::Rgb(10, 20, 30));
    }

    #[test]
    fn inverse_and_hidden_resolve_in_display_colors() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[7mI\x1b[0m\x1b[8mH");
        let inv = e.line(0)[0];
        assert!(inv.inverse);
        assert_eq!(
            inv.display_colors(),
            (CellColor::Background, CellColor::Foreground)
        );
        let hid = e.line(0)[1];
        assert!(hid.hidden);
        let (fg, bg) = hid.display_colors();
        assert_eq!(fg, bg, "hidden text paints foreground as background");
    }

    #[test]
    fn cursor_addressing_and_relative_moves() {
        let mut e = emu(20, 6);
        e.feed(b"\x1b[3;5Hx");
        // CSI H is 1-based; cell written at row 2, col 4; cursor advanced by 1.
        assert_eq!(e.line(2)[4].ch, 'x');
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 5 }));
        e.feed(b"\x1b[2D"); // left twice
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 2, col: 3 }));
        e.feed(b"\x1b[A"); // up
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 1, col: 3 }));
    }

    #[test]
    fn clear_screen_and_home() {
        let mut e = emu(20, 4);
        e.feed(b"aaa\r\nbbb\r\nccc");
        e.feed(b"\x1b[2J\x1b[H");
        for row in 0..4 {
            assert_eq!(e.row_text(row), "");
        }
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 0 }));
        e.feed(b"fresh");
        assert_eq!(e.row_text(0), "fresh");
    }

    #[test]
    fn erase_line_variants() {
        let mut e = emu(20, 2);
        e.feed(b"abcdef\x1b[3D\x1b[K"); // erase from cursor (col 3) to end
        assert_eq!(e.row_text(0), "abc");
    }

    #[test]
    fn scrollback_history_and_scrolling() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        // Viewport shows the tail (line7, line8, then the blank prompt row).
        assert_eq!(e.row_text(0), "line7");
        assert_eq!(e.history_lines(), 6);
        assert_eq!(e.display_offset(), 0);
        // Scroll up into history.
        e.scroll(2);
        assert_eq!(e.display_offset(), 2);
        assert_eq!(e.row_text(0), "line5");
        // Cursor is below the viewport while scrolled back.
        assert_eq!(e.cursor(), None);
        // Over-scroll clamps to the top of history.
        e.scroll(100);
        assert_eq!(e.display_offset(), 6);
        assert_eq!(e.row_text(0), "line1");
        e.scroll_to_bottom();
        assert_eq!(e.display_offset(), 0);
        assert_eq!(e.row_text(0), "line7");
    }

    #[test]
    fn alt_screen_restores_primary_content() {
        let mut e = emu(20, 4);
        e.feed(b"primary");
        // Enter the alt screen; 1049 keeps the cursor position, so home first.
        e.feed(b"\x1b[?1049h\x1b[H");
        e.feed(b"alt-content");
        assert_eq!(e.row_text(0), "alt-content");
        e.feed(b"\x1b[?1049l"); // leave
        assert_eq!(e.row_text(0), "primary");
    }

    #[test]
    fn dsr_cursor_report_produces_pty_response() {
        let mut e = emu(20, 4);
        e.feed(b"\x1b[2;3H");
        let responses = e.feed(b"\x1b[6n");
        assert_eq!(String::from_utf8_lossy(&responses), "\x1b[2;3R");
    }

    #[test]
    fn osc_title_and_bell() {
        let mut e = emu(20, 2);
        assert_eq!(e.title(), None);
        e.feed(b"\x1b]0;my title\x07");
        assert_eq!(e.title(), Some("my title"));
        assert!(!e.take_bell());
        e.feed(b"\x07");
        assert!(e.take_bell());
        assert!(!e.take_bell(), "bell reads clear it");
    }

    #[test]
    fn app_cursor_and_bracketed_paste_modes_toggle() {
        let mut e = emu(10, 2);
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?1h");
        assert!(e.app_cursor_mode());
        e.feed(b"\x1b[?1l");
        assert!(!e.app_cursor_mode());
        e.feed(b"\x1b[?2004h");
        assert!(e.bracketed_paste_mode());
    }

    /// The hole that made every mouse-reporting TUI unusable: `alacritty`
    /// already tracks 1000/1002/1003/1006, but the emulator only exposed
    /// APP_CURSOR/BRACKETED_PASTE. Now the pointer router can see them.
    #[test]
    fn mouse_modes_track_the_decsit_protocols() {
        let mut e = emu(10, 2);
        assert_eq!(e.mouse_modes().protocol, MouseProtocol::Off);
        assert!(!e.mouse_modes().sgr);

        // DECSET 1000 → click reporting.
        e.feed(b"\x1b[?1000h");
        assert_eq!(e.mouse_modes().protocol, MouseProtocol::Click);

        // DECSET 1002 supersedes 1000 with drag tracking; SGR stacks on.
        // alacritty treats the tracking protocols as MUTUALLY EXCLUSIVE — each
        // `set` first clears MOUSE_MODE, so only the last one requested stays.
        e.feed(b"\x1b[?1002h");
        assert_eq!(e.mouse_modes().protocol, MouseProtocol::Drag);
        assert!(!e.mouse_modes().sgr);
        e.feed(b"\x1b[?1006h");
        assert!(e.mouse_modes().sgr, "SGR 1006 is now visible");

        // DECSET 1003 (any-event) replaces 1002.
        e.feed(b"\x1b[?1003h");
        assert_eq!(e.mouse_modes().protocol, MouseProtocol::Any);

        // Resetting 1003 clears tracking entirely (not a fall-back to 1002,
        // which was already cleared when 1003 was set).
        e.feed(b"\x1b[?1003l");
        assert_eq!(e.mouse_modes().protocol, MouseProtocol::Off);

        // Resetting SGR clears the flag.
        e.feed(b"\x1b[?1006l");
        assert!(!e.mouse_modes().sgr);
    }

    /// Alt-screen + app-cursor feed the AltScroll path: full-screen apps that
    /// request no mouse mode still need the wheel routed to them as arrows.
    #[test]
    fn mouse_modes_report_alt_and_app_cursor() {
        let mut e = emu(10, 2);
        assert!(!e.mouse_modes().alt_screen);
        assert!(!e.mouse_modes().app_cursor);
        e.feed(b"\x1b[?1049h");
        e.feed(b"\x1b[?1h");
        let modes = e.mouse_modes();
        assert!(modes.alt_screen);
        assert!(modes.app_cursor);
        assert_eq!(
            modes.protocol,
            MouseProtocol::Off,
            "alt screen is not a mouse mode"
        );
    }

    #[test]
    fn hidden_cursor_mode() {
        let mut e = emu(10, 2);
        e.feed(b"\x1b[?25l");
        assert_eq!(e.cursor(), None);
        e.feed(b"\x1b[?25h");
        assert!(e.cursor().is_some());
    }

    #[test]
    fn resize_preserves_content_and_reflows_cursor() {
        let mut e = emu(20, 5);
        e.feed(b"keepme\r\nsecond");
        e.resize(30, 3);
        assert_eq!(e.cols(), 30);
        assert_eq!(e.rows(), 3);
        assert_eq!(e.row_text(0), "keepme");
        assert_eq!(e.row_text(1), "second");
    }

    #[test]
    fn wide_chars_occupy_two_cells_with_spacer() {
        let mut e = emu(10, 2);
        e.feed("宽w".as_bytes());
        let line = e.line(0);
        assert!(line[0].wide);
        assert_eq!(line[0].ch, '宽');
        assert!(line[1].wide_spacer);
        assert_eq!(line[2].ch, 'w');
        assert_eq!(e.row_text(0), "宽w");
        assert_eq!(e.cursor(), Some(CursorSnapshot { row: 0, col: 3 }));
    }

    /// Viewport row → grid line, which is the translation every selection
    /// anchor goes through. Unscrolled they coincide; scrolled back, the same
    /// viewport row names a line further up history.
    #[test]
    fn grid_point_offsets_by_the_scrollback_position() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(e.grid_point(0, 2), Point::new(Line(0), Column(2)));
        e.scroll(4);
        assert_eq!(e.grid_point(0, 2), Point::new(Line(-4), Column(2)));
        // Columns clamp into the grid so an over-wide pointer cannot anchor
        // outside it.
        assert_eq!(e.grid_point(0, 99).column, Column(9));
    }

    #[test]
    fn simple_selection_yields_its_text_and_marks_its_cells() {
        let mut e = emu(20, 3);
        e.feed(b"hello world");
        assert!(!e.has_selection());
        assert_eq!(e.selection_text(), None);

        // Drag across "hello".
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(0, 4), Side::Right);
        assert!(e.has_selection());
        assert_eq!(e.selection_text().as_deref(), Some("hello"));

        let line = e.line(0);
        assert!(line[..5].iter().all(|c| c.selected));
        assert!(!line[5].selected, "the space past the drag is not selected");

        e.clear_selection();
        assert!(!e.has_selection());
        assert!(e.line(0).iter().all(|c| !c.selected));
    }

    /// Double-click granularity: the anchor expands to the whole word without
    /// the caller computing any boundaries.
    #[test]
    fn semantic_selection_expands_to_the_word() {
        let mut e = emu(30, 2);
        e.feed(b"alpha beta gamma");
        e.start_selection(SelectionType::Semantic, e.grid_point(0, 7), Side::Left);
        assert_eq!(e.selection_text().as_deref(), Some("beta"));
    }

    /// Triple-click granularity. The trailing newline is part of the copy —
    /// pasting a line-selection should reproduce the line break, the way it
    /// does in every other terminal.
    #[test]
    fn line_selection_takes_the_whole_row() {
        let mut e = emu(30, 3);
        e.feed(b"first row\r\nsecond row");
        e.start_selection(SelectionType::Lines, e.grid_point(1, 3), Side::Left);
        assert_eq!(e.selection_text().as_deref(), Some("second row\n"));
    }

    /// A selection made across a line break keeps the newline, so pasting the
    /// copy reproduces the rows.
    #[test]
    fn selection_spans_rows_with_a_newline() {
        let mut e = emu(10, 3);
        e.feed(b"ab\r\ncd");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(1, 1), Side::Right);
        assert_eq!(e.selection_text().as_deref(), Some("ab\ncd"));
    }

    /// The reason anchors live in grid space: output that scrolls the grid must
    /// carry the selection with its text, not leave it pinned to a screen row.
    #[test]
    fn selection_follows_its_text_when_output_scrolls() {
        let mut e = emu(10, 3);
        e.feed(b"target\r\n");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 0), Side::Left);
        e.update_selection(e.grid_point(0, 5), Side::Right);
        assert_eq!(e.selection_text().as_deref(), Some("target"));
        // Push it up the screen; the text is unchanged, so the copy is too.
        e.feed(b"a\r\nb\r\nc\r\n");
        assert_eq!(e.selection_text().as_deref(), Some("target"));
    }

    /// A click with no drag selects nothing, and must not report a selection —
    /// otherwise the copy action fires on every bare click.
    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let mut e = emu(20, 2);
        e.feed(b"hello");
        e.start_selection(SelectionType::Simple, e.grid_point(0, 2), Side::Left);
        assert_eq!(e.selection_text(), None);
        assert!(!e.has_selection());
    }

    #[test]
    fn utf8_split_across_feeds_reassembles() {
        let mut e = emu(10, 2);
        let bytes = "é".as_bytes();
        e.feed(&bytes[..1]);
        e.feed(&bytes[1..]);
        assert_eq!(e.row_text(0), "é");
    }

    /// The scrollback setting lands on `Term` at construction: a small
    /// capacity must actually cap the history (which is why changing it on a
    /// live grid needs a rebuild — the panel applies it to new tabs).
    #[test]
    fn with_scrollback_caps_the_history() {
        let mut e = Emulator::with_scrollback(10, 3, 4);
        for i in 1..=10 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(
            e.history_lines() <= 4,
            "history capped: {}",
            e.history_lines()
        );
        // A default-sized emulator still keeps the full configured window.
        let mut wide = emu(10, 3);
        for i in 1..=10 {
            wide.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(wide.history_lines() > 4);
    }

    /// Context-menu Select All: the copy must start at the top of history and
    /// reach the bottom of the viewport, exactly what a native terminal does.
    #[test]
    fn select_all_spans_history_and_viewport() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        e.select_all();
        assert!(e.has_selection());
        let text = e.selection_text().expect("a selection to copy");
        assert!(text.starts_with("line1"), "starts at the top: {text:?}");
        assert!(text.contains("line8"), "reaches the live rows: {text:?}");
    }

    /// No history yet (a fresh prompt): Select All still covers the viewport
    /// without panicking on the negative anchor.
    #[test]
    fn select_all_on_an_empty_grid_selects_the_viewport() {
        let mut e = emu(10, 3);
        e.feed(b"hi");
        e.select_all();
        let text = e.selection_text().expect("viewport selected");
        assert!(text.starts_with("hi"));
    }

    /// The engine moves an emulator between threads inside its session mutex;
    /// the `Rc<RefCell>` event capture this crate used to carry would have made
    /// that impossible. It is `Send`, and deliberately not `Sync` (a `Term` is
    /// a mutable fold, so callers hold it in a lock).
    #[test]
    fn emulator_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Emulator>();
    }

    #[test]
    fn row_text_and_line_text_agree_on_the_same_row() {
        let mut e = emu(12, 3);
        e.feed("a宽b\r\ntail".as_bytes());
        assert_eq!(e.row_text(0), "a宽b");
        assert_eq!(e.line_text(Line(0)), "a宽b");
        assert_eq!(e.line_text(Line(1)), "tail");
        assert_eq!(e.line_text(Line(2)), "", "padding trims to nothing");
    }

    #[test]
    fn screen_text_is_the_viewport_top_to_bottom() {
        let mut e = emu(10, 3);
        e.feed(b"one\r\ntwo\r\nthree");
        assert_eq!(e.screen_text(), vec!["one", "two", "three"]);
        // A redraw in place (the full-screen-app case) shows up in the screen.
        e.feed(b"\x1b[2J\x1b[H\x1b[1;1Hnew");
        assert_eq!(e.screen_text(), vec!["new", "", ""]);
    }

    #[test]
    fn screen_text_follows_the_scrolled_window() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(e.screen_text(), vec!["line7", "line8", ""]);
        e.scroll(2);
        assert_eq!(e.screen_text(), vec!["line5", "line6", "line7"]);
    }

    #[test]
    fn tail_text_walks_history_then_viewport() {
        let mut e = emu(10, 3);
        for i in 1..=8 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        // history = line1..line6, viewport = line7, line8, "" (the prompt row).
        assert_eq!(e.text_lines(), 9);
        assert_eq!(e.tail_text(3), vec!["line7", "line8", ""]);
        assert_eq!(e.tail_text(4), vec!["line6", "line7", "line8", ""]);
        assert_eq!(e.tail_text(5), vec!["line5", "line6", "line7", "line8", ""]);
        // Asking for more than exists returns all of it, oldest first.
        let all = e.tail_text(1_000);
        assert_eq!(all.len(), 9);
        assert_eq!(all.first().unwrap(), "line1");
        assert_eq!(all.last().unwrap(), "");
    }

    #[test]
    fn tail_text_on_a_fresh_grid_is_the_viewport() {
        let e = emu(10, 3);
        assert_eq!(e.tail_text(50), vec!["", "", ""]);
        assert_eq!(e.tail_text(0), Vec::<String>::new());
    }

    /// The alt screen is a different buffer, not a scrollback region: leaving it
    /// must restore the primary text exactly (this is the read an agent would
    /// make after `vim` exits).
    #[test]
    fn tail_text_reads_the_active_screen_only() {
        let mut e = emu(10, 3);
        e.feed(b"primary\r\n");
        e.feed(b"\x1b[?1049h\x1b[H");
        e.feed(b"alt");
        assert_eq!(e.screen_text()[0], "alt");
        e.feed(b"\x1b[?1049l");
        assert_eq!(e.screen_text()[0], "primary");
    }

    #[test]
    fn text_reads_survive_a_resize() {
        let mut e = emu(20, 4);
        e.feed(b"keepme\r\nsecond");
        e.resize(10, 3);
        assert_eq!(e.cols(), 10);
        assert_eq!(e.screen_text()[0], "keepme");
        assert_eq!(e.screen_text()[1], "second");
    }

    // ---- OSC 133 through the real grid ----
    //
    // The tracker tests above cover the bookkeeping; these cover the integration:
    // the same bytes a marked shell actually writes, fed to `feed`, must give
    // correct blocks *and* an unchanged screen.

    use crate::BlockStatus;

    fn osc(body: &str) -> String {
        format!("\x1b]{body}\x07")
    }

    /// What a bash with our injected markers sends for `make deploy` failing
    /// with status 2: the prompt (`A` … `B`), the echoed command, execution (`C`),
    /// the output, then the status (`D;2`) on the line the next prompt starts on.
    fn marked_stream() -> String {
        format!(
            "{a}user@host:/srv/app$ {b}make deploy\r\n{c}building\r\nerror: boom\r\n{d}",
            a = osc("133;A;Cwd=/srv/app"),
            b = osc("133;B"),
            c = osc("133;C"),
            d = osc("133;D;2"),
        )
    }

    /// One marked `cmd{i}` printing two lines, as the shell writes it.
    fn command_round(i: usize) -> String {
        format!(
            "{a}p$ {b}cmd{i}\r\n{c}out{i}a\r\nout{i}b\r\n{d}",
            a = osc("133;A"),
            b = osc("133;B"),
            c = osc("133;C"),
            d = osc("133;D;0"),
        )
    }

    /// Remove every `ESC ] … BEL` string, leaving the visible text behind.
    fn strip_osc(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find("\x1b]") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start + 2..].find('\x07') else {
                out.push_str(&rest[start..]);
                return out;
            };
            rest = &rest[start + 2 + end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The safety property in one assertion: a marker is read *alongside* the
    /// ANSI parser, never instead of it, so the grid a marked shell draws is
    /// pixel-for-pixel the grid an unmarked one draws.
    #[test]
    fn markers_leave_no_visible_characters_on_screen() {
        let stream = marked_stream();
        let mut marked = emu(40, 6);
        let mut plain = emu(40, 6);
        marked.feed(stream.as_bytes());
        plain.feed(strip_osc(&stream).as_bytes());
        assert_eq!(
            marked.screen_text(),
            plain.screen_text(),
            "a marker must never reach the screen"
        );
        assert_eq!(marked.screen_text()[0], "user@host:/srv/app$ make deploy");
        for row in marked.screen_text() {
            assert!(!row.contains("133"), "marker payload leaked: {row:?}");
            assert!(!row.contains('\x1b'), "stray escape leaked: {row:?}");
            assert!(!row.contains('\x07'), "stray BEL leaked: {row:?}");
        }
    }

    /// The acceptance this step exists for, end to end: one marked command gives
    /// the command line, the cwd, the output region and a non-zero exit code.
    #[test]
    fn a_marked_stream_describes_a_command_with_its_nonzero_exit() {
        let mut e = emu(40, 6);
        e.feed(marked_stream().as_bytes());
        assert!(e.saw_markers());
        let blocks = e.command_blocks();
        let done = blocks
            .iter()
            .find(|block| block.status == BlockStatus::Done)
            .expect("a finished command");
        assert_eq!(done.command.as_deref(), Some("make deploy"));
        assert_eq!(done.command_source, CommandSource::Typed);
        assert_eq!(done.cwd.as_deref(), Some("/srv/app"));
        assert_eq!(done.exit_code, Some(2));
        assert!(done.exit_is_known());
        assert!(done.marker_backed);
        assert_eq!(
            e.block_lines(done),
            vec!["building", "error: boom"],
            "the output is the command's, not the prompt's"
        );
    }

    /// PTY reads cut anywhere — including inside a marker. Byte-at-a-time feeding
    /// is the worst case, and the held bytes must make up for it.
    #[test]
    fn markers_split_across_feeds_still_build_one_block() {
        let stream = marked_stream();
        let mut e = emu(40, 6);
        for byte in stream.as_bytes() {
            e.feed(&[*byte]);
        }
        let blocks = e.command_blocks();
        let done = blocks
            .iter()
            .find(|block| block.status == BlockStatus::Done)
            .expect("a finished command");
        assert_eq!(done.command.as_deref(), Some("make deploy"));
        assert_eq!(done.exit_code, Some(2));
        assert_eq!(e.block_lines(done), vec!["building", "error: boom"]);
        // And the slow feed drew the same screen as the fast one.
        let mut fast = emu(40, 6);
        fast.feed(stream.as_bytes());
        assert_eq!(e.screen_text(), fast.screen_text());
    }

    /// A marker with a payload beats reading the grid, and says where it came
    /// from: a shell that sends `E=` is trusted for the command line.
    #[test]
    fn a_payload_command_line_is_taken_verbatim() {
        let mut e = emu(40, 6);
        e.feed(
            format!(
                "{}{}\r\nsleep 45\r\n",
                osc("133;A"),
                osc("133;C;E=sleep%2045")
            )
            .as_bytes(),
        );
        let open = e
            .command_blocks()
            .pop()
            .expect("the running command is open");
        assert_eq!(open.status, BlockStatus::Running);
        assert_eq!(open.command.as_deref(), Some("sleep 45"));
        assert_eq!(open.command_source, CommandSource::Marker);
    }

    /// Without markers, output still becomes a block — one that admits it knows
    /// neither the command nor its status.
    #[test]
    fn silence_fallback_settles_unmarked_output() {
        let mut e = emu(40, 6);
        e.feed(b"user@host:~$ ls\r\nfile.txt\r\n");
        assert!(!e.saw_markers());
        assert!(e.settle_silence());
        let blocks = e.command_blocks();
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.status, BlockStatus::Settled);
        assert!(!block.marker_backed);
        assert!(block.command.is_none(), "the command line is not guessed");
        assert!(block.exit_code.is_none());
        assert_eq!(e.block_lines(block), vec!["user@host:~$ ls", "file.txt"]);
        // No new output: no new block.
        assert!(!e.settle_silence());
        e.feed(b"more\r\n");
        assert!(e.settle_silence());
        let after = e.command_blocks();
        assert_eq!(after.len(), 2);
        assert_eq!(e.block_lines(&after[1]), vec!["more"]);
    }

    /// A marked terminal that simply never got its `D` (the command is still
    /// running) must not be closed by the fallback.
    #[test]
    fn silence_never_closes_a_marked_running_command() {
        let mut e = emu(40, 6);
        e.feed(format!("{a}make\r\n{c}", a = osc("133;A"), c = osc("133;C")).as_bytes());
        e.feed(b"cc1\r\ncc2\r\n");
        assert!(!e.settle_silence());
        let blocks = e.command_blocks();
        let open = blocks.last().expect("the running command");
        assert_eq!(open.status, BlockStatus::Running);
        assert_eq!(
            e.block_lines(open),
            vec!["cc1", "cc2"],
            "and its output grows while it runs"
        );
    }

    /// The UI consumer: walking commands and putting one's output at the top of
    /// the viewport, using absolute content lines only.
    #[test]
    fn navigation_walks_commands_and_jumps_to_their_output() {
        let mut e = emu(40, 4);
        for i in 0..3 {
            e.feed(command_round(i).as_bytes());
        }
        let blocks = e.command_blocks();
        assert_eq!(blocks.len(), 3, "three finished commands");
        assert!(e.history_lines() > 0, "the first ones are in scrollback");
        assert_eq!(
            blocks
                .iter()
                .map(|b| b.command.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("cmd0"), Some("cmd1"), Some("cmd2")],
            "each round reads its own typed line, prompt excluded"
        );
        // Backwards from the live bottom: 2, then 1, then 0, then nothing.
        let mut at = e.content_line();
        let mut walked = Vec::new();
        while let Some(block) = e.command_before(at) {
            walked.push(block.command.clone().unwrap_or_default());
            at = block.output_start_line.saturating_sub(1);
        }
        assert_eq!(walked, vec!["cmd2", "cmd1", "cmd0"]);
        // Jumping to `cmd1`'s output puts its first line on row 0.
        let target = e
            .command_blocks()
            .into_iter()
            .find(|block| block.command.as_deref() == Some("cmd1"))
            .expect("cmd1");
        let offset = e.scroll_offset_for_line(target.output_start_line);
        e.scroll(offset as i32 - e.display_offset() as i32);
        assert_eq!(e.display_offset(), offset, "the jump landed where computed");
        assert_eq!(e.row_text(0), "out1a");
        assert_eq!(
            e.line_at_abs(target.output_start_line).as_deref(),
            Some("out1a"),
            "the absolute line still names the same text after scrolling"
        );
        // And forward from there finds the next one.
        let next = e
            .command_after(target.output_start_line)
            .expect("a command below");
        assert_eq!(next.command.as_deref(), Some("cmd2"));
    }

    /// Once the scrollback is exhausted the oldest lines are gone: a jump must
    /// clamp to what the grid can show instead of scrolling into nothing.
    #[test]
    fn jumping_to_a_dropped_line_clamps_instead_of_failing() {
        let mut e = Emulator::with_scrollback(40, 4, 2);
        for i in 0..20 {
            e.feed(format!("line{i}\r\n").as_bytes());
        }
        let offset = e.scroll_offset_for_line(1);
        assert_eq!(offset, e.history_lines(), "clamped at the oldest line held");
        e.scroll(offset as i32 - e.display_offset() as i32);
        assert!(
            e.row_text(0).starts_with("line"),
            "still real text: {:?}",
            e.row_text(0)
        );
    }

    /// Blocks fold in the engine's output seqs without their ranges sliding:
    /// the delta read an agent makes depends on those bounds being real.
    #[test]
    fn seqs_attach_to_the_batches_that_made_the_blocks() {
        let mut e = emu(40, 6);
        e.feed_with_seq(format!("{a}p$ \r\n", a = osc("133;A")).as_bytes(), 3);
        e.feed_with_seq(format!("{c}work\r\n", c = osc("133;C")).as_bytes(), 4);
        e.feed_with_seq(osc("133;D;0").as_bytes(), 5);
        let blocks = e.command_blocks();
        let done = blocks
            .iter()
            .find(|block| block.status == BlockStatus::Done)
            .expect("the finished command");
        assert_eq!(
            (done.seq_start, done.seq_end),
            (3, 5),
            "the block spans exactly the batches its bytes arrived in"
        );
        // Later output must not stretch a closed block's range.
        e.feed_with_seq(b"more work\r\n", 9);
        let blocks = e.command_blocks();
        let done = blocks
            .iter()
            .find(|block| block.status == BlockStatus::Done)
            .expect("still there");
        assert_eq!(
            (done.seq_start, done.seq_end),
            (3, 5),
            "a closed block's range cannot stretch into output it did not contain"
        );
    }
}
