//! Spaces sidebar: the space-filter dropdown (searchable, with "All projects"),
//! the filtered Sessions list, and the native add-project picker.
//!
//! A space = a synced (device, folder) pair. Spaces stopped being a
//! navigation spine when tabs went device-local: the dropdown only FILTERS
//! the sidebar's session list (never the tab strip) and hosts space
//! management (add via the OS folder picker; rename/delete via row context
//! menus). Child module of `shell` so it renders straight off `Shell`'s
//! private state.

use super::*;
use gpui::FocusHandle;
use komet_proto::{ChatIndicator, Device, RepoRef, Space};

/// The space-filter dropdown, `Some` while open. The same searchable-menu
/// recipe as the composer's ref picker: filter input on top
/// (`PaletteSearch` context so ↑↓/⏎ bubble to the card), ranked substring
/// rows, keyboard highlight.
pub(super) struct SpacesMenu {
    search: Entity<ComposerInput>,
    page: SpacesMenuPage,
    /// Keyboard highlight within [`Shell::spaces_menu_rows`].
    active: usize,
    /// Tracked on the card — puts it on the keyboard dispatch path while the
    /// search input holds focus (the structure every working picker uses).
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SpacesMenuPage {
    #[default]
    Actions,
    Projects,
    Settings,
    Worktrees,
}

/// One row of the open dropdown, in display order.
#[derive(Clone, PartialEq)]
pub(crate) enum SpacesMenuRow {
    All,
    Space(String),
    AddSpace,
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request). Non-done statuses sit well below full
        // strength: at full alpha the colored words shouted across the
        // whole sidebar (user request) — only Done keeps its pop.
        ChatIndicator::Working => {
            theme.busy.opacity(0.55) // pink-400, muted
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.6),
        ChatIndicator::Errored => theme.danger.opacity(0.65),
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            theme.success.opacity(0.9) // emerald-400
        }
        ChatIndicator::Idle => crate::theme::ink(0.14),
    }
}

mod add;
mod edit;
mod menu;
mod rows;
