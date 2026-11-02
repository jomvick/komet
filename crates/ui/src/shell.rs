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
    AnyElement, App, Context, Decorations, Empty, Entity, Focusable as _, IntoElement,
    MouseButton, MouseDownEvent, MouseUpEvent, ObjectFit, Pixels, Point, Render,
    ResizeEdge, SharedString, Size, StyleRefinement, StyledImage, Subscription, Task, Tiling,
    Window, WindowButton, WindowButtonLayout, WindowControlArea, actions, div, img, prelude::*, px,
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
use crate::settings::notifications::{NotificationsEvent, NotificationsPage};
use crate::settings::providers::ProvidersPage;
use crate::settings::security::{SecurityDefaults, SecurityEvent, SecurityPage};
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::sync::{SyncEvent, SyncPage};
use crate::settings::wallpaper;
use crate::settings::{
    RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, RememberedNavigation,
    SAVE_DEBOUNCE_MS, SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, TERMINAL_DEFAULT_HEIGHT,
    UiSettings,
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
    cluster_buttons_start, cluster_clearance, titlebar_cluster_start, titlebar_spacer_width,
    CLUSTER_BUTTONS_WIDTH,
};

mod navigation;
pub use navigation::{
    CHAT_ROW_HEIGHT, ChatPanels, NavEntry, NavHistory, RESORT, RightSurface, Route, SessionPanels,
    SettingsSection, SIDEBAR_GLASS_FADE_BAND, SIDEBAR_LIST_GAP, push_unique_right_surface,
    resort_offsets, workspace_file_title,
};

mod gate;
use gate::{OrgGateUi, grid_backdrop};

mod sync;
pub use sync::*;

mod overlays;
pub use overlays::*;

