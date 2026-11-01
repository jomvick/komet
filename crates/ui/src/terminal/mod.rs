//! Terminal panel (feature-inventory §1.10): an `alacritty_terminal`-backed
//! emulator fed by the engine's PTY stream over the generic RPC client.
//!
//! - [`emulator`] — the gpui seam over `komet_term`'s emulator core (the grid,
//!   selection and mouse modes it emulates live in `crates/term`, shared with the
//!   engine); this module only re-exports it,
//! - [`view`] — cell palette, keystroke→bytes encoding, input coalescing, and
//!   the custom grid-painting element;
//! - [`panel`] — session-scoped tabs, subscriptions with reconnect backoff,
//!   drag-reorder, the Cmd/Ctrl+J toggle, tab chrome (new/close/cycle/rename,
//!   useful titles) and font zoom; settings persist through the
//!   shell via [`panel::TerminalPanelEvent`].
//!
//! Method names come from `komet_rpc::methods` and wire types from
//! `komet_proto` (`TerminalSession`, `TerminalEvent`) — the same contract the
//! engine serves (feature-inventory §2.1).

pub mod emulator;
pub mod panel;
pub mod view;
