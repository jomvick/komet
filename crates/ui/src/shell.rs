//! The app shell (komet `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the connection gate.
//!
//! Layout is komet's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360–760px, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, App, Bounds, Context, Decorations, Empty, Entity, Focusable as _, IntoElement,
    MouseButton, MouseDownEvent, MouseUpEvent, ObjectFit, Pixels, Point, Render, ResizeEdge,
    SharedString, Size, StyleRefinement, StyledImage, Subscription, Task, Tiling, Window,
    WindowButton, WindowButtonLayout, WindowControlArea, actions, div, img, prelude::*, px,
};

use komet_engine::harness_cli::HarnessCliStatus;
use komet_proto::{AuthState, HarnessId, WorkspaceScope};
use komet_rpc::methods;

use crate::changes::{Changes, ChangesEvent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::files::{FilesCloseDisposition, FilesEvent, FilesSurface, WorkspacePathDrag};
use crate::icons::{self, icon};
use crate::motion::{self, AnimationExt as _, MotionSpec, RESIZE, TAB_SLIDE};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::settings::appearance::AppearancePage;
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::files::FilesSettingsPage;
use crate::settings::integrations::IntegrationsPage;
use crate::settings::memory::MemoryPage;
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::providers::ProvidersPage;
use crate::settings::security::{SecurityDefaults, SecurityEvent, SecurityPage};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::sync::{SyncEvent, SyncPage};
use crate::settings::usage::{UsageEvent, UsagePage};
use crate::settings::wallpaper;
use crate::settings::{
    RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, RememberedNavigation, SAVE_DEBOUNCE_MS,
    SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, TERMINAL_DEFAULT_HEIGHT, UiSettings,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, EngineMode, GatePhase, Indicator,
    RECONNECT_MAX_ATTEMPTS, format_ms, format_time_ago,
};
use crate::subagent::{SubagentEvent, SubagentOpen, SubagentPanel};
use crate::terminal::panel::{
    NewTerminalTab, TerminalPanel, TerminalPanelEvent, TerminalPrefs, ToggleTerminal,
    clamp_terminal_height,
};
use crate::theme::Theme;
use crate::transcript::{
    self, ErrorOpenAccounts, ErrorOpenModelPicker, ErrorRetryTurn, RevealLastCompaction,
    Transcript, TranscriptEvent,
};

/// Wider floor for the right pane while it shows the Files+FileEditor split
/// (tree column + content column side by side) — below this the content
/// column left after the fixed-width tree is too narrow to read. See
/// `Shell::right_target`/`files_split_active`.
const SPLIT_RIGHT_PANE_MIN: f32 = 560.0;

mod github;
mod spaces;
mod tabs;

use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(
    shell,
    [ToggleSidebar, ToggleChanges, AddSpacePalette, NewSession]
);

mod keymap;
pub use keymap::apply_keymap;
mod titlebar;
pub use titlebar::{
    CLUSTER_BUTTONS_WIDTH, cluster_buttons_start, cluster_clearance, titlebar_cluster_start,
    titlebar_spacer_width,
};

mod navigation;
pub use navigation::{
    CHAT_ROW_HEIGHT, ChatPanels, NavEntry, NavHistory, RESORT, RightSurface, Route,
    SIDEBAR_GLASS_FADE_BAND, SIDEBAR_LIST_GAP, SessionPanels, SettingsSection,
    push_unique_right_surface, resort_offsets, workspace_file_title,
};

mod gate;
use gate::{OrgGateUi, grid_backdrop};

mod sync;
pub use sync::*;

mod overlays;
pub use overlays::*;

mod sidebar;
pub use sidebar::*;

mod content;
pub use content::*;

mod right_pane;

mod terminal;

mod chat;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;

/// The dragged surface-tab payload (strip reorder).
#[allow(dead_code)]
struct RightTabDrag {
    panel_key: String,
    from: usize,
    title: SharedString,
    workspace_path: Option<WorkspacePathDrag>,
}

/// Live drag-over state for the surface-tab strip — the terminal drawer's
/// [`crate::terminal::panel`] DragState, ported: `epoch` keys the 150ms
/// slide-animation restarts as the hovered slot changes.
struct RightTabDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// Ghost chip following the pointer while a surface tab drags.
struct SurfaceTabGhost {
    title: SharedString,
}

impl Render for SurfaceTabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .h(px(24.0))
            .w(px(112.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(11.5))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
pub(super) struct WidthTween {
    pub(super) from: f32,
    pub(super) to: f32,
    pub(super) started: std::time::Instant,
}

impl WidthTween {
    pub(super) fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

/// The chat-row Rename dialog.
pub(super) struct RenameChatDialog {
    pub(super) chat_id: String,
    pub(super) input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    pub(super) focus_pending: bool,
    pub(super) _events: Subscription,
}

/// Transient "will attach to your next message" pill shown near a transcript
/// file drop (issue #44); `_task` clears it after [`DROP_CONFIRM_HOLD`].
pub(super) struct DropConfirm {
    /// Monotonic id so each drop replays the pill's entrance animation.
    pub(super) id: usize,
    /// Drop point relative to the conversation column.
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) _task: Task<()>,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    /// A file link clicked from an agent response. Rendering owns the window,
    /// so the subscription stores the intent until it can open and focus the
    /// corresponding editor surface on this frame.
    pending_workspace_file: Option<(String, Option<u32>)>,
    composer: Entity<Composer>,
    /// External file drag hovering the conversation column — shows the
    /// "Drop files or images to attach" veil; a drop stages them in the composer.
    file_drag_active: bool,
    /// Bounds of the conversation column during the active file drag — anchors
    /// the drop confirmation (issue #44); refreshed by every drag move.
    file_drag_bounds: Bounds<Pixels>,
    /// In-flight transcript-drop confirmation (issue #44).
    drop_confirm: Option<DropConfirm>,
    /// Monotonic id source for [`DropConfirm`]s.
    drop_confirm_seq: usize,
    /// Measured height of the bottom chrome stack (status strip + composer +
    /// terminal dock) the full-height transcript scrolls under — written by a
    /// paint-time canvas each frame, read the NEXT frame for the fade inset,
    /// the transcript's bottom clearance, and the jump pill's anchor (the
    /// same one-frame lag every fade here rides).
    bottom_stack: std::rc::Rc<std::cell::Cell<f32>>,
    /// The sidebar's archived accordion (t3code Sidebar): OPEN by default
    /// (user request), session-transient. `archived_shown` pages the
    /// expanded list ("Show more" reveals another page).
    pub(super) archived_open: bool,
    pub(super) archived_shown: usize,
    /// Archived slim row under the pointer — swaps its time label for the
    /// Unarchive affordance and restores the dimmed harness mark (t3code's
    /// settled-row hover).
    pub(super) archived_hover: Option<String>,
    /// "All projects" grouped view: space IDs whose folder is expanded.
    /// Absent = collapsed (the default on first launch); persisted in
    /// `UiSettings::expanded_spaces` and pruned with the spaces.
    pub(super) expanded_spaces: std::collections::HashSet<String>,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    /// Embedded terminal host for right-pane Terminal surfaces — a SEPARATE
    /// entity from the bottom drawer's (own PTYs, own grid geometry; one
    /// panel can only size one visible grid at a time).
    right_terminal: Option<Entity<TerminalPanel>>,
    /// Panel → shell chrome events (font zoom persistence, surface-tab
    /// bookkeeping). One subscription per panel entity.
    terminal_event_sub: Option<Subscription>,
    right_terminal_event_sub: Option<Subscription>,
    /// The surface-tab strip's `+` menu (Terminal / Files / Git / Browser rows).
    right_plus: popover::Popup<()>,
    /// Titlebar GitHub control: its dropdown + the checkout's status.
    github_menu: popover::Popup<()>,
    github: github::GitHubHeader,
    /// Diff surfaces by id — each tab its own [`Changes`] viewer with its own
    /// scope/base pick and diff watch (multiple diff panels, user request).
    diffs: std::collections::HashMap<u64, Entity<Changes>>,
    /// Event hookups for [`Self::diffs`] (History rows opening commit tabs).
    diff_subs: std::collections::HashMap<u64, Subscription>,
    diff_seq: u64,
    /// The headless [`Changes`] entity backing the composer's Changes chip.
    /// Created lazily from the strip; `add_diff_surface` takes it over so the
    /// chip and the opened pane share one watch.
    widget_changes: Option<Entity<Changes>>,
    /// The composer widget strip (Changes / subagents / processes chips).
    widgets: crate::composer_widgets::WidgetStrip,
    /// One-shot repaint armed for the soonest chip-hide deadline.
    widgets_wake: Option<Task<()>>,
    /// The chat the strip currently describes; a change resets popup + gates.
    widgets_chat: Option<String>,
    /// Subagent transcript surfaces by id — one per spawn chip clicked. The
    /// panel owns its own `WatchSubagent`/`WatchActivity` tasks, so closing the
    /// tab (dropping the entity) is what stops the subscription.
    subagent_tabs: std::collections::HashMap<u64, Entity<SubagentPanel>>,
    /// Event hookups for [`Self::subagent_tabs`] (a nested spawn opening its
    /// own tab, one level deeper).
    subagent_subs: std::collections::HashMap<u64, Subscription>,
    subagent_seq: u64,
    /// One Servo browser surface per chat/panel key. Dropping the entity stops
    /// its RPC client and terminates the child browser engine.
    browsers: std::collections::HashMap<String, Entity<crate::browser::BrowserSurface>>,
    /// One workspace browser per chat/panel key. Dropping the entity closes
    /// its file watcher and every in-flight workspace request.
    files: std::collections::HashMap<String, Entity<FilesSurface>>,
    files_subs: std::collections::HashMap<String, Subscription>,
    /// One independent editor/tree per opened workspace file. IDs are global
    /// while the lookup key keeps a file tab scoped to its chat panel.
    file_surfaces: std::collections::HashMap<u64, Entity<FilesSurface>>,
    file_surface_paths: std::collections::HashMap<u64, String>,
    file_surface_keys: std::collections::HashMap<(String, String), u64>,
    file_surface_subs: std::collections::HashMap<u64, Subscription>,
    file_surface_seq: u64,
    pending_file_closes: std::collections::HashSet<RightSurface>,
    window_close_pending: bool,
    /// Ordered surface tabs per panel key (drag-reorderable; stale entries —
    /// closed terminals/diffs — are skipped at read time).
    right_tabs: std::collections::HashMap<String, Vec<RightSurface>>,
    /// In-flight surface-tab drag (slide animation state).
    right_tab_drag: Option<RightTabDragState>,
    /// Surface-tab strip scroll (the strip overflows horizontally, t3
    /// ScrollArea-style; drag drop-math reads the offset back out).
    right_tab_scroll: gpui::ScrollHandle,
    /// Chat outlet vs settings pages.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    files_settings_page: Option<Entity<FilesSettingsPage>>,
    notifications_page: Option<Entity<NotificationsPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    providers_page: Option<Entity<ProvidersPage>>,
    usage_page: Option<Entity<UsagePage>>,
    memory_page: Option<Entity<MemoryPage>>,
    security_page: Option<Entity<SecurityPage>>,
    sync_page: Option<Entity<SyncPage>>,
    integrations_page: Option<Entity<IntegrationsPage>>,
    shortcuts_sub: Option<Subscription>,
    notifications_sub: Option<Subscription>,
    security_sub: Option<Subscription>,
    sync_sub: Option<Subscription>,
    usage_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: popover::Popup<(String, Point<Pixels>)>,
    rename_dialog: Option<RenameChatDialog>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Delete-confirm card keyboard focus: tracked on the card so Escape can
    /// dismiss it without the mouse; `delete_focus_pending` arms the initial
    /// focus on open (rename's dialog piggybacks on its focused input).
    delete_focus: gpui::FocusHandle,
    delete_focus_pending: bool,
    /// Space-row context menu (dropdown rows): (space id, window position).
    space_menu: popover::Popup<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// Same keyboard treatment as the chat delete card (see `delete_focus`).
    space_delete_focus: gpui::FocusHandle,
    space_delete_focus_pending: bool,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// The sidebar's space-filter dropdown.
    spaces_menu: popover::Popup<spaces::SpacesMenu>,
    /// The sidebar's workspace (device) switcher dropdown.
    devices_menu: popover::Popup<()>,
    /// Chat id whose STATUS CORNER is under the pointer — just that corner
    /// swaps to the archive button (t3code's settle-on-hover); hovering the
    /// row body leaves the status readable.
    chat_status_hover: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// The remembered device workspace has been reopened once (paired list landed).
    workspace_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, komet_proto::SessionStatus>,
    user_menu: popover::Popup<()>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Local lifecycle of an in-app update (macOS bundle swap) — the engine's
    /// UpdateStatus stream says WHETHER one exists; this says how far the
    /// download/stage of it has come in this process.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    /// Version whose update strip the user dismissed (advisory installs only —
    /// a newer release shows the strip again).
    update_dismissed: Option<String>,
    /// Version currently shown in the proactive update modal.
    update_modal_version: Option<String>,
    /// Newest version already processed for notification/modal detection.
    update_observed: Option<String>,
    /// Agent-CLI updates found by the launch check (home-page card). One pass
    /// per process — the engine spawns `<cli> --version` for every harness.
    cli_updates: Vec<HarnessCliStatus>,
    cli_check_started: bool,
    /// The card's × — session-scoped: a relaunch surfaces the same updates.
    cli_updates_dismissed: bool,
    /// Per-harness row lifecycle in the home card (idle/updating/updated/failed).
    cli_update_rows: HashMap<HarnessId, CliUpdateRow>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: komet_update::InstallKind,
    org: Option<OrgGateUi>,
    sync_flow: SyncFlow,
    mutate_task: Option<Task<()>>,
    auth_task: Option<Task<()>>,
    runtime_change_task: Option<Task<()>>,
    runtime_change_error: Option<SharedString>,
    /// The one-time local→synced import stream (switch wizard progress step).
    import_task: Option<Task<()>>,
    /// Title of the chat the import stream is copying right now.
    import_current: Option<SharedString>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Cached main-session metadata: the sidebar only re-sorts/rebuilds this
    /// when the visible chat set or filter changes.
    sidebar_active_cache: Option<(u64, Vec<SidebarSessionRow>)>,
    /// Last rendered sidebar order (key + estimated height) — the FLIP baseline
    /// for the §1.6 resort glide.
    sidebar_prev_order: Vec<(String, f32)>,
    /// Per-key paint offsets of the resort in flight, keyed elements restart on
    /// `resort_epoch` bumps.
    sidebar_resort: std::collections::HashMap<String, f32>,
    /// Keys that just appeared in a live list (fade in, no glide).
    sidebar_new_keys: std::collections::HashSet<String>,
    resort_epoch: usize,
    /// Last observed `window.is_window_active()` — rising edge fires a
    /// ProbeSync so a broadcast-deaf room heals as the user looks at the app.
    was_window_active: bool,
    /// Dev/testing knobs (`KOMET_OPEN_DIALOG`, `KOMET_FORCE_GATE`) — see
    /// [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    /// Changes-panel takeover (the header's expand button): the panel fills
    /// everything right of the sidebar and the conversation column collapses
    /// to zero. Session-local view state — never persisted, reset on close.
    right_pane_expanded: bool,
    /// Viewport width stamped each frame at render — the expanded panel's
    /// width target ([`Self::right_target`] has no `Window`).
    viewport_width: f32,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    save_task: Option<Task<()>>,
    /// Ticker that keeps the banner countdown moving. Armed only while
    /// `link_status()` is `Reconnecting` with a deadline still ahead of now —
    /// an idle app must not pay a notify every quarter-second.
    reconnect_ticker: Option<Task<()>>,
    /// Last link status seen by [`Shell::on_state_changed`]: the drain of the
    /// composer's offline outbox rides the edge into `Ready`, not every notify.
    last_connection: ConnectionStatus,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    _composer_events: Subscription,
    _pickers_observation: Subscription,
    _transcript_events: Subscription,
}

impl Shell {
    fn sidebar_cache_revision(&self, cx: &mut Context<Self>) -> u64 {
        let state = self.state.read(cx);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        for chat in state.visible_chats().filter(|c| !c.archived) {
            chat.id.hash(&mut hasher);
            chat.space_id.hash(&mut hasher);
            chat.device_id.hash(&mut hasher);
            chat.pinned.hash(&mut hasher);
            chat.last_message_at
                .map(|ts| ts.timestamp_millis())
                .hash(&mut hasher);
            chat.created_at.timestamp_millis().hash(&mut hasher);
            chat.branch.hash(&mut hasher);
        }

        self.settings.space_filter.hash(&mut hasher);
        hasher.finish()
    }

    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), cx));
        transcript.update(cx, |t, _| t.bind_composer(composer.downgrade()));
        let transcript_events = cx.subscribe(
            &transcript,
            |this: &mut Shell, _, event: &TranscriptEvent, cx| match event {
                TranscriptEvent::OpenWorkspaceFile { path, line } => {
                    this.pending_workspace_file = Some((path.clone(), *line));
                    cx.notify();
                }
                TranscriptEvent::OpenSubagent {
                    chat_id,
                    target_device_id,
                    spawn,
                } => {
                    let open = SubagentOpen {
                        chat_id: chat_id.clone(),
                        target_device_id: target_device_id.clone(),
                        spawn: spawn.clone(),
                    };
                    this.add_subagent_surface(open, cx);
                }
                // A Run chip's badge: the read-only processes popup above
                // the composer (retargeted from the old right-pane surface).
                // A chat with no commands shows no chip, so it opens nothing.
                TranscriptEvent::OpenAgentShell => {
                    if !this.transcript.read(cx).agent_shell_log().is_empty() {
                        crate::composer_widgets::retarget_agent_shell(&mut this.widgets);
                        cx.notify();
                    }
                }
            },
        );
        // Every send glides the prompt to the viewport top and reserves the
        // reply's space below it (notes-app parity).
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent {
                    chat_id,
                    message_id,
                } => {
                    transcript.update(cx, |t, cx| {
                        t.on_own_send(chat_id.clone(), message_id.clone(), cx)
                    });
                }
            }
        });
        let pickers = composer.read(cx).pickers().clone();
        let pickers_observation = cx.observe(&pickers, |_, _, cx| cx.notify());
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `KOMET_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("KOMET_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/providers") | Some("settings/agents") | Some("settings/harnesses") => {
                Route::Settings(SettingsSection::Providers)
            }
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/notifications") => Route::Settings(SettingsSection::Notifications),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            Some("settings/security") => Route::Settings(SettingsSection::Security),
            Some("settings/integrations") => Route::Settings(SettingsSection::Integrations),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            // Default startup opens the new-session canvas. Existing sessions
            // remain available in the sidebar and are opened explicitly.
            _ => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
        };
        // More capture knobs of the same kind: `KOMET_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `KOMET_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("KOMET_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("KOMET_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the komet engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(section),
        });
        Self {
            state,
            transcript,
            pending_workspace_file: None,
            composer,
            file_drag_active: false,
            file_drag_bounds: Bounds::default(),
            drop_confirm: None,
            drop_confirm_seq: 0,
            // Seed with the compact composer stack's rough height so the
            // first frame's clearance isn't zero (the measure corrects it).
            bottom_stack: std::rc::Rc::new(std::cell::Cell::new(120.0)),
            archived_open: true,
            archived_shown: 0,
            archived_hover: None,
            terminal: None,
            right_terminal: None,
            terminal_event_sub: None,
            right_terminal_event_sub: None,
            right_plus: popover::Popup::default(),
            github_menu: popover::Popup::default(),
            github: github::GitHubHeader::default(),
            diffs: std::collections::HashMap::new(),
            diff_subs: std::collections::HashMap::new(),
            diff_seq: 0,
            widget_changes: None,
            widgets: crate::composer_widgets::WidgetStrip::new(),
            widgets_wake: None,
            widgets_chat: None,
            subagent_tabs: std::collections::HashMap::new(),
            subagent_subs: std::collections::HashMap::new(),
            subagent_seq: 0,
            browsers: std::collections::HashMap::new(),
            files: std::collections::HashMap::new(),
            files_subs: std::collections::HashMap::new(),
            file_surfaces: std::collections::HashMap::new(),
            file_surface_paths: std::collections::HashMap::new(),
            file_surface_keys: std::collections::HashMap::new(),
            file_surface_subs: std::collections::HashMap::new(),
            file_surface_seq: 0,
            pending_file_closes: std::collections::HashSet::new(),
            window_close_pending: false,
            right_tabs: std::collections::HashMap::new(),
            right_tab_drag: None,
            right_tab_scroll: gpui::ScrollHandle::new(),
            route,
            nav,
            devices_page: None,
            archived_page: None,
            appearance_page: None,
            files_settings_page: None,
            notifications_page: None,
            shortcuts_page: None,
            providers_page: None,
            usage_page: None,
            memory_page: None,
            security_page: None,
            sync_page: None,
            integrations_page: None,
            shortcuts_sub: None,
            notifications_sub: None,
            security_sub: None,
            sync_sub: None,
            usage_sub: None,
            chat_menu: popover::Popup::default(),
            rename_dialog: None,
            delete_confirm: None,
            // Tracked on the delete-confirm card (like the add-space palette)
            // so Escape reaches it without the mouse — the rename dialog
            // gets its keys from its focused input instead.
            delete_focus: cx.focus_handle(),
            delete_focus_pending: false,
            space_menu: popover::Popup::default(),
            rename_space_dialog: None,
            delete_space_confirm: None,
            space_delete_focus: cx.focus_handle(),
            space_delete_focus_pending: false,
            add_space: None,
            spaces_menu: popover::Popup::default(),
            devices_menu: popover::Popup::default(),
            chat_status_hover: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            workspace_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            user_menu: popover::Popup::default(),
            sidebar_notice: None,
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_dismissed: None,
            update_modal_version: None,
            update_observed: None,
            cli_updates: Vec::new(),
            cli_check_started: false,
            cli_updates_dismissed: false,
            cli_update_rows: HashMap::new(),
            install: komet_update::detect_install(),
            org: None,
            sync_flow: SyncFlow::Idle,
            mutate_task: None,
            auth_task: None,
            runtime_change_task: None,
            runtime_change_error: None,
            import_task: None,
            import_current: None,
            boot,
            data_dir,
            expanded_spaces: settings.expanded_spaces.clone(),
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            sidebar_active_cache: None,
            sidebar_prev_order: Vec::new(),
            sidebar_resort: std::collections::HashMap::new(),
            sidebar_new_keys: std::collections::HashSet::new(),
            resort_epoch: 0,
            was_window_active: false,
            debug_dialog,
            debug_gate,
            sidebar_tween: None,
            right_tween: None,
            right_pane_expanded: false,
            viewport_width: 1280.0,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            save_task: None,
            reconnect_ticker: None,
            last_connection: ConnectionStatus::Connecting,
            focus_sub: None,
            _ticker: ticker,
            _state_observation: observation,
            _composer_events: composer_events,
            _pickers_observation: pickers_observation,
            _transcript_events: transcript_events,
        }
    }

    // ---- observation ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        let update_status = state.read(cx).update.clone();
        if let Some(status) = update_status.as_ref()
            && let Some(version) = newly_detected_update(self.update_observed.as_deref(), status)
        {
            self.update_observed = Some(version.clone());
            if update_modal_should_show(&version, self.update_dismissed.as_deref(), status) {
                self.update_modal_version = Some(version.clone());
            }
            if update_notification_allowed(
                self.settings.notifications_enabled,
                self.settings.notifications_background_only,
                cx.active_window().is_some(),
            ) {
                crate::notify::post(
                    "Komet update available",
                    &format!("Version {version} is ready to review"),
                );
            }
            cx.notify();
        }
        // Launch check for the agent CLIs: once, as soon as the runtime is
        // Ready (before that the RPC has nowhere to land). The card renders
        // from whatever comes back; an empty list shows nothing.
        if !self.cli_check_started && matches!(state.read(cx).gate(), GatePhase::Ready) {
            self.check_harness_updates(cx);
        }
        let next_sync_flow = {
            let state = state.read(cx);
            sync_flow_after_auth(self.sync_flow, state.workspace_scope, state.auth.as_ref())
        };
        if next_sync_flow != self.sync_flow {
            self.sync_flow = next_sync_flow;
            if matches!(
                self.sync_flow,
                SyncFlow::RestartPending { .. } | SyncFlow::SwitchOffer { .. }
            ) {
                self.org = None;
            }
        }
        // The in-place local→synced switch: once the replacement runtime is
        // attached and Ready, kick the import (or finish) from here.
        self.drive_sync_switch(cx);
        let signed_out_synced = {
            let state = state.read(cx);
            state.workspace_scope == Some(WorkspaceScope::Synced)
                && matches!(state.auth, Some(AuthState::SignedOut))
        };
        // AuthStatus is shared by every viewport. Whichever viewport owns the
        // embedded runtime drains it; remote viewports request daemon shutdown
        // and all of them independently reattach to the new local runtime.
        if signed_out_synced && self.runtime_change_task.is_none() {
            self.start_local_runtime_transition(false, cx);
        }
        // Capture knob: the add-space palette needs only the device registry.
        if self.debug_dialog.as_deref() == Some("add-space") && !state.read(cx).devices.is_empty() {
            self.debug_dialog = None;
            self.open_add_space(cx);
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                    self.delete_focus_pending = true;
                }
                _ => {}
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent. Desktop banners
        // (`notify::post`) ride the SAME edges and gates behind their own
        // settings flag — one detector, two outputs, so the banner can never
        // fire where the chime wouldn't.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        //
        // SEND-PENDING-GATED too (`AppState::send_pending`): a send whose
        // queued command the host hasn't executed yet can still surface a
        // phantom Working→Idle (a stale Working row crossing the 45s gate on
        // the send's own re-render, or a late old Idle row) — the done-chime
        // stays quiet for that chat until the host acks, while the baseline
        // keeps tracking silently so the ghost edge never fires later. The
        // question chime is NOT gated: an instant AwaitingInput ack should
        // still ring.
        {
            let now = Utc::now();
            type Ping = (String, komet_proto::SessionStatus, bool, Option<String>);
            let sessions: Vec<Ping> = {
                let state = state.read(cx);
                state
                    .sessions
                    .iter()
                    .map(|s| {
                        use komet_proto::view::Indicator;
                        let status = match komet_proto::view::effective_indicator(Some(s), now) {
                            Indicator::Working => komet_proto::SessionStatus::Working,
                            Indicator::AwaitingInput => komet_proto::SessionStatus::AwaitingInput,
                            Indicator::Errored => komet_proto::SessionStatus::Errored,
                            Indicator::None => komet_proto::SessionStatus::Idle,
                        };
                        let send_pending = state.send_pending(&s.chat_id, now);
                        let title = state
                            .chats
                            .iter()
                            .find(|c| c.id == s.chat_id)
                            .and_then(|c| c.title.clone());
                        (s.chat_id.clone(), status, send_pending, title)
                    })
                    .collect()
            };
            // Background-only banners: `active_window()` is app-level (any
            // Komet window being key), so a ping for a *background chat* in a
            // focused app still stays a chime — you're already looking at
            // Komet; the sidebar dot carries the rest.
            let app_focused = cx.active_window().is_some();
            for (chat_id, status, send_pending, title) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                    && !(send_pending && sound == crate::sound::Sound::Done)
                {
                    if self.settings.sound_enabled {
                        crate::sound::play(sound);
                    }
                    if self.settings.notifications_enabled
                        && !(self.settings.notifications_background_only && app_focused)
                    {
                        let title = title.unwrap_or_else(|| "New session".into());
                        let body = match sound {
                            crate::sound::Sound::Done => "Run finished",
                            crate::sound::Sound::Request => "Waiting on your input",
                        };
                        crate::notify::post(&title, body);
                    }
                }
            }
        }
        // Boot: reopen on the device the sidebar was showing last time, once the
        // paired-device list has actually arrived (before that frame every
        // remote row is unknown, and switching to a vanished device would be a
        // silent drop to an empty workspace).
        if !self.workspace_boot_applied && state.read(cx).env_list_synced {
            self.workspace_boot_applied = true;
            let target = self.settings.active_workspace.clone();
            let (current, exists) = {
                let state = state.read(cx);
                (
                    state.active_workspace.clone(),
                    target
                        .as_deref()
                        .is_some_and(|id| state.workspace_row(id).is_some()),
                )
            };
            if exists
                && Some(current.as_str()) != target.as_deref()
                && let Some(target) = target
            {
                self.select_workspace(&target, cx);
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none() {
                // A set sidebar filter is an explicit standing choice — the
                // canvas defaults (project AND its device) follow it, even
                // over a remembered "no project" opt-out. Otherwise the last
                // selected project stands, unless opted out.
                let exists = |id: &String| state.read(cx).space_row(id).is_some();
                let filter = self.settings.space_filter.clone().filter(exists);
                let target = match filter {
                    Some(filter) => Some(filter),
                    None if !state.read(cx).no_project => {
                        self.settings.last_space_id.clone().filter(exists)
                    }
                    None => None,
                };
                if target.is_some() {
                    state.update(cx, |s, cx| s.select_space(target, cx));
                }
            }
        }
        // Persist the selected space (the new-tab fallback under "All").
        {
            let selected_space = state.read(cx).selected_space.clone();
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Boot landing: the most recent session once the first chats frame
        // syncs (manual selection wins).
        self.boot_select_chat(cx);
        // Prune stale remembered destinations (like open_tabs pruning).
        if self.prune_remembered_navigation(cx) {
            self.schedule_save(cx);
        }
        // Keep NewTask remembered project in sync while staying on the canvas.
        if self.active_chat.is_empty() {
            self.remember_current_navigation(cx);
        }
        // Heal a dangling sidebar filter (space deleted, possibly elsewhere):
        // fall back to "All" rather than filtering everything out.
        if state.read(cx).spaces_synced
            && let Some(filter) = self.settings.space_filter.clone()
            && state.read(cx).space_row(&filter).is_none()
        {
            self.settings.space_filter = None;
            self.schedule_save(cx);
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // komet's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open
                && let RightSurface::Diff(id) = self.resolved_right_active(cx)
                && let Some(changes) = self.diffs.get(&id).cloned()
            {
                changes.update(cx, |changes, cx| changes.ensure_content(cx));
            }
            self.remember_current_navigation(cx);
        }
        // Task 10: a reconnect keeps the frame up, so the only new surface is
        // the banner's countdown — and the messages the composer parked while
        // the link was down go back out on the edge into Ready, once (the drain
        // is a no-op on an empty outbox, so the first boot costs nothing).
        let link = state.read(cx).link_status();
        self.drive_reconnect_ticker(&link, cx);
        if link == ConnectionStatus::Ready && self.last_connection != ConnectionStatus::Ready {
            self.composer
                .update(cx, |composer, cx| composer.drain_queued_sends(cx));
        }
        self.last_connection = link;
    }

    /// Is an automatic retry still due? `Reconnecting` whose deadline has
    /// passed is a chain waiting on the engine's own frame — there is nothing
    /// left to count down to, so the ticker must not ride it.
    fn reconnect_countdown_live(&self, cx: &App) -> bool {
        let state = self.state.read(cx);
        match state.link_status() {
            ConnectionStatus::Reconnecting { .. } => {
                state.reconnect_deadline.is_some_and(|due| due > Utc::now())
            }
            _ => false,
        }
    }

    /// Arm/disarm the countdown ticker. It exists only to make the banner's
    /// "retrying in …" move between engine frames; the moment the link leaves
    /// `Reconnecting` (or its deadline passes) the task is dropped, so a
    /// healthy app never polls.
    fn drive_reconnect_ticker(&mut self, link: &ConnectionStatus, cx: &mut Context<Self>) {
        let pending = matches!(link, ConnectionStatus::Reconnecting { .. })
            && self.reconnect_countdown_live(cx);
        match (pending, self.reconnect_ticker.is_some()) {
            (true, false) => {
                self.reconnect_ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(RECONNECT_TICK).await;
                        let alive = this.update(cx, |shell: &mut Shell, cx| {
                            if !shell.reconnect_countdown_live(cx) {
                                // The task is exiting; release the slot so the
                                // next attempt can arm a fresh ticker.
                                shell.reconnect_ticker = None;
                                return false;
                            }
                            cx.notify();
                            true
                        });
                        if !alive.unwrap_or(false) {
                            break;
                        }
                    }
                }));
            }
            (false, true) => {
                self.reconnect_ticker = None;
            }
            _ => {}
        }
    }

    // ---- layout state ----

    pub(super) fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // komet caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    pub(super) fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            // Re-stamp the appearance from the global before writing. The View
            // menu changes it through `appearance::set_mode`, which never touches
            // this shell's in-memory copy — without this, the next pane resize
            // would quietly write the boot-time appearance back over the user's
            // choice.
            let Ok(snapshot) = this.update(cx, |shell, cx| {
                shell.settings.appearance = crate::appearance::mode(cx);
                shell.settings.clone()
            }) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `KOMET_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    pub(super) fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    fn check_harness_updates(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let target = self.state.read(cx).harness_target_device();
        self.cli_check_started = true;
        cx.spawn(async move |this, cx| {
            let mut check_latest = true;
            let mut retry_in = HARNESS_RETRY_FIRST;
            loop {
                let mut params = serde_json::json!({ "checkLatest": check_latest });
                if let (Some(target), Some(object)) = (target.as_ref(), params.as_object_mut()) {
                    object.insert("targetDeviceId".into(), serde_json::json!(target));
                }
                let result = engine
                    .client()
                    .call(methods::GET_HARNESS_CLI_STATUS, params)
                    .await;
                let mut retry = true;
                let mut updates = None;
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessCliStatus>>(value) {
                        Ok(statuses) => {
                            retry = harness_check_needs_retry(&statuses);
                            let enabled = enabled_harness_ids(&engine, target.as_deref()).await;
                            updates = Some(harness_updates(statuses, enabled.as_ref()));
                            // Retry with a fresh feed check so a cached feed
                            // error does not repeat; otherwise read the cache.
                            check_latest = retry;
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "harness CLI update status response was invalid"
                            );
                            check_latest = true;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "could not check harness CLI updates");
                        check_latest = true;
                    }
                }
                let closed = this
                    .update(cx, |shell, cx| {
                        if let Some(updates) = updates {
                            let present: HashSet<HarnessId> =
                                updates.iter().map(|status| status.harness).collect();
                            shell
                                .cli_update_rows
                                .retain(|harness, _| present.contains(harness));
                            shell.cli_updates = updates;
                            cx.notify();
                        }
                    })
                    .is_err();
                if closed {
                    tracing::debug!("harness CLI update check completed after shell closed");
                    return;
                }
                if !retry {
                    return;
                }
                cx.background_executor().timer(retry_in).await;
                retry_in = (retry_in * 2).min(HARNESS_RETRY_MAX);
            }
        })
        .detach();
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    /// Build the application tree: wallpaper backdrop, the auth/org/error
    /// gate switch and the platform caption chrome above it.
    fn render_root(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx);
        let (shell_surface, text, font) = (theme.surface, theme.text, theme.font_sans.clone());
        let restart_required = self.sync_flow == SyncFlow::SignedOutRestartRequired;
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    Route::Chat => window.focus(&this.composer.focus_handle(cx), cx),
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if !restart_required
            && matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat)
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(shell_surface)
            .text_color(text)
            .font_family(font)
            .text_size(px(14.0))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (komet __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            // A new-tab request from the EMBEDDED panel deliberately bubbles
            // here (the panel lets it pass): the `+` button and the keyboard
            // equivalent must open a fresh surface tab, not just an in-panel
            // tab. The drawer panel stops propagation and opens in-panel, so
            // this handler only ever sees embedded requests.
            .on_action(cx.listener(|this, _: &NewTerminalTab, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.add_terminal_surface(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            // New session works from anywhere — `open_new_session` routes back
            // to chat itself, so Settings is not a dead spot.
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.open_new_session(cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            // Error-chip actions (transcript rows): Auth opens the Providers
            // settings section, Billing/ModelUnavailable opens the model
            // picker, and Réessayer resubmits the last prompt verbatim.
            .on_action(cx.listener(|this, _: &ErrorOpenAccounts, _, cx| {
                this.open_settings(SettingsSection::Providers, cx);
            }))
            .on_action(cx.listener(|this, _: &ErrorOpenModelPicker, window, cx| {
                let composer = this.composer.clone();
                composer.update(cx, |composer, cx| {
                    let pickers = composer.pickers().clone();
                    pickers.update(cx, |pickers, cx| {
                        pickers.open_model_menu(window, cx);
                    });
                });
            }))
            .on_action(cx.listener(|this, _: &ErrorRetryTurn, _, cx| {
                let composer = this.composer.clone();
                composer.update(cx, |composer, cx| {
                    composer.resubmit_last(cx);
                });
            }))
            // The Context popover's « See in thread »: the popover is about the
            // gauge, the marker is about the history — dismiss, then land on it.
            .on_action(cx.listener(|this, _: &RevealLastCompaction, _, cx| {
                let composer = this.composer.clone();
                composer.update(cx, |composer, cx| {
                    let pickers = composer.pickers().clone();
                    pickers.update(cx, |pickers, cx| pickers.close(cx));
                });
                this.transcript.update(cx, |transcript, cx| {
                    transcript.reveal_last_compaction(cx);
                });
            }));
        // Whole-window scope paints behind everything; the in-card scopes
        // paint inside the main card instead (see `panel_wallpaper` below).
        let wallpaper_scope = wallpaper::scope(cx);
        let root = if let Some(path) = wallpaper::path(cx)
            .filter(|path| path.is_file() && wallpaper_scope == wallpaper::WallpaperScope::Window)
        {
            root.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .child(img(path).size_full().object_fit(ObjectFit::Cover)),
            )
        } else {
            root
        };

        let render_gate = if restart_required {
            GatePhase::Loading
        } else if matches!(&gate, GatePhase::Loading) {
            // Connecting paints the normal frame: the window opens straight
            // into the shell and rows fill in as the engine attaches, instead
            // of a boot splash covering an empty window.
            GatePhase::Ready
        } else {
            gate.clone()
        };
        let root = match &render_gate {
            GatePhase::Ready => self.render_ready_page(root, window, cx),
            // Only the sign-out restart lands here; a normal Connecting boot
            // renders the Ready page above.
            GatePhase::Loading => root,
            GatePhase::OrgGate => {
                let card = self.render_org_gate(window, cx);
                root.child(card)
            }
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };
        let root = if restart_required {
            let restart = self.render_signed_out_restart(cx);
            root.child(restart)
        } else {
            root
        };

        // Manual tweens and hover fades share this root's vsync cadence;
        // looping loaders run on the slower pulse ticker (`motion`).
        if self.motion_active.get() | motion::hover_frame(cx.entity_id(), cx) {
            window.request_animation_frame();
        }

        // Caption controls are shell-level chrome, not Ready-page content:
        // keep them above every auth/org/error gate as well as the full
        // application. Gate pages also need a native drag surface
        // because they do not render the unified tabs/settings titlebar.
        let root = if (!restart_required && matches!(gate, GatePhase::Ready))
            || !cfg!(target_os = "windows")
        {
            root
        } else {
            root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(Theme::TITLEBAR_HEIGHT))
                    .window_control_area(WindowControlArea::Drag),
            )
        };
        root.children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx))
    }

    /// The application page behind the Ready gate: titlebar, sidebar, workspace
    /// card and overlays.
    fn render_ready_page(
        &mut self,
        root: gpui::Stateful<gpui::Div>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = Theme::of(cx);
        let (shell_surface, workspace_bg) = (theme.surface, theme.bg);
        let wallpaper_scope = wallpaper::scope(cx);
        // Focus is a sync signal: on the rising edge of window
        // activation, nudge every open room to verify liveness — a
        // broadcast-deaf socket (accepted writes, runtime pongs,
        // nothing delivered; 2026-08-04 incident) then heals within
        // seconds of the user looking at the app rather than waiting
        // out the background probe cadence.
        let window_active = window.is_window_active();
        if window_active && !self.was_window_active {
            self.state.update(cx, |s, cx| s.probe_sync(cx));
        }
        self.was_window_active = window_active;
        // A run finishing while you're LOOKING at the session must not
        // badge "completed" until you leave and return — mark it seen
        // live while the window is active (idempotent guard inside;
        // one extra frame settles it).
        if window_active {
            let unseen_selected = {
                let s = self.state.read(cx);
                s.selected_chat_row()
                    .filter(|c| c.unseen())
                    .map(|c| c.id.clone())
            };
            if let Some(chat_id) = unseen_selected {
                self.state
                    .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
            }
        }
        // Capture knob: `KOMET_OPEN_DIALOG=model` pops the combined
        // harness/model menu (needs `window`, so it fires here rather
        // than in `on_state_changed`).
        if self.debug_dialog.as_deref() == Some("model") {
            self.debug_dialog = None;
            self.composer
                .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
        }
        // MessageRail width gate: hide below 48rem of main-panel width.
        let viewport = f32::from(window.viewport_size().width);
        // Stamped for `right_target` — the expanded changes panel
        // sizes itself to the viewport.
        self.viewport_width = viewport;
        let main_width = (viewport - self.sidebar_target() - self.right_target(cx) - 10.0).max(0.0);
        // Clearance excludes the terminal dock: the transcript
        // viewport ends at the dock's top (see the underlay in
        // `render_main`), so only the chrome above it overlaps.
        let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
        let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
        self.transcript.update(cx, |t, cx| {
            t.set_rail_enabled(rail::rail_visible(main_width), cx);
            t.set_bottom_clearance(stack_h, cx);
        });

        let sidebar = self.render_sidebar(window, cx);
        let sidebar_handle = self.resize_handle(
            "sidebar-resize",
            || SidebarResize,
            |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
            cx,
        );
        let main = self.render_main(window, cx);
        // The Changes pane is chat-scoped chrome: the Settings route
        // never renders it (komet __root.tsx `!isSettings && activeChat`
        // around the diff column) — the per-session open flags stay
        // intact for the return trip.
        let on_chat = matches!(self.route, Route::Chat);
        let right: AnyElement = if on_chat {
            self.render_right_pane(cx)
        } else {
            Empty.into_any_element()
        };
        let overlays = self.render_overlays(window.viewport_size(), window, cx);
        // Copied out (not held) — `render_title_bar` needs `cx` mutable.
        let border_color = Theme::of(cx).border;
        // The workspace is one inset surface: the transcript and any
        // open side panel share its rounded boundary.
        let panel_wallpaper = wallpaper::path(cx)
            .filter(|path| path.is_file())
            .and_then(|path| wallpaper::card_layer(path, wallpaper_scope, workspace_bg));
        let workspace: AnyElement = div()
            .flex_1()
            .min_w_0()
            .mt(px(8.0))
            .mb(px(8.0))
            .ml(px(8.0))
            .mr(px(8.0))
            .flex()
            .flex_row()
            .overflow_hidden()
            .rounded(px(Theme::PANEL_RADIUS))
            .border_1()
            .border_color(border_color)
            .bg(workspace_bg)
            .children(panel_wallpaper)
            .child(main)
            .child(right)
            .into_any_element();
        // The banner is a row ABOVE the workspace, not an overlay on it:
        // the frame keeps painting and nothing has to be restored when
        // the link comes back.
        let card: AnyElement = match self.render_reconnect_banner(cx) {
            Some(banner) => div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(banner)
                .child(workspace)
                .into_any_element(),
            None => workspace,
        };
        // The whole app page is one keyed `animate-in` entrance (komet
        // App.tsx `<div key={phase} className="animate-in h-full">`):
        // arriving from any gate fades the page in.
        // The sidebar resize handle FLOATS over the sidebar/card seam
        // (zero layout width, same idiom as the changes-pane grabber)
        // so the sidebar's right gutter stays exactly as wide as its
        // left one — a 5px flex child here read as lopsided spacing.
        let sidebar_seam = div()
            .w(px(0.0))
            .h_full()
            .flex_none()
            .relative()
            .child(sidebar_handle.absolute().top_0().bottom_0().left(px(-2.0)));
        let (linux_left_controls, _) = self.linux_titlebar_controls_width(window, cx);
        let title_bar = self.render_title_bar(window, cx);
        // The sidebar and titlebar share the shell surface. Its width
        // rides the same tween as the sidebar, so the tone melts away
        // with the collapse instead of vanishing in a frame.
        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        // The sidebar and titlebar are one uninterrupted shell surface.
        let sidebar_tone = div()
            .absolute()
            .top(px(Theme::TITLEBAR_HEIGHT))
            .bottom_0()
            .left_0()
            .w(px(sidebar_now))
            .bg(shell_surface);
        let titlebar_tone = div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .bg(shell_surface);
        // Start the content row below the unified titlebar. The
        // sidebar stays flush with the shell; the workspace gets its
        // own inset and rounded boundary.
        let explorer: AnyElement = Empty.into_any_element();
        let page = div()
            .size_full()
            .relative()
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .pt(px(Theme::TITLEBAR_HEIGHT))
                    .child(sidebar)
                    .child(sidebar_seam)
                    .child(explorer)
                    .child(card),
            )
            .child(div().absolute().top_0().left_0().right_0().child(title_bar))
            .child(self.render_titlebar_cluster(linux_left_controls, cx))
            .children(overlays);
        root.child(sidebar_tone)
            .child(titlebar_tone)
            .child(motion::fade_in("phase-app", page))
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `KOMET_WINDOW_STATS=1`: this is the window's only root render, so it
        // runs exactly once per frame — the place to measure the frame budget
        // (interval between passes, tree build and child renders; see
        // `crate::perf`). Inactive windows are throttled to ~30 fps by gpui, so
        // their intervals are excluded rather than scored as missed.
        let _frame = crate::perf::frame(window.is_window_active());
        if let Some((path, line)) = self.pending_workspace_file.take() {
            if !self.right_pane_open(cx) {
                self.toggle_right_pane(cx);
            }
            self.add_file_surface(path, line, window, cx);
        }
        if self.window_close_pending && self.all_file_edits_flushed(cx) {
            self.window_close_pending = false;
            window.defer(cx, |window, _| window.remove_window());
        }
        let (workspace_scope, auth) = {
            let state = self.state.read(cx);
            (state.workspace_scope, state.auth.clone())
        };
        self.sync_flow = sync_flow_after_auth(self.sync_flow, workspace_scope, auth.as_ref());
        let root = self.render_root(window, cx);

        if cfg!(target_os = "linux")
            && let Decorations::Client { tiling } = window.window_decorations()
        {
            // Komet paints its opaque surface edge-to-edge: there is no
            // transparent shadow margin for the platform to inset. The 10px
            // client inset only shrank the interactive area, so keep it at
            // zero and let the strips below own the resize edges.
            const RESIZE_STRIP: f32 = 6.0;
            window.set_client_inset(px(0.));
            let root = root
                .when(!tiling.top && !tiling.left, |el| {
                    el.rounded_tl(px(Theme::PANEL_RADIUS))
                })
                .when(!tiling.top && !tiling.right, |el| {
                    el.rounded_tr(px(Theme::PANEL_RADIUS))
                })
                .when(!tiling.bottom && !tiling.left, |el| {
                    el.rounded_bl(px(Theme::PANEL_RADIUS))
                })
                .when(!tiling.bottom && !tiling.right, |el| {
                    el.rounded_br(px(Theme::PANEL_RADIUS))
                })
                .overflow_hidden();
            let resize_hitbox = |edge: ResizeEdge, strip: gpui::Div| {
                let strip = match edge {
                    ResizeEdge::Top => strip.cursor_n_resize(),
                    ResizeEdge::Bottom => strip.cursor_s_resize(),
                    ResizeEdge::Left => strip.cursor_w_resize(),
                    ResizeEdge::Right => strip.cursor_e_resize(),
                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => strip.cursor_nwse_resize(),
                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => strip.cursor_nesw_resize(),
                };
                strip.block_mouse_except_scroll().on_mouse_down(
                    MouseButton::Left,
                    move |event, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        if client_resize_edge(event.position, size, RESIZE_STRIP, tiling)
                            == Some(edge)
                        {
                            window.start_window_resize(edge);
                        }
                    },
                )
            };
            let hitboxes = div()
                .absolute()
                .inset_0()
                .when(!tiling.top, |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::Top,
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(RESIZE_STRIP)),
                    ))
                })
                .when(!tiling.bottom, |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::Bottom,
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(px(RESIZE_STRIP)),
                    ))
                })
                .when(!tiling.left, |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::Left,
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(px(RESIZE_STRIP)),
                    ))
                })
                .when(!tiling.right, |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::Right,
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(px(RESIZE_STRIP)),
                    ))
                })
                .when(!(tiling.top || tiling.left), |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::TopLeft,
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w(px(RESIZE_STRIP))
                            .h(px(RESIZE_STRIP)),
                    ))
                })
                .when(!(tiling.top || tiling.right), |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::TopRight,
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(RESIZE_STRIP))
                            .h(px(RESIZE_STRIP)),
                    ))
                })
                .when(!(tiling.bottom || tiling.left), |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::BottomLeft,
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .w(px(RESIZE_STRIP))
                            .h(px(RESIZE_STRIP)),
                    ))
                })
                .when(!(tiling.bottom || tiling.right), |el| {
                    el.child(resize_hitbox(
                        ResizeEdge::BottomRight,
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .w(px(RESIZE_STRIP))
                            .h(px(RESIZE_STRIP)),
                    ))
                });
            // Content first, strips last: the strips must be hit-tested above
            // the sidebar/titlebar/cards, which `.occlude()` their own area.
            div()
                .size_full()
                .child(root)
                .child(hitboxes)
                .into_any_element()
        } else {
            root.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests;