mod sidebar;
pub use sidebar::*;


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
    /// The surface-tab strip's `+` menu (Terminal / Git diff rows).
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
    /// Subagent transcript surfaces by id — one per spawn chip clicked. The
    /// panel owns its own `WatchSubagent`/`WatchActivity` tasks, so closing the
    /// tab (dropping the entity) is what stops the subscription.
    subagent_tabs: std::collections::HashMap<u64, Entity<SubagentPanel>>,
    /// Event hookups for [`Self::subagent_tabs`] (a nested spawn opening its
    /// own tab, one level deeper).
    subagent_subs: std::collections::HashMap<u64, Subscription>,
    subagent_seq: u64,
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
    security_page: Option<Entity<SecurityPage>>,
    sync_page: Option<Entity<SyncPage>>,
    integrations_page: Option<Entity<IntegrationsPage>>,
    shortcuts_sub: Option<Subscription>,
    notifications_sub: Option<Subscription>,
    security_sub: Option<Subscription>,
    sync_sub: Option<Subscription>,
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
                // A Run chip's badge: the read-only shell log of this chat.
                TranscriptEvent::OpenAgentShell => {
                    this.add_agent_shell_surface(cx);
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
            subagent_tabs: std::collections::HashMap::new(),
            subagent_subs: std::collections::HashMap::new(),
            subagent_seq: 0,
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
            security_page: None,
            sync_page: None,
            integrations_page: None,
            shortcuts_sub: None,
            notifications_sub: None,
            security_sub: None,
            sync_sub: None,
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

    /// Whether the right pane shows. NOT gated on git any more: the pane is
    /// a surface HOST now (terminals work in any space), so only the Git
    /// surface rows check `space_git_detected`. Also available on the
    /// new-session canvas now (user request, reversing an earlier one) —
    /// `panel_key` already keys canvas state under `space-canvas:{id}`, so
    /// per-panel storage (tabs, changes_open, right_active) works
    /// identically whether or not a chat is active.
    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if !self.right_pane_open(cx) {
            0.0
        } else if self.right_pane_expanded {
            // Takeover: everything right of the sidebar; the conversation
            // column (flex_1) collapses to zero behind it. Rides the sidebar's
            // width tween so a sidebar toggle mid-takeover stays seamless.
            let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
            (self.viewport_width - sidebar_now).max(RIGHT_PANE_MIN)
        } else {
            // The Files+FileEditor split (render_right_pane) spends a fixed
            // 220px on the tree column; below SPLIT_RIGHT_PANE_MIN total, the
            // remaining content column is too narrow to read (user report:
            // truncated/wrapped-to-nothing file preview). Only the split
            // needs the wider floor — Diff/Terminal/a lone FileEditor stay at
            // the normal minimum, and a user's wider manual resize always wins
            // via `.max`.
            let min = if self.files_split_active(cx) {
                SPLIT_RIGHT_PANE_MIN
            } else {
                RIGHT_PANE_MIN
            };
            self.settings.right_pane_width.max(min)
        }
    }

    /// Whether `render_right_pane` will draw the tree+content split for the
    /// panel key ~as `right_target` sees it: a Files surface tab exists
    /// alongside whichever surface is active, and that active surface isn't
    /// the tree itself (matches the `files_panel` condition in
    /// `render_right_pane` by construction, not a duplicated match).
    fn files_split_active(&self, cx: &App) -> bool {
        !matches!(self.resolved_right_active(cx), RightSurface::Files)
            && self
                .right_surface_rows(cx)
                .iter()
                .any(|(s, _, _, _)| matches!(s, RightSurface::Files))
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git gate: the pane hosts terminals too (see `right_pane_open`).
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if !open {
            // Closing always leaves takeover mode — reopening at full bleed
            // with the conversation gone read as a broken chat.
            self.right_pane_expanded = false;
        }
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        if open
            && let RightSurface::Diff(id) = self.resolved_right_active(cx)
            && let Some(changes) = self.diffs.get(&id).cloned()
        {
            // Reopening onto a diff tab revalidates its watch.
            changes.update(cx, |changes, cx| changes.ensure_content(cx));
        }
        cx.notify();
    }

    fn right_terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.right_terminal {
            return terminal.clone();
        }
        let prefs = self.terminal_prefs();
        let terminal = cx.new(|cx| TerminalPanel::new_embedded(self.state.clone(), cx));
        terminal.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        let sub = cx.subscribe(&terminal, |this, panel, event: &TerminalPanelEvent, cx| {
            this.on_terminal_panel_event(panel, event, cx);
        });
        self.right_terminal_event_sub = Some(sub);
        self.right_terminal = Some(terminal.clone());
        terminal
    }

    /// The right pane's surface tabs in the STORED (drag-reorderable) order —
    /// `(surface, title, dirty, tooltip_detail)`; entries whose backing tab/entity
    /// is gone are skipped.
    fn right_surface_rows(
        &self,
        cx: &App,
    ) -> Vec<(RightSurface, SharedString, bool, Option<SharedString>)> {
        let key = self.panel_key(cx);
        let stored: &[RightSurface] = self
            .right_tabs
            .get(&key)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let terminals: Vec<(u64, SharedString, bool)> = self
            .right_terminal
            .as_ref()
            .map(|t| t.read(cx).tab_summaries(cx))
            .unwrap_or_default();
        stored
            .iter()
            .filter_map(|surface| match surface {
                RightSurface::Files => self.files.get(&key).map(|files| {
                    let files = files.read(cx);
                    (
                        *surface,
                        files.tab_title(),
                        files.has_unsaved_changes(),
                        None,
                    )
                }),
                RightSurface::File(id) => self.file_surfaces.get(id).map(|file| {
                    let path = self.file_surface_paths.get(id);
                    let title = path
                        .map(|p| workspace_file_title(p))
                        .unwrap_or_else(|| SharedString::from("File"));
                    (
                        *surface,
                        title,
                        file.read(cx).has_unsaved_changes(),
                        path.cloned().map(Into::into),
                    )
                }),
                RightSurface::Diff(id) => self
                    .diffs
                    .get(id)
                    // Contextual title (user request): the pane's scope
                    // label, or the pinned commit's subject.
                    .map(|changes| (*surface, changes.read(cx).tab_title(), false, None)),
                RightSurface::Terminal(tab) => terminals
                    .iter()
                    .find(|(k, _, _)| k == tab)
                    .map(|(_, title, _)| (*surface, title.clone(), false, None)),
                RightSurface::Picker => None,
                RightSurface::Subagent(id) => self
                    .subagent_tabs
                    .get(id)
                    .map(|panel| (*surface, panel.read(cx).tab_title(), false, None)),
                RightSurface::AgentShell => Some((
                    *surface,
                    SharedString::from("Shell agent"),
                    false,
                    Some(SharedString::from(
                        "Read-only view of the agent's own shell",
                    )),
                )),
            })
            .collect()
    }

    /// Drag-reorder a surface tab within this chat's strip.
    fn reorder_right_tabs(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if let Some(tabs) = self.right_tabs.get_mut(&key)
            && from < tabs.len()
            && to < tabs.len()
            && from != to
        {
            let surface = tabs.remove(from);
            tabs.insert(to, surface);
            cx.notify();
        }
    }

    /// Track the hovered drop slot mid-drag (the terminal drawer's
    /// `update_drag_over`, ported: epoch bumps restart the slide tween).
    fn update_right_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.right_tab_drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.right_tab_drag = Some(RightTabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// The surface that actually renders: the stored pick when it still
    /// exists, else the first remaining tab, else the picker. Terminal keys
    /// go stale when their tab closes/exits — never render a dead surface.
    fn resolved_right_active(&self, cx: &App) -> RightSurface {
        let picked = self.panels.get(&self.panel_key(cx)).right_active;
        let rows = self.right_surface_rows(cx);
        let exists = match picked {
            RightSurface::Picker => false,
            surface => rows.iter().any(|(s, _, _, _)| *s == surface),
        };
        if exists {
            picked
        } else {
            rows.first()
                .map(|(s, _, _, _)| *s)
                .unwrap_or(RightSurface::Picker)
        }
    }

    fn set_right_active(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        self.panels.update(&key, |p| p.right_active = surface);
        match surface {
            RightSurface::Files => {
                if let Some(files) = self.files.get(&key).cloned() {
                    files.update(cx, |files, cx| files.ensure_loaded(cx));
                }
            }
            RightSurface::File(id) => {
                if let Some(file) = self.file_surfaces.get(&id).cloned() {
                    file.update(cx, |file, cx| file.ensure_loaded(cx));
                }
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
            }
            RightSurface::Diff(id) => {
                if let Some(changes) = self.diffs.get(&id).cloned() {
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                }
            }
            RightSurface::Subagent(id) => {
                if let Some(panel) = self.subagent_tabs.get(&id).cloned() {
                    panel.update(cx, |panel, cx| panel.ensure_started(cx));
                }
            }
            RightSurface::AgentShell => {
                // Nothing to load or start: the surface reads the transcript
                // at paint time and owns no subscription by design.
            }
            RightSurface::Picker => {}
        }
        cx.notify();
    }

    fn focus_right_file_editor(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        let files = match surface {
            RightSurface::Files => self.files.get(&key).cloned(),
            RightSurface::File(id) => self.file_surfaces.get(&id).cloned(),
            _ => None,
        };
        if let Some(files) = files {
            files.update(cx, |files, cx| {
                files.focus_editor(window, cx);
            });
        }
    }

    /// Files is single-instance per chat: both the picker and the `+` menu
    /// focus the existing surface instead of creating duplicate trees and
    /// duplicate workspace subscriptions.
    fn add_files_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        if !self.files.contains_key(&key) {
            let delay = self.settings.files_autosave_delay_ms;
            let editor_font_size = self.settings.files_editor_font_size;
            let word_wrap = self.settings.files_word_wrap;
            let show_all_files = self.settings.files_show_all;
            let files = cx.new(|cx| {
                FilesSurface::new(
                    self.state.clone(),
                    key.clone(),
                    delay,
                    editor_font_size,
                    word_wrap,
                    show_all_files,
                    cx,
                )
            });
            let event_key = key.clone();
            let sub = cx.subscribe_in(
                &files,
                window,
                move |this: &mut Self, _, event, window, cx| match event {
                    FilesEvent::OpenFile(path) => {
                        this.add_file_surface(path.clone(), None, window, cx)
                    }
                    FilesEvent::TitleChanged => cx.notify(),
                    FilesEvent::FileRenamed { .. } => cx.notify(),
                    FilesEvent::WordWrapChanged(word_wrap) => {
                        this.set_files_word_wrap(*word_wrap, window, cx)
                    }
                    FilesEvent::ShowAllFilesChanged(show_all_files) => {
                        this.set_files_show_all(*show_all_files, cx)
                    }
                    FilesEvent::CloseReady => {
                        this.on_file_close_ready(RightSurface::Files, &event_key, cx)
                    }
                    FilesEvent::CloseCancelled => this.cancel_file_close(RightSurface::Files, cx),
                },
            );
            self.files.insert(key.clone(), files);
            self.files_subs.insert(key.clone(), sub);
        }
        let tabs = self.right_tabs.entry(key).or_default();
        push_unique_right_surface(tabs, RightSurface::Files);
        self.set_right_active(RightSurface::Files, cx);
        self.focus_right_file_editor(RightSurface::Files, window, cx);
    }

    /// Open a workspace file as a first-class right-pane tab. Every editor is
    /// a separate FilesSurface so its tree, search, watcher and split layout
    /// stay stable while users move among open files.
    fn add_file_surface(
        &mut self,
        path: String,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_key = self.panel_key(cx);
        let lookup = (panel_key.clone(), path.clone());
        if let Some(id) = self.file_surface_keys.get(&lookup).copied() {
            let surface = RightSurface::File(id);
            self.set_right_active(surface, cx);
            if let Some(line) = line
                && let Some(file) = self.file_surfaces.get(&id)
            {
                file.update(cx, |file, cx| file.open_at_line(line, cx));
            }
            self.focus_right_file_editor(surface, window, cx);
            return;
        }

        self.file_surface_seq += 1;
        let id = self.file_surface_seq;
        let file = cx.new(|cx| {
            FilesSurface::new_editor(
                self.state.clone(),
                panel_key.clone(),
                path.clone(),
                self.settings.files_autosave_delay_ms,
                self.settings.files_editor_font_size,
                self.settings.files_word_wrap,
                self.settings.files_show_all,
                cx,
            )
        });
        let event_panel_key = panel_key.clone();
        let sub = cx.subscribe_in(
            &file,
            window,
            move |this: &mut Self, _, event, window, cx| match event {
                FilesEvent::OpenFile(path) => this.add_file_surface(path.clone(), None, window, cx),
                FilesEvent::TitleChanged => cx.notify(),
                FilesEvent::FileRenamed { old_path, new_path } => {
                    this.rename_file_surface(id, &event_panel_key, old_path, new_path, cx)
                }
                FilesEvent::WordWrapChanged(word_wrap) => {
                    this.set_files_word_wrap(*word_wrap, window, cx)
                }
                FilesEvent::ShowAllFilesChanged(show_all_files) => {
                    this.set_files_show_all(*show_all_files, cx)
                }
                FilesEvent::CloseReady => {
                    this.on_file_close_ready(RightSurface::File(id), &event_panel_key, cx)
                }
                FilesEvent::CloseCancelled => this.cancel_file_close(RightSurface::File(id), cx),
            },
        );
        if let Some(line) = line {
            file.update(cx, |file, cx| file.open_at_line(line, cx));
        }
        self.file_surfaces.insert(id, file);
        self.file_surface_paths.insert(id, path);
        self.file_surface_keys.insert(lookup, id);
        self.file_surface_subs.insert(id, sub);
        self.right_tabs
            .entry(panel_key)
            .or_default()
            .push(RightSurface::File(id));
        self.set_right_active(RightSurface::File(id), cx);
    }

    fn rename_file_surface(
        &mut self,
        id: u64,
        panel_key: &str,
        old_path: &str,
        new_path: &str,
        cx: &mut Context<Self>,
    ) {
        if self.file_surface_paths.get(&id).map(String::as_str) != Some(old_path) {
            return;
        }
        self.file_surface_paths.insert(id, new_path.to_string());
        self.file_surface_keys
            .remove(&(panel_key.to_string(), old_path.to_string()));
        self.file_surface_keys
            .entry((panel_key.to_string(), new_path.to_string()))
            .or_insert(id);
        cx.notify();
    }

    fn workspace_path_for_surface(
        &self,
        surface: RightSurface,
        cx: &App,
    ) -> Option<WorkspacePathDrag> {
        let path = match surface {
            RightSurface::Files => self
                .files
                .get(&self.panel_key(cx))
                .and_then(|files| files.read(cx).attachment_path().map(str::to_string))?,
            RightSurface::File(id) => self.file_surface_paths.get(&id).cloned()?,
            RightSurface::Diff(_)
            | RightSurface::Picker
            | RightSurface::Terminal(_)
            | RightSurface::Subagent(_)
            | RightSurface::AgentShell => {
                return None;
            }
        };
        Some(WorkspacePathDrag::new(path, false))
    }

    fn set_files_word_wrap(
        &mut self,
        word_wrap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.files_word_wrap = word_wrap;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_word_wrap(word_wrap, cx));
        }
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_word_wrap(word_wrap, window, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn set_files_editor_font_size(&mut self, editor_font_size: f32, cx: &mut Context<Self>) {
        self.settings.files_editor_font_size = editor_font_size;
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_editor_font_size(editor_font_size, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn set_files_show_all(&mut self, show_all_files: bool, cx: &mut Context<Self>) {
        self.settings.files_show_all = show_all_files;
        if let Some(page) = self.files_settings_page.clone() {
            page.update(cx, |page, cx| page.set_show_all_files(show_all_files, cx));
        }
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        for surface in surfaces {
            surface.update(cx, |surface, cx| {
                surface.set_show_all_files(show_all_files, cx)
            });
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_file_close_ready(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pending_file_closes.contains(&surface) {
            self.complete_file_close(surface, panel_key, cx);
        } else {
            cx.notify();
        }
    }

    fn cancel_file_close(&mut self, surface: RightSurface, cx: &mut Context<Self>) {
        self.pending_file_closes.remove(&surface);
        self.window_close_pending = false;
        cx.notify();
    }

    fn complete_file_close(
        &mut self,
        surface: RightSurface,
        panel_key: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(tabs) = self.right_tabs.get_mut(panel_key) {
            tabs.retain(|candidate| *candidate != surface);
        }
        match surface {
            RightSurface::Files => {
                self.files.remove(panel_key);
                self.files_subs.remove(panel_key);
            }
            RightSurface::File(id) => {
                self.file_surfaces.remove(&id);
                self.file_surface_paths.remove(&id);
                self.file_surface_subs.remove(&id);
                self.file_surface_keys.retain(|_, value| *value != id);
            }
            _ => return,
        }
        self.pending_file_closes.remove(&surface);
        self.panels.update(panel_key, |panel| {
            if panel.right_active == surface {
                panel.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    /// A surface tab's ✕. For Files/File surfaces, initiates a graceful
    /// close that waits for pending autosaves to flush.
    fn close_right_surface(
        &mut self,
        surface: RightSurface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.panel_key(cx);
        let files = match surface {
            RightSurface::Files => self.files.get(&key).cloned(),
            RightSurface::File(id) => self.file_surfaces.get(&id).cloned(),
            _ => None,
        };
        if let Some(files) = files {
            match files.update(cx, |files, cx| files.prepare_close(cx)) {
                FilesCloseDisposition::Allow => {
                    self.complete_file_close(surface, &key, cx);
                }
                FilesCloseDisposition::Pending | FilesCloseDisposition::Blocked => {
                    self.pending_file_closes.insert(surface);
                    self.set_right_active(surface, cx);
                }
            }
            return;
        }
        if let Some(tabs) = self.right_tabs.get_mut(&key) {
            tabs.retain(|s| *s != surface);
        }
        match surface {
            RightSurface::Files | RightSurface::File(_) => {}
            RightSurface::Diff(id) => {
                // Dropping the entity tears down its diff watch.
                self.diffs.remove(&id);
                self.diff_subs.remove(&id);
            }
            RightSurface::Terminal(tab) => {
                let panel = self.right_terminal_panel(cx);
                panel.update(cx, |panel, cx| panel.close_tab_by_key(tab, window, cx));
            }
            RightSurface::Subagent(id) => {
                // Dropping the entity ends its subscription tasks: a tab that
                // is closed is a feed nobody is watching.
                self.subagent_tabs.remove(&id);
                self.subagent_subs.remove(&id);
            }
            RightSurface::AgentShell => {
                // Nothing to tear down: the read-only surface owns no entity,
                // no subscription, and no PTY.
            }
            RightSurface::Picker => {}
        }
        self.panels.update(&key, |p| {
            if p.right_active == surface {
                p.right_active = RightSurface::Picker;
            }
        });
        cx.notify();
    }

    pub fn prepare_window_close(&mut self, cx: &mut Context<Self>) -> bool {
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .cloned()
            .collect::<Vec<_>>();
        if surfaces
            .iter()
            .all(|surface| !surface.read(cx).has_unsaved_changes())
        {
            self.window_close_pending = false;
            return true;
        }
        self.window_close_pending = true;
        let mut all_ready = true;
        for surface in surfaces {
            let disposition = surface.update(cx, |surface, cx| surface.prepare_close(cx));
            all_ready &= disposition == FilesCloseDisposition::Allow;
        }
        if all_ready {
            self.window_close_pending = false;
        }
        all_ready
    }

    fn all_file_edits_flushed(&self, cx: &App) -> bool {
        self.files
            .values()
            .chain(self.file_surfaces.values())
            .all(|surface| !surface.read(cx).has_unsaved_changes())
    }
    fn add_diff_surface(&mut self, cx: &mut Context<Self>) {
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.register_diff_surface(changes, cx);
    }

    /// A History row click: the commit opens as its own pinned diff tab
    /// (user request).
    fn add_commit_diff_surface(
        &mut self,
        commit: komet_proto::GitHistoryCommit,
        cx: &mut Context<Self>,
    ) {
        let changes = cx.new(|cx| Changes::for_commit(self.state.clone(), commit, cx));
        self.register_diff_surface(changes, cx);
    }

    fn register_diff_surface(&mut self, changes: Entity<Changes>, cx: &mut Context<Self>) {
        self.diff_seq += 1;
        let id = self.diff_seq;
        let sub = cx.subscribe(&changes, |this: &mut Self, _, event, cx| match event {
            ChangesEvent::OpenCommit(commit) => {
                this.add_commit_diff_surface(commit.clone(), cx);
            }
        });
        self.diffs.insert(id, changes);
        self.diff_subs.insert(id, sub);
        let key = self.panel_key(cx);
        self.right_tabs
            .entry(key)
            .or_default()
            .push(RightSurface::Diff(id));
        self.set_right_active(RightSurface::Diff(id), cx);
    }

    /// A spawn chip's badge click: the child's transcript opens as its own
    /// read-only tab, over the host's run journal (`WatchSubagent`).
    fn add_subagent_surface(&mut self, open: SubagentOpen, cx: &mut Context<Self>) {
        // One tab per feed: clicking the same chip again — or a chip whose
        // child was already opened — focuses the tab it opened rather than
        // starting a second subscription to the same journal feed.
        let id = match self
            .subagent_tabs
            .iter()
            .find(|(_, panel)| {
                // Feed identity only: status/counters/title change while the
                // tab stays open, and must not spawn a second subscription.
                let existing = panel.read(cx).open();
                existing.chat_id == open.chat_id
                    && existing.target_device_id == open.target_device_id
                    && existing.spawn.parent_tool_use_id == open.spawn.parent_tool_use_id
            })
            .map(|(id, _)| *id)
        {
            Some(id) => id,
            None => {
                self.subagent_seq += 1;
                let id = self.subagent_seq;
                let panel = cx.new(|_| SubagentPanel::new(self.state.clone(), open));
                let sub = cx.subscribe(&panel, |this: &mut Self, _, event, cx| match event {
                    SubagentEvent::OpenNested(nested) => {
                        this.add_subagent_surface((**nested).clone(), cx);
                    }
                });
                self.subagent_tabs.insert(id, panel);
                self.subagent_subs.insert(id, sub);
                id
            }
        };
        let surface = RightSurface::Subagent(id);
        let key = self.panel_key(cx);
        push_unique_right_surface(self.right_tabs.entry(key.clone()).or_default(), surface);
        // A chip is clicked from the conversation, where the pane may well be
        // closed — opening a tab the user cannot see would read as the click
        // doing nothing.
        if !self.right_pane_open(cx) {
            let from = self.right_target(cx);
            self.panels.update(&key, |p| p.changes_open = true);
            self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        }
        self.set_right_active(surface, cx);
    }

    /// The picker's « Shell agent » card: the read-only log of the commands
    /// the agent ran in its OWN shell (Claude/Codex keep that traffic out of
    /// any Komet PTY). One tab per chat — it projects the transcript's tool
    /// events, so a second tab would show the identical list.
    fn add_agent_shell_surface(&mut self, cx: &mut Context<Self>) {
        let key = self.panel_key(cx);
        push_unique_right_surface(
            self.right_tabs.entry(key.clone()).or_default(),
            RightSurface::AgentShell,
        );
        // Same rule as a spawn chip's tab: opening a surface the user cannot
        // see reads as the click doing nothing.
        if !self.right_pane_open(cx) {
            let from = self.right_target(cx);
            self.panels.update(&key, |p| p.changes_open = true);
            self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        }
        self.set_right_active(RightSurface::AgentShell, cx);
    }

    /// The picker's Terminal card / the `+` menu's Terminal row: every click
    /// opens a fresh embedded terminal tab.
    fn add_terminal_surface(&mut self, cx: &mut Context<Self>) {
        let panel = self.right_terminal_panel(cx);
        let opened = panel.update(cx, |panel, cx| {
            panel.set_open(true, cx);
            panel.open_tab_for_selected(cx)
        });
        if let Some(tab) = opened {
            let key = self.panel_key(cx);
            self.right_tabs
                .entry(key)
                .or_default()
                .push(RightSurface::Terminal(tab));
            self.set_right_active(RightSurface::Terminal(tab), cx);
        }
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let prefs = self.terminal_prefs();
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        terminal.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        let sub = cx.subscribe(&terminal, |this, panel, event: &TerminalPanelEvent, cx| {
            this.on_terminal_panel_event(panel, event, cx);
        });
        self.terminal_event_sub = Some(sub);
        self.terminal = Some(terminal.clone());
        terminal
    }

    /// The persisted terminal chrome settings, as the panels consume them.
    fn terminal_prefs(&self) -> TerminalPrefs {
        TerminalPrefs {
            font_size: self.settings.terminal_font_size,
            scrollback_lines: self.settings.terminal_scrollback_lines,
        }
    }

    /// Mirror prefs onto the panel that did NOT report the change (updating
    /// the emitter back into itself while it is mid-update would re-borrow
    /// the entity).
    fn push_terminal_prefs(&self, emitter: &Entity<TerminalPanel>, cx: &mut Context<Self>) {
        let prefs = self.terminal_prefs();
        for panel in [&self.terminal, &self.right_terminal].into_iter().flatten() {
            if panel.entity_id() == emitter.entity_id() {
                continue;
            }
            let panel = panel.clone();
            panel.update(cx, |panel, cx| panel.set_prefs(prefs, cx));
        }
    }

    fn on_terminal_panel_event(
        &mut self,
        panel: Entity<TerminalPanel>,
        event: &TerminalPanelEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalPanelEvent::FontSizeChanged(size) => {
                self.settings.terminal_font_size = *size;
                self.schedule_save(cx);
                self.push_terminal_prefs(&panel, cx);
            }
            TerminalPanelEvent::TabActivated(key) => {
                // Embedded cycling: the surface strip highlights from shell
                // state, so follow the panel's new active tab — but only if
                // that tab has a surface (drawer tabs have none).
                let key_panel = self.panel_key(cx);
                let has = self
                    .right_tabs
                    .get(&key_panel)
                    .is_some_and(|tabs| tabs.contains(&RightSurface::Terminal(*key)));
                if has {
                    self.panels.update(&key_panel, |p| {
                        p.right_active = RightSurface::Terminal(*key)
                    });
                    cx.notify();
                }
            }
            TerminalPanelEvent::TabClosed(key) => {
                // Drop the surface for a closed embedded tab (the ✕ path also
                // does this, but keyboard/menu closes land here; retaining an
                // absent surface is a no-op). State-only: never update the
                // emitting panel here, it is mid-update.
                let key_panel = self.panel_key(cx);
                let surface = RightSurface::Terminal(*key);
                let had = if let Some(tabs) = self.right_tabs.get_mut(&key_panel) {
                    let before = tabs.len();
                    tabs.retain(|s| *s != surface);
                    tabs.len() != before
                } else {
                    false
                };
                if had {
                    let next = self
                        .right_tabs
                        .get(&key_panel)
                        .and_then(|tabs| {
                            tabs.iter().find_map(|s| match s {
                                RightSurface::Terminal(k) => Some(RightSurface::Terminal(*k)),
                                _ => None,
                            })
                        })
                        .unwrap_or(RightSurface::Picker);
                    self.panels.update(&key_panel, |p| {
                        if p.right_active == surface {
                            p.right_active = next;
                        }
                    });
                    cx.notify();
                }
            }
        }
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (komet `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (komet terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in komet's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
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

    /// Record the current navigation for the active device (session or new-task).
    fn remember_current_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(device_id) = self.state.read(cx).local_device_id.clone() else {
            return;
        };
        let nav = if self.active_chat.is_empty() {
            let state = self.state.read(cx);
            let project_id = if state.no_project {
                None
            } else {
                state.selected_space.clone()
            };
            RememberedNavigation::NewTask { project_id }
        } else {
            RememberedNavigation::Session {
                id: self.active_chat.clone(),
            }
        };
        if self.settings.last_session_by_device.get(&device_id) != Some(&nav) {
            self.settings.last_session_by_device.insert(device_id, nav);
            self.schedule_save(cx);
        }
    }

    /// Prune stale remembered entries (archived/deleted sessions, deleted projects)
    /// — same rule as `open_tabs` pruning against the doc.
    fn prune_remembered_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        let mut to_remove = Vec::new();
        for (device_id, nav) in &self.settings.last_session_by_device {
            let keep = match nav {
                RememberedNavigation::Session { id } => {
                    let state = self.state.read(cx);
                    let chat = state.chats.iter().find(|c| &c.id == id);
                    match chat {
                        Some(c) if !c.archived => match &c.space_id {
                            Some(sid) => state.space_row(sid).is_some(),
                            None => true,
                        },
                        _ => false,
                    }
                }
                RememberedNavigation::NewTask { project_id } => match project_id {
                    None => true,
                    Some(pid) => self.state.read(cx).space_row(pid).is_some(),
                },
            };
            if !keep {
                to_remove.push(device_id.clone());
            }
        }
        let pruned = !to_remove.is_empty();
        for id in to_remove {
            self.settings.last_session_by_device.remove(&id);
        }
        // Drop folder expansion state for deleted spaces (same rule as the
        // remembered destinations above).
        let before = self.settings.expanded_spaces.len();
        let state = self.state.read(cx);
        self.settings
            .expanded_spaces
            .retain(|id| state.space_row(id).is_some());
        pruned || self.settings.expanded_spaces.len() != before
    }

    pub(super) fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- sidebar mutations ----

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    pub(super) fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    pub(super) fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.close_chat_menu(cx);
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.as_ref() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if title.is_empty() {
            // Empty titles aren't valid — keep the dialog open so a cleared
            // field doesn't silently close with no rename and no feedback.
            cx.notify();
            return;
        }
        let chat_id = dialog.chat_id.clone();
        self.rename_dialog = None;
        self.mutate(
            serde_json::json!({ "op": "renameChat", "chatId": chat_id, "title": title }),
            cx,
        );
        cx.notify();
    }

    pub(super) fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.set_chat_archived(chat_id, true, cx);
    }

    pub(super) fn set_chat_archived(
        &mut self,
        chat_id: String,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        self.close_chat_menu(cx);
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": archived }),
            cx,
        );
        cx.notify();
    }

    pub(super) fn toggle_chat_pinned(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let pinned = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .is_some_and(|c| c.pinned);
        self.close_chat_menu(cx);
        self.state.update(cx, |state, _cx| {
            if let Some(chat) = state.chats.iter_mut().find(|c| c.id == chat_id) {
                chat.pinned = !pinned;
            }
            crate::state::sort_chats(&mut state.chats);
        });
        self.mutate(
            serde_json::json!({ "op": "setChatPinned", "chatId": chat_id, "pinned": !pinned }),
            cx,
        );
        cx.notify();
    }

    pub(super) fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, _| composer.purge_chat(&chat_id));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    pub(super) fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let scope = self.state.read(cx).workspace_scope;
        if scope == Some(WorkspaceScope::Development) {
            return;
        }
        self.close_user_menu(cx);
        if scope == Some(WorkspaceScope::Local) {
            self.sync_flow = SyncFlow::Enabling;
        }
        let Some(engine) = self.state.read(cx).local_engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                    cx.notify();
                }
                Err(err) => {
                    if scope == Some(WorkspaceScope::Local) && shell.sync_flow == SyncFlow::Enabling
                    {
                        shell.sync_flow = SyncFlow::Idle;
                    }
                    shell.sidebar_notice = Some(format!("Sign in failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
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

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);

        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar). Settings never
        // underlaps: pad below the overlaid titlebar.
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }

        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let home_canvas = !has_selection;
        self.composer.update(cx, |composer, cx| {
            composer.set_home_canvas(home_canvas, cx);
        });
        let no_project = self.state.read(cx).no_project;
        // While the engine is still connecting the space list is simply empty —
        // show the new-session canvas, not "Add a project", which would read as
        // a wrong state rather than a pending one.
        let booting = matches!(self.state.read(cx).gate(), GatePhase::Loading);

        // Content outlet: selected chat → transcript; nothing selected → the
        // centered new-session canvas; no spaces at all → the onboarding card.
        //
        // The transcript is a cached gpui view: it re-renders only when its
        // entity — or one of its embedded views — is notified (gpui dirties
        // the ancestors of a notified view). It observes AppState, its list
        // notifies on scroll, the in-place editor notifies on caret blink, and
        // `motion::hover_frame` notifies the fade owners. A shell repaint —
        // hover, spinner, sync overlay — no longer rebuilds it.
        let outlet: AnyElement = if has_selection {
            self.transcript
                .clone()
                .cached(StyleRefinement::default().size_full())
                .into_any_element()
        } else if !has_spaces && !no_project && !booting {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .relative()
                        .w_full()
                        .max_w(px(1187.0))
                        .h(px(451.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(crate::blobatar::blobatar(
                            "onboarding-blobatar",
                            64.0,
                            Some(theme.text.opacity(0.85)),
                        ))
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Add a project to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(
                                    "A project is a folder on one of your devices.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a project")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // Keep destination and checkout selectors in one row; agent/model
            // selection stays in the composer's action bar.
            let pickers = self.composer.read(cx).pickers().clone();
            let (target_selectors, checkout_selectors) = pickers.update(cx, |p, cx| {
                (p.render_target_selectors(cx), p.render_footer(window, cx))
            });
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .w_full()
                        .max_w(px(768.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .mb(px(28.0))
                                .text_size(px(20.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child("What are we building?"),
                        )
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .w_full()
                                        .px(px(Theme::SPACE_LG))
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(Theme::SPACE_MD))
                                        .child(div().flex_none().child(target_selectors))
                                        .when_some(checkout_selectors, |el, controls| {
                                            el.child(div().flex_1().min_w_0().child(controls))
                                        }),
                                )
                                .child(div().h(px(12.0)).flex_none())
                                .child(self.composer.clone()),
                        ),
                ))
                .into_any_element()
        };
        let status = self.render_status_strip(window, cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop files or images to attach" veil; a drop
        // stages them in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        let provider_switch_notice = self
            .composer
            .read(cx)
            .pickers()
            .read(cx)
            .provider_switch_notice()
            .map(|notice| {
                let theme = Theme::of(cx).clone();
                let (from_icon, from_tint) = crate::pickers::harness_brand_icon(notice.from);
                let (to_icon, to_tint) = crate::pickers::harness_brand_icon(notice.to);
                let transition = div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(10.0))
                    .child(
                        icon(from_icon)
                            .size(px(18.0))
                            .text_color(from_tint.unwrap_or(theme.text_muted)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_muted)
                            .child(notice.from.display_name()),
                    )
                    .child(
                        icon(crate::icons::ARROW_RIGHT)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.75)),
                    )
                    .child(
                        icon(to_icon)
                            .size(px(18.0))
                            .text_color(to_tint.unwrap_or(theme.text)),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(notice.to.display_name()),
                    );
                let card = div()
                    .px(px(16.0))
                    .py(px(11.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(crate::theme::hairline(0.12))
                    .bg(theme.surface)
                    .shadow(crate::theme::card_selected_shadows())
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(5.0))
                    .child(transition)
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme.text_muted.opacity(0.8))
                            .child(format!(
                                "{} is selected for this session",
                                notice.to.display_name()
                            )),
                    );
                div()
                    .absolute()
                    .top(px(24.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(motion::fade_in(
                        ("provider-switch-notice", notice.sequence),
                        card,
                    ))
                    .into_any_element()
            });
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, false, cx));
                cx.notify();
            }))
            .on_drag_move::<WorkspacePathDrag>(cx.listener(
                |this, e: &gpui::DragMoveEvent<WorkspacePathDrag>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop::<WorkspacePathDrag>(cx.listener(
                |this, payload: &WorkspacePathDrag, window, cx| {
                    this.file_drag_active = false;
                    this.composer.update(cx, |composer, cx| {
                        composer.add_workspace_path(&payload.path, payload.is_directory, window, cx)
                    });
                    cx.notify();
                },
            ))
            .children(provider_switch_notice)
            .child(
                // Full-height underlay: the transcript viewport spans the
                // whole column, scrolling UNDER the titlebar above and the
                // composer stack below. The per-glyph EdgeFade (glass-safe,
                // same as the sidebar's) spans the full column with
                // ASYMMETRIC bands sized to the chrome: content is opaque at
                // the chrome's inner edge and fades to zero at the window
                // edge — visible mid-fade through the glass chrome it slides
                // under. Always on (the resting paddings keep pinned content
                // out of the bands, and gating on measured scroll state left
                // the top unfaded for one frame on session switch — user
                // report). The jump pill floats outside the fade scope,
                // anchored above the measured stack.
                {
                    // The terminal dock is NOT glass the transcript may slide
                    // under: with the dock's translucent fill, transcript text
                    // ghosted through the grid (user report). The underlay
                    // ends at the dock's top instead, riding the same height
                    // tween the dock animates with; `stack_h` below is only
                    // the chrome that still overlaps the transcript (status
                    // strip + composer).
                    let term_h = self.eval_tween(self.terminal_tween, self.terminal_target(cx));
                    let stack_h = (self.bottom_stack.get() - term_h).max(0.0);
                    // Opaque from the composer PILL's top (the reserved
                    // status strip above it is empty air), zero at the
                    // underlay's bottom edge.
                    let bottom_band = (stack_h - Theme::STATUS_STRIP_HEIGHT).max(1.0);
                    div()
                        .absolute()
                        .inset_0()
                        .bottom(px(term_h))
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                true,
                                true,
                                div().size_full().child(outlet),
                            )
                            .inset_top(0.0)
                            .band_top(Theme::TRANSCRIPT_FADE_BAND)
                            .band_bottom(bottom_band),
                        )
                        .children(self.render_jump_to_bottom(stack_h, cx))
                },
            )
            // The glass chrome stack, floating over the transcript's bottom:
            // reserved status strip (h-6, the WorkingIndicator — the composer
            // below never shifts), composer, terminal dock. A paint-time
            // canvas measures the stack for next frame's fade inset and
            // transcript clearance. The flex_1 spacer has no id/listeners, so
            // pointer + wheel events over it fall through to the list below.
            .child(div().flex_1().min_h_0())
            .child({
                let measured = self.bottom_stack.clone();
                div()
                    .flex_none()
                    .relative()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::canvas(
                            move |bounds, _, _| measured.set(f32::from(bounds.size.height)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .when(has_selection, |el| el.child(status))
                    .when(has_spaces && has_selection, |el| {
                        el.when_some(self.render_plan_card(cx), |el, card| el.child(card))
                            .child(self.composer.clone())
                    })
                    .child(self.render_terminal_container(cx))
            })
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(theme.scrim().opacity(0.4 / 0.6))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .child("Drop files or images to attach"),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    /// `stack_h` is the measured bottom chrome stack the full-height
    /// transcript scrolls under — the pill anchors just above it (the -14
    /// carries the old status-strip overlap).
    fn render_jump_to_bottom(
        &mut self,
        stack_h: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .bottom(px(stack_h - 14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(motion::dialog_in(
                    "jump-to-bottom",
                    div()
                        .id("jump-to-bottom-btn")
                        .h(px(30.0))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(11.0))
                        .pr(px(13.0))
                        .cursor_pointer()
                        // Hover must BRIGHTEN the opaque pill, never replace it
                        // with a translucent wash (a 10%-alpha bg here made the
                        // pill go see-through on hover — user-reported), and it
                        // fades over the CSS transition-colors 150ms, not snaps.
                        .bg(motion::hover_blend(
                            "jump-pill",
                            theme.surface_raised,
                            theme.surface_raised_hover,
                        ))
                        .on_hover(motion::hover_listener("jump-pill", cx.entity_id()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcript
                                .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        }))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("↓")),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(theme.text)
                                .child(SharedString::from("Scroll to bottom")),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes). The handle
        // FLOATS over the panel's top edge (painted after, so it wins hit
        // testing) instead of stacking above it — stacked, its 5px read as
        // dead air between the seam and the tab bar (user report).
        let inner = div()
            .h(px(height))
            .w_full()
            .relative()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h_0().child(panel))
            .child(handle.absolute().top_0().left_0().right_0());

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The selected chat's current plan, above the composer. Visible while
    /// work remains or the run is still live: once every task is done and the
    /// run has settled, the transcript chips keep the record and the card
    /// leaves the chrome. Width matches the composer column so the two read as
    /// one stack; nothing here takes focus or blocks typing.
    fn render_plan_card(&self, cx: &App) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let chat_id = state.selected_chat.as_deref()?;
        let plan = state.active_plan.as_deref()?;
        let total = plan.len();
        let done = plan.iter().filter(|item| item.done).count();
        let run_live = matches!(
            state.indicator_for(chat_id, Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        );
        let (rows, hidden) = plan_card_layout(total, done, run_live)?;
        let theme = Theme::of(cx).clone();
        Some(
            div()
                .w_full()
                .max_w(px(768.0))
                .mx_auto()
                .px(px(Theme::SPACE_LG))
                .pb(px(Theme::SPACE_SM))
                .child(
                    div()
                        .id("active-plan")
                        .w_full()
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_raised)
                        .px(px(Theme::SPACE_MD))
                        .py(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            icon(icons::CHECKLIST)
                                                .size(px(13.0))
                                                .text_color(theme.text_muted),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_color(theme.text)
                                                .child("Plan"),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(format!("{done}/{total}"))),
                                ),
                        )
                        .children(plan.iter().take(rows).map(|item| {
                            div()
                                .flex()
                                .items_start()
                                .gap(px(6.0))
                                .text_size(px(12.0))
                                .child(
                                    div()
                                        .w(px(14.0))
                                        .flex_none()
                                        .text_color(if item.done {
                                            theme.success
                                        } else {
                                            theme.text_faint
                                        })
                                        .child(if item.done { "✓" } else { "○" }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_color(if item.done {
                                            theme.text_muted.opacity(0.7)
                                        } else {
                                            theme.text
                                        })
                                        .child(SharedString::from(item.text.clone())),
                                )
                        }))
                        .when(hidden > 0, |el| {
                            el.child(
                                div()
                                    .pl(px(20.0))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(format!("+{hidden} more"))),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (komet's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(px(11.0));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        // Timer base: the freshest of the session row's turn start and the
        // in-flight send. During the send→ack window the row (if any) still
        // carries the PREVIOUS turn's start, and using it opened the timer at
        // the old turn's elapsed instead of 0:00.
        let started = state
            .session_for(&chat_id)
            .and_then(|s| s.started_at)
            .into_iter()
            .chain(state.pending_send_started(&chat_id, now))
            .max();
        let elapsed_secs = started
            .map(|t| now.signed_duration_since(t).num_seconds().max(0))
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        // Unused here since the Working loader moved into the transcript
        // (its trailer computes its own elapsed).
        let _ = elapsed_secs;
        match indicator {
            // The working loader lives in the TRANSCRIPT now, under the
            // streaming reply (user request) — the strip stays empty (its
            // reserved height still steadies the composer).
            Indicator::Working => strip.into_any_element(),
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(
                    crate::thinking_orbs::ThinkingOrb::new(
                        crate::thinking_orbs::OrbState::Listening,
                        20.0,
                    )
                    .speed(1.25)
                    .driven(cx.entity_id(), window, cx),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right pane — the surface host (t3code RightPanelTabs): hidden by
    /// default, drag-resizable. Content is the ACTIVE surface — the Diff
    /// page (its options row + the lazy [`Changes`] viewer), an embedded
    /// terminal, or the "Open a surface" picker when no tabs exist.
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.right_pane_open(cx) {
            let active = self.resolved_right_active(cx);
            let main: AnyElement = match active {
                RightSurface::Files => {
                    let key = self.panel_key(cx);
                    if let Some(files) = self.files.get(&key).cloned() {
                        files.update(cx, |files, cx| files.ensure_loaded(cx));
                        files.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::File(id) => {
                    if let Some(file) = self.file_surfaces.get(&id).cloned() {
                        file.update(cx, |file, cx| file.ensure_loaded(cx));
                        file.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::Diff(id) if self.diffs.contains_key(&id) => {
                    let changes = self.diffs.get(&id).cloned().expect("checked");
                    // Idempotent — also covers a persisted-open pane on boot.
                    changes.update(cx, |changes, cx| changes.ensure_content(cx));
                    // The diff options (scope dropdown, ref selector,
                    // fold-all) moved DOWN from the titlebar band — the
                    // surface tabs own that row now; the expand/close
                    // buttons stayed up there (user request).
                    let controls =
                        changes.update(cx, |changes, cx| changes.render_header_controls(cx));
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_none()
                                .h(px(36.0))
                                .px(px(8.0))
                                .border_b_1()
                                .border_color(theme.border)
                                .child(controls),
                        )
                        // Cached boundary: the diff surface observes AppState
                        // and its own list state, and gpui dirties this view
                        // when an embedded view (history, fetch button, ref
                        // search) is notified. Resizes invalidate via the
                        // cached bounds.
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .child(changes.cached(StyleRefinement::default().size_full())),
                        )
                        .into_any_element()
                }
                RightSurface::Terminal(tab) => {
                    let panel = self.right_terminal_panel(cx);
                    // Keep the embedded panel's own active tab aligned with
                    // the resolved surface (fallbacks can move it).
                    panel.update(cx, |panel, cx| panel.select_tab_by_key(tab, cx));
                    panel.into_any_element()
                }
                RightSurface::Subagent(id) => {
                    if let Some(panel) = self.subagent_tabs.get(&id).cloned() {
                        // Idempotent, like the diff surface: a persisted-open
                        // pane on boot starts watching here, not at tab open.
                        panel.update(cx, |panel, cx| panel.ensure_started(cx));
                        panel.into_any_element()
                    } else {
                        self.render_surface_picker(cx)
                    }
                }
                RightSurface::AgentShell => {
                    // Read at paint time: the panel is a projection of the
                    // transcript (commands from the doc's `Exec` chips, output
                    // from the ephemeral activity tails), so it holds no
                    // entity, no subscription — and no path to a PTY.
                    let log = self.transcript.read(cx).agent_shell_log();
                    let harness = self
                        .composer
                        .read(cx)
                        .pickers()
                        .read(cx)
                        .resolved(cx)
                        .harness;
                    let agent = harness.map(|id| id.display_name().to_string());
                    let surface = harness.map_or(komet_proto::ProgressSurface::None, |id| {
                        komet_harness::capabilities::interaction_surface(id).tool_progress
                    });
                    crate::agent_shell::render(
                        &log,
                        agent.as_deref().unwrap_or("The agent"),
                        surface,
                        &theme,
                    )
                }
                _ => self.render_surface_picker(cx),
            };
            // Files browser companion: shown alongside File editor tabs
            // and any other surface that isn't Files itself, matching
            // VS Code's explorer-stays-put behaviour.
            let files_panel = if active == RightSurface::Files {
                None
            } else {
                let key = self.panel_key(cx);
                self.right_tabs.get(&key).and_then(|tabs| {
                    if tabs.contains(&RightSurface::Files) {
                        self.files.get(&key).cloned()
                    } else {
                        None
                    }
                })
            };
            match files_panel {
                Some(files) => {
                    // Two independent-looking panels (own bg/border/rounded
                    // corners each, separated by a gap) rather than one
                    // merged block split by a hairline — user request: the
                    // file editor should read as its own container, not as
                    // fused into the explorer's box.
                    div()
                        .flex()
                        .flex_row()
                        .size_full()
                        .gap(px(8.0))
                        .p(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .rounded(px(8.0))
                                .overflow_hidden()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .child(main),
                        )
                        .child(
                            div()
                                .w(px(240.0))
                                .flex_none()
                                .h_full()
                                .rounded(px(8.0))
                                .overflow_hidden()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.glass())
                                .child(files),
                        )
                        .into_any_element()
                }
                None => main,
            }
        } else {
            gpui::Empty.into_any_element()
        };
        // Flush panel (user request — the inset card is gone): full window
        // height with a left hairline, glass-friendly like the terminal dock
        // (translucent over the frost; solid otherwise). The resize grabber
        // floats over the border seam.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the panel's left border.
            .left(px(0.0));
        let panel_bg = bg;
        let panel = div()
            .size_full()
            .flex()
            .flex_col()
            // In takeover the panel's left edge IS the sidebar seam, which
            // already carries the sidebar tone's right hairline — a second
            // border there doubled up (user report).
            .when(!self.right_pane_expanded, |el| {
                el.border_l_1().border_color(theme.border)
            })
            .bg(panel_bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        // Takeover mode has no drag width — the handle would fight the
        // viewport-derived target.
        let handle = (!self.right_pane_expanded).then_some(handle);
        self.pane_container(
            self.right_tween,
            target,
            div()
                .h_full()
                .relative()
                .child(panel)
                .children(handle)
                .into_any_element(),
        )
    }

    /// The right pane's empty state: the "Open a surface" heading over a
    /// compact vertical list of surface rows (icon + label) — the Capy
    /// arrangement (user request): the old two-card grid clipped in narrow
    /// panes and wasted short ones.
    fn render_surface_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let text = theme.text;
        let muted = theme.text_muted;
        let border = theme.border;
        let border_strong = theme.border_strong;
        let row = |id: &'static str, icon_path: &'static str, title: &'static str| {
            div()
                .id(id)
                .w_full()
                .h(px(44.0))
                .px(px(14.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(border)
                .bg(crate::theme::ink(0.02))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .cursor_pointer()
                .hover(move |s| s.bg(crate::theme::ink(0.05)).border_color(border_strong))
                .child(icon(icon_path).size(px(15.0)).flex_none().text_color(muted))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(text)
                        .child(SharedString::from(title)),
                )
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(280.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_center()
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text)
                            .child(SharedString::from("Open a surface")),
                    )
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_center()
                            .text_size(px(11.5))
                            .text_color(muted)
                            .child(SharedString::from(
                                "Choose what to show in the right panel.",
                            )),
                    )
                    .child(
                        div()
                            .mt(px(16.0))
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                row("surface-card-terminal", icons::TERMINAL, "Terminal").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.add_terminal_surface(cx);
                                    }),
                                ),
                            )
                            // The agent's own shell, in read-only: no Komet PTY
                            // is involved, so nothing here can be typed into.
                            .child(
                                row("surface-card-agent-shell", icons::TERMINAL, "Shell agent")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.add_agent_shell_surface(cx);
                                    })),
                            )
                            .child(
                                row("surface-card-files", icons::FOLDER_WITH_FILES, "Files")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_files_surface(window, cx);
                                    })),
                            )
                            // Git only where there IS git — the pane itself
                            // no longer gates on it (terminals work anywhere).
                            .when(self.space_git_detected(cx), |el| {
                                el.child(
                                    row("surface-card-git", icons::GIT_BRANCH, "Git").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.add_diff_surface(cx);
                                        }),
                                    ),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn close_right_plus(&mut self, cx: &mut Context<Self>) {
        if self.right_plus.begin_close() {
            popover::reap_popup(cx, |shell: &mut Self| &mut shell.right_plus);
        }
        cx.notify();
    }

    /// The titlebar strip over the right pane: one chip per surface tab
    /// (icon · title · ✕) plus the `+` menu — the t3code RightPanelTabs bar,
    /// living in the top row; the diff options moved into the pane below.
    pub(crate) fn render_right_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        /// Fixed chip slot — the terminal drawer's drag mechanics (drop-index
        /// quantisation + slide offsets) assume uniform widths.
        const CHIP_W: f32 = 112.0;
        const CHIP_SLOT: f32 = CHIP_W + 4.0; // + the strip's own gap

        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the strip.
        if self.right_tab_drag.is_some() && !cx.has_active_drag() {
            self.right_tab_drag = None;
        }
        let rows = self.right_surface_rows(cx);
        let count = rows.len();
        let active = self.resolved_right_active(cx);
        let drag = self
            .right_tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        // Fade flags from the LAST frame's scroll state (invisible lag).
        // The EdgeFade scope below fades per-pixel on x for glyphs AND
        // quads/images (fork 5d1f83d) — washes dissolve across the band.
        const FADE_WIDTH: f32 = 36.0;
        let scrolled = -f32::from(self.right_tab_scroll.offset().x);
        let max_scroll = f32::from(self.right_tab_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        // The old session-tab strip's proven scroll shape: the flex row IS
        // the scroller (id + overflow_x_scroll + track_scroll), wrapped in a
        // relative min_w_0 region below; drop math runs in CONTENT
        // coordinates (viewport-relative x plus the scrolled-off width).
        let scroll_for_drag = self.right_tab_scroll.clone();
        let mut strip = div()
            .id("right-surface-strip")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&self.right_tab_scroll)
            .on_drag_move::<RightTabDrag>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<RightTabDrag>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.panel_key != this.panel_key(cx) {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x)
                        - f32::from(event.bounds.left())
                        - f32::from(scroll_for_drag.offset().x);
                    let over = crate::terminal::panel::drop_index(rel_x, CHIP_SLOT, count);
                    this.update_right_tab_drag_over(from, over, cx);
                },
            ))
            .on_drop::<RightTabDrag>(cx.listener(move |this, payload: &RightTabDrag, _, cx| {
                if payload.panel_key != this.panel_key(cx) {
                    this.right_tab_drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .right_tab_drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from);
                this.right_tab_drag = None;
                this.reorder_right_tabs(payload.from, to, cx);
            }));
        for (ix, (surface, title, _dirty, _detail)) in rows.into_iter().enumerate() {
            let is_active = surface == active;
            let icon_path = match surface {
                RightSurface::Files => icons::FOLDER_WITH_FILES,
                RightSurface::File(_) => icons::DOCUMENT,
                RightSurface::Diff(_) => icons::GIT_BRANCH,
                RightSurface::Subagent(_) => icons::DOCUMENT,
                _ => icons::TERMINAL,
            };
            // t3 tab hover: the surface icon swaps IN PLACE for the close ✕
            // (same slot, no width jump) — the ✕ only shows while the tab is
            // hovered (user request).
            let group: SharedString = format!("right-surface-tab-{ix}").into();
            let ghost_title = title.clone();
            let workspace_path = self.workspace_path_for_surface(surface, cx);
            let chip = div()
                .id(("right-surface-tab", ix))
                .group(group.clone())
                .h(px(34.0))
                .w(px(CHIP_W))
                .flex_none()
                .pl(px(8.0))
                .pr(px(4.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(5.0))
                .cursor_pointer()
                .border_b_2()
                .border_color(if is_active {
                    theme.accent
                } else {
                    gpui::transparent_black()
                })
                // The old session-tab strip's solved carve-out: NOT
                // `.occlude()` — a BlockMouse hitbox ends the hit test,
                // so the scroll container behind the tabs never saw
                // wheel events and an overflowing strip could not be
                // scrolled (tabs tile the whole region). ExceptScroll
                // keeps the titlebar drag-region carve-out and lets the
                // strip scroll.
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                    window.prevent_default()
                })
                .when(is_active, |el| el.bg(crate::theme::wash(0.06)))
                .when(!is_active, |el| {
                    el.hover(|s| s.bg(crate::theme::wash(0.04)))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_right_active(surface, cx);
                }))
                // Middle-click closes, like every tab strip.
                .on_mouse_down(
                    gpui::MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        this.close_right_surface(surface, window, cx);
                    }),
                )
                .on_drag(
                    RightTabDrag {
                        panel_key: self.panel_key(cx),
                        from: ix,
                        title: ghost_title,
                        workspace_path,
                    },
                    |payload, _point, _, cx| {
                        let title = payload.title.clone();
                        cx.stop_propagation();
                        cx.new(|_| SurfaceTabGhost { title })
                    },
                )
                .child(icon(icon_path).size(px(13.0)).text_color(if is_active {
                    theme.text
                } else {
                    theme.text_muted.opacity(0.7)
                }))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(if is_active {
                            theme.text
                        } else {
                            theme.text_muted
                        })
                        .child(title),
                )
                .child(
                    div()
                        .id(("right-surface-close", ix))
                        .flex_none()
                        .size(px(18.0))
                        .rounded(px(3.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .opacity(0.0)
                        .group_hover(group.clone(), |s| s.opacity(1.0))
                        .hover(|s| s.bg(crate::theme::wash(0.12)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_right_surface(surface, window, cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(10.0))
                                .text_color(theme.text_muted),
                        ),
                );
            // Sliding transform while a sibling drags over (the terminal
            // drawer's exact recipe): animate 150ms between committed
            // offsets; the dragged tab leaves an invisible spacer — the
            // ghost carries it.
            let wrapped: AnyElement = match drag {
                Some((from, over, epoch, prev_over)) if ix != from => {
                    let target = crate::terminal::panel::slide_offset(ix, from, over) * CHIP_SLOT;
                    let start =
                        crate::terminal::panel::slide_offset(ix, from, prev_over) * CHIP_SLOT;
                    div()
                        .relative()
                        .child(chip.with_animation(
                            ("right-tab-slide", (ix as u64) | ((epoch as u64) << 32)),
                            TAB_SLIDE.animation(),
                            move |el, t| el.left(px(motion::lerp(start, target, t))),
                        ))
                        .into_any_element()
                }
                Some((from, ..)) if ix == from => div()
                    .w(px(CHIP_W))
                    .h(px(24.0))
                    .flex_none()
                    .into_any_element(),
                _ => chip.into_any_element(),
            };
            strip = strip.child(wrapped);
        }
        // The `+` — a small menu offering the two surfaces (t3 "Add panel
        // surface"); mirrors the picker cards.
        let plus_open = self.right_plus.get().is_some();
        let plus_fade = "right-surface-add-fade";
        let mut plus = div()
            .id("right-surface-add")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                plus_fade,
                crate::theme::wash(0.0),
                crate::theme::wash(0.11),
            ))
            .on_hover(motion::hover_listener(plus_fade, cx.entity_id()))
            .block_mouse_except_scroll()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.right_plus.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.right_plus.take_press_was_open() {
                    this.close_right_plus(cx);
                } else {
                    this.right_plus.open(());
                    cx.notify();
                }
            }))
            .child(
                icon(icons::PLUS)
                    .size(px(13.0))
                    .text_color(theme.text_muted),
            );
        if plus_open {
            let closing = self.right_plus.closing_since();
            let menu = popover::popover_card(&theme)
                .w(px(168.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_right_plus(cx)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            popover::menu_row(&theme, false, "right-plus-terminal", cx.entity_id())
                                .id("right-plus-terminal-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_terminal_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::TERMINAL)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Terminal")),
                        )
                        .child(
                            popover::menu_row(&theme, false, "right-plus-files", cx.entity_id())
                                .id("right-plus-files-row")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_files_surface(window, cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::FOLDER_WITH_FILES)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Files")),
                        )
                        .child(
                            popover::menu_row(&theme, false, "right-plus-diff", cx.entity_id())
                                .id("right-plus-diff-row")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_diff_surface(cx);
                                    this.close_right_plus(cx);
                                }))
                                .child(
                                    icon(icons::GIT_BRANCH)
                                        .size(px(13.0))
                                        .text_color(theme.text_muted),
                                )
                                // "Git", not "Git diff" — the surface hosts
                                // history and per-commit views too (user
                                // request; matches the picker card).
                                .child(SharedString::from("Git")),
                        ),
                )
                .into_any_element();
            plus = plus.relative().child(popover::anchored_menu_below_gap(
                "right-plus-menu",
                menu,
                closing,
                10.0,
            ));
        }
        strip = strip.child(plus);
        // Edge fades on whichever side hides tabs (flags computed above).
        // Glass: per-glyph EdgeFade scope over the chips' own opacity ramps;
        // opaque: painted gradients in the shell surface tone.
        let glass = theme.is_glass();
        let bar_bg = theme.surface;
        let region = div()
            .relative()
            .min_w_0()
            .size_full()
            .flex()
            .items_center()
            .child(strip)
            .when(fade_left && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });
        if glass {
            crate::edge_fade::edge_faded(FADE_WIDTH, false, false, region)
                .fade_left(fade_left)
                .fade_right(fade_right)
                .into_any_element()
        } else {
            region.into_any_element()
        }
    }

    /// Toggle the changes-panel takeover (the header's expand button, t3code
    /// parity): the panel grows to fill everything right of the sidebar,
    /// hiding the conversation column; toggling back restores the saved
    /// width. Rides the same width tween as open/close so the jump glides.
    fn toggle_right_pane_expand(&mut self, cx: &mut Context<Self>) {
        let from = self.right_target(cx);
        self.right_pane_expanded = !self.right_pane_expanded;
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        cx.notify();
    }

}

/// Row ceiling for the plan card above the composer: the card must stay a
/// glance, not a second transcript.
const PLAN_CARD_MAX_ROWS: usize = 6;

/// The plan card's two rules, decided apart from rendering. Visibility: open
/// work always shows the card, and a fully-done plan leaves once the run
/// settles (the transcript chips keep the record) — a run still going keeps the
/// card up so the last tick is never a flicker. Cap: rows past
/// [`PLAN_CARD_MAX_ROWS`] are counted so the card can announce `+N` instead of
/// silently cutting the list (transparency invariant I9).
fn plan_card_layout(total: usize, done: usize, run_live: bool) -> Option<(usize, usize)> {
    if total == 0 || (done == total && !run_live) {
        return None;
    }
    let rows = total.min(PLAN_CARD_MAX_ROWS);
    Some((rows, total - rows))
}


/// A size-6 icon button for the titlebar strip (komet window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // komet window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.glass_hover().opacity(0.0),
            theme.glass_hover(),
        ))
        .on_hover(motion::hover_listener(fade_key, owner))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(17.0)).text_color(muted))
}

const WINDOWS_CAPTION_BUTTON_WIDTH: f32 = 36.0;
const WINDOWS_CAPTION_WIDTH: f32 = WINDOWS_CAPTION_BUTTON_WIDTH * 3.0;
const LINUX_CAPTION_BUTTON_WIDTH: f32 = 34.0;

fn linux_control_side_width(
    buttons: [Option<WindowButton>; 3],
    has_minimize: bool,
    has_maximize: bool,
) -> f32 {
    let count = buttons
        .iter()
        .flatten()
        .filter(|button| match button {
            WindowButton::Minimize => has_minimize,
            WindowButton::Maximize => has_maximize,
            WindowButton::Close => true,
        })
        .count();
    if count == 0 {
        0.0
    } else {
        count as f32 * LINUX_CAPTION_BUTTON_WIDTH + (count - 1) as f32 * 2.0 + 8.0
    }
}

fn effective_controls(
    buttons: [Option<WindowButton>; 3],
    controls: gpui::WindowControls,
) -> (bool, bool) {
    // If the platform/compositor advertises controls (or on X11), respect them.
    // However, on Wayland compositors without xdg-decoration / WmCapabilities (e.g. GNOME Mutter),
    // controls.minimize and controls.maximize default to false even though the desktop
    // button-layout expects them. If the button layout explicitly includes minimize/maximize,
    // allow them unless controls explicitly signaled support and excluded them.
    let has_layout_min = buttons.contains(&Some(WindowButton::Minimize));
    let has_layout_max = buttons.contains(&Some(WindowButton::Maximize));
    (
        controls.minimize || has_layout_min,
        controls.maximize || has_layout_max,
    )
}

fn linux_button_layout(cx: &App, controls: gpui::WindowControls) -> WindowButtonLayout {
    effective_linux_button_layout(
        cx.button_layout().unwrap_or(WindowButtonLayout {
            left: [None; 3],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ],
        }),
        controls,
    )
}

fn effective_linux_button_layout(
    layout: WindowButtonLayout,
    controls: gpui::WindowControls,
) -> WindowButtonLayout {
    let (left_min, left_max) = effective_controls(layout.left, controls);
    let (right_min, right_max) = effective_controls(layout.right, controls);
    if linux_control_side_width(layout.left, left_min, left_max)
        + linux_control_side_width(layout.right, right_min, right_max)
        > 0.0
    {
        layout
    } else {
        WindowButtonLayout {
            left: [None; 3],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ],
        }
    }
}

fn titlebar_right_padding(is_windows: bool, linux_controls: f32, base: f32) -> f32 {
    base + if is_windows {
        WINDOWS_CAPTION_WIDTH
    } else {
        linux_controls
    }
}

fn client_resize_edge(
    position: Point<Pixels>,
    size: Size<Pixels>,
    inset: f32,
    tiling: Tiling,
) -> Option<ResizeEdge> {
    let x = f32::from(position.x);
    let y = f32::from(position.y);
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    let left = x < inset && !tiling.left;
    let right = x >= width - inset && !tiling.right;
    let top = y < inset && !tiling.top;
    let bottom = y >= height - inset && !tiling.bottom;

    match (top, right, bottom, left) {
        (true, false, false, true) => Some(ResizeEdge::TopLeft),
        (true, true, false, false) => Some(ResizeEdge::TopRight),
        (false, true, true, false) => Some(ResizeEdge::BottomRight),
        (false, false, true, true) => Some(ResizeEdge::BottomLeft),
        (true, _, _, _) => Some(ResizeEdge::Top),
        (_, true, _, _) => Some(ResizeEdge::Right),
        (_, _, true, _) => Some(ResizeEdge::Bottom),
        (_, _, _, true) => Some(ResizeEdge::Left),
        _ => None,
    }
}

/// A Windows-owned caption target using the same system glyphs and native
/// non-client hit-test areas as GPUI/Zed's platform titlebar.
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    theme: &Theme,
    close: bool,
) -> impl IntoElement {
    let (hover_bg, hover_fg, active_bg, active_fg) = if close {
        let red: gpui::Hsla = gpui::rgb(0xe81123).into();
        (
            red,
            gpui::white(),
            red.opacity(0.8),
            gpui::white().opacity(0.8),
        )
    } else {
        (
            theme.glass_hover(),
            theme.text,
            theme.glass_hover().opacity(0.7),
            theme.text,
        )
    };
    div()
        .id(id)
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.0))
        .text_color(theme.text)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(active_fg))
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

/// A titlebar history button (komet window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(
                icon(icon_path)
                    .size(px(16.0))
                    .text_color(theme.text_muted.opacity(0.35)),
            )
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, owner, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (komet __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    owner: gpui::EntityId,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        // komet __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            crate::theme::wash(0.0),
            crate::theme::wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key, owner))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
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
        let theme = Theme::of(cx);
        // Sidebar and titlebar share one opaque shell tone; the workspace card
        // uses the separate content tone. Keeping both explicit makes their
        // reference colors identical across platforms and Settings.
        let (shell_surface, text, font) = (theme.surface, theme.text, theme.font_sans.clone());
        let workspace_bg = theme.bg;
        let (workspace_scope, auth) = {
            let state = self.state.read(cx);
            (state.workspace_scope, state.auth.clone())
        };
        self.sync_flow = sync_flow_after_auth(self.sync_flow, workspace_scope, auth.as_ref());
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
            GatePhase::Ready => {
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
                let main_width =
                    (viewport - self.sidebar_target() - self.right_target(cx) - 10.0).max(0.0);
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
        let root = root
            .children(self.render_windows_caption_controls(window, cx))
            .children(self.render_linux_caption_controls(window, cx));
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
