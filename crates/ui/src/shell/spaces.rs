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
use komet_proto::{ChatIndicator, Device, FolderListing, RepoRef, Space};

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
    /// "Import project…": copy a folder from another workspace.
    ImportProject,
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

/// The remote add-project browser, `Some` while open. Inside an environment
/// workspace the OS folder picker can't see the environment's filesystem, so
/// "+" opens this instead: LIST_FOLDERS on the env's engine, then createSpace
/// on its device.
pub(super) struct RemoteFolderBrowser {
    /// The environment device the new space is created on.
    device: Device,
    /// Folder requested; `None` = the device's home dir (engine default).
    path: Option<String>,
    listing: Option<FolderListing>,
    loading: bool,
    error: Option<String>,
    /// Bumped per navigation so a slow earlier listing can't overwrite a
    /// newer one.
    generation: u64,
    focus: FocusHandle,
    focus_pending: bool,
    list_scroll: gpui::ScrollHandle,
    import_destination: bool,
}

pub(super) use import::ImportDialog;

/// Built-in project marks are embedded vector assets so they render on systems
/// without an emoji font. Custom marks use a data URL stored with the project.
pub(super) fn project_logo_icon_id(logo: &str) -> Option<&'static str> {
    let logo = if let Some(logo) = logo.strip_prefix("builtin:") {
        logo
    } else {
        // Keep logos chosen with the previous emoji picker visible; the
        // platform font may not include those glyphs, so render their native
        // icon equivalents instead.
        match logo {
            "📁" => "folder",
            "💻" => "computer",
            "🚀" => "branch",
            "⭐" => "star",
            "🌐" => "web",
            "🎨" => "image",
            "📦" => "drive",
            "🔧" => "settings",
            "🧪" => "checklist",
            "📝" => "document",
            "🎮" => "terminal",
            "🧠" => "chat",
            _ => return None,
        }
    };
    match logo {
        "folder" => Some(crate::icons::FOLDER),
        "computer" => Some(crate::icons::MONITOR),
        "star" => Some(crate::icons::STAR),
        "web" => Some(crate::icons::GLOBAL),
        "terminal" => Some(crate::icons::TERMINAL),
        "image" => Some(crate::icons::IMAGE),
        "chat" => Some(crate::icons::CHAT_ROUND_LINE),
        "drive" => Some(crate::icons::HARD_DRIVE),
        "document" => Some(crate::icons::DOCUMENT),
        "branch" => Some(crate::icons::GIT_BRANCH),
        "settings" => Some(crate::icons::SETTINGS_MINIMALISTIC),
        "checklist" => Some(crate::icons::CHECKLIST),
        _ => None,
    }
}

pub(super) fn project_logo_image(logo: &str) -> Option<std::sync::Arc<gpui::Image>> {
    use base64::Engine as _;
    let (mime, encoded) = logo.strip_prefix("data:image/")?.split_once(";base64,")?;
    let format = match mime {
        "png" => gpui::ImageFormat::Png,
        "jpeg" => gpui::ImageFormat::Jpeg,
        "gif" => gpui::ImageFormat::Gif,
        "webp" => gpui::ImageFormat::Webp,
        "bmp" => gpui::ImageFormat::Bmp,
        "tiff" => gpui::ImageFormat::Tiff,
        _ => return None,
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    Some(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
}

pub(super) fn project_logo_element(
    logo: &str,
    size: gpui::Pixels,
    color: gpui::Hsla,
) -> gpui::AnyElement {
    if let Some(image) = project_logo_image(logo) {
        gpui::img(image).size(size).into_any_element()
    } else if let Some(icon) = project_logo_icon_id(logo) {
        crate::icons::icon(icon)
            .size(size)
            .text_color(color)
            .into_any_element()
    } else {
        div()
            .text_size(size)
            .child(logo.to_string())
            .into_any_element()
    }
}

mod add;
mod browse;
mod edit;
mod import;
mod menu;
mod rows;
