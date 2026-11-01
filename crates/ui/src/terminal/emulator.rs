//! The gpui side of the terminal seam.
//!
//! The emulator *mechanics* — grid, scrollback, selection, alt-screen,
//! truecolour cells, mouse-reporting modes, the byte feed, titles —
//! live in the `komet-term` crate, which has no gpui dependency so the engine can
//! feed the same state machine from its PTY pump. What stays here is the re-export
//! the panel and the view already spoke (so `crate::terminal::emulator::*` paths
//! keep resolving) and the shape of the seam:
//!
//! - **cells in, colors out**: [`komet_term::CellColor`] is a palette-free
//!   description of a cell; [`super::view::resolve_color`] turns it into an `Hsla`
//!   against the active theme, and `super::view` owns the glyph metrics, the cell
//!   rectangles, and the pointer to cell conversion.
//! - the panel keeps RPC, subscriptions, scheduling and settings; the view paints.
//!
//! Nothing here emulates anything. A change that needs to know how an escape
//! sequence behaves belongs in `crates/term`.

pub use komet_term::{
    BlockStatus, CellColor, CellSnapshot, CommandBlock, CursorSnapshot, Emulator, GridPoint,
    GridSize, MouseModes, MouseProtocol, SCROLLBACK_LINES, SelectionType, Side,
};
