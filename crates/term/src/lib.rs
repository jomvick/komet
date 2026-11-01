//! # komet-term
//!
//! The terminal emulator core, shared by the gpui terminal panel and the
//! engine — deliberately **free of gpui** so the engine can know what a
//! terminal is displaying without a UI attached.
//!
//! Two layers:
//!
//! - [`Emulator`] (`emulator`): `alacritty_terminal`'s `Term` + vte's ANSI
//!   `Processor` folded into a pure state machine. Bytes in
//!   ([`Emulator::feed`]), grid snapshots out ([`Emulator::line`],
//!   [`Emulator::cursor`]), plus the escape-sequence-derived state the callers
//!   act on: OSC titles, the bell, the mouse-reporting modes, alt-screen,
//!   selection. No I/O, no timers, no paint.
//! - [`TextJournal`] (`text`): the plain-text reads on top of the emulator —
//!   the current screen ([`Emulator::screen_text`]), the tail of the transcript
//!   ([`Emulator::tail_text`]), and what appeared since a given output sequence
//!   number ([`TextJournal::delta_since`]). This is what lets a non-graphical
//!   consumer (the engine, later an MCP `terminal_read`) follow a shell, and it
//!   is why the journal is stamped with the engine's `seq`.
//!
//! The UI side of the seam stays in `komet-ui::terminal`: theme colors
//! (`CellColor` → `Hsla`), glyph metrics, pointer→cell conversion and paint.
//! Those read the snapshots this crate produces and never touch
//! `alacritty_terminal` themselves.
//!
//! `Emulator` is `Send` (see the `emulator_is_send` test) because the engine
//! moves it between threads inside its session mutex; it is *not* `Sync`, and
//! neither is `alacritty`'s `Term` — callers hold it in a `Mutex`.

mod emulator;
mod markers;
mod text;

pub use alacritty_terminal::index::{Point as GridPoint, Side};
pub use alacritty_terminal::selection::SelectionType;
pub use emulator::{
    CellColor, CellSnapshot, CursorSnapshot, Emulator, GridSize, MouseModes, MouseProtocol,
    SCROLLBACK_LINES,
};
pub use markers::{
    BlockStatus, CommandBlock, CommandMarker, CommandSource, CommandTracker, MAX_OSC_BYTES, Mark,
    MarkerKind, Scan, ScannedMarker, scan_markers,
};
pub use text::{
    TRACKED_JOURNAL_STEPS, TRACKED_TAIL_WINDOW, TerminalMirror, TextDelta, TextJournal, TextStep,
};
