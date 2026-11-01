//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring komet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::comments::ReviewComment;

use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;

use komet_doc::SessionMessageEntry;
use komet_engine::environments::LOCAL_ENV_ID;
use komet_proto::{
    AuthState, Chat, ChatIndicator, Device, Environment, Session, Space, WorkspaceScope,
};
use komet_rpc::{RpcClient, methods};

pub mod engine;
mod reducers;
mod workspaces;

pub use engine::{
    EngineBootConfig, EngineHandle, EngineMode, ThreadCompactions, WorkspaceEntry, WorkspaceStatus,
    url_host,
};
pub(crate) use engine::{
    WatchOwner, WorkspaceMemory, wait_for_deferred_engine, workspace_home_device, workspace_version,
};

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `komet_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use komet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, RECONNECT_MAX_ATTEMPTS, SESSION_STALE_MS,
    attention_rank, chat_location, display_status, effective_indicator, format_ms, format_time_ago,
    gate_phase, group_chats, next_reconnect_step, parse_auth_state, project_label,
    reconnect_base_delay_ms, reconnect_jittered_delay_ms, reconnect_should_retry, sort_active,
    sort_chats, sort_spaces, sort_tabs,
};

pub mod org;

pub use org::{OrgRow, org_name_valid, parse_orgs, sort_memberships};

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// A composer send whose doc command is queued but not yet executed by the
/// chat's host device — cleared when the host writes the user message back
/// into the transcript (same client-minted id as the [`AppState::echoes`]
/// dedup), or after [`PENDING_SEND_TTL_MS`].
#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started: DateTime<Utc>,
}

/// How long the send-in-flight overlay may hold before the synced status
/// shows through again. Covers the queue → nudge → drain → sync round-trip
/// to a remote host; when the host is offline the dot falls back to the
/// truth after this.
pub const PENDING_SEND_TTL_MS: i64 = 30_000;

struct UploadProgress {
    done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    total: u64,
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Fixed data boundary of the attached engine. Authentication may change
    /// in place, but changing this scope requires assembling a new runtime.
    pub workspace_scope: Option<WorkspaceScope>,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// The project the new-session canvas mints into. Healed by
    /// [`Self::apply_spaces`] when the row vanishes; selecting a chat implies
    /// its project.
    pub selected_space: Option<String>,
    /// Deliberate "Don't work in a project" pick: while set, the canvas mints
    /// project-less sessions (cwd `~` on the picked device) and
    /// [`Self::selected_space_row`] reads as `None` — healing must NOT
    /// re-select a project underneath it.
    pub no_project: bool,
    /// The composer's device pick — where project-less sessions run, and the
    /// device whose projects the project picker lists. `None` falls back to
    /// the local device.
    pub selected_device: Option<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// First chats / spaces watch frame has landed — device-local state that
    /// prunes against the doc (open tabs, the sidebar space filter) must not
    /// judge by the empty pre-sync lists.
    pub chats_synced: bool,
    pub spaces_synced: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// The selected chat's current plan: latest `ToolCall::Todo` in doc order,
    /// cached so the persistent plan card never rescans a long transcript on
    /// every streaming frame.
    pub active_plan: Option<Vec<komet_proto::TodoItem>>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// Send-in-flight overlay per chat id: a queued doc command the host
    /// hasn't executed yet (see [`Self::begin_pending_send`]).
    pending_sends: HashMap<String, PendingSend>,
    /// The in-flight send's attachment upload, when it has one.
    upload_progress: Option<UploadProgress>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully). Follows the active
    /// workspace: in a paired device it is that device, so every
    /// `targetDeviceId` decision reads correctly.
    pub local_device_id: Option<String>,
    /// The machine this app's own engine runs on — never changes, and says which
    /// registry row is "me" in the local workspace.
    pub home_device_id: Option<String>,
    /// Active workspace: [`LOCAL_ENV_ID`] or a paired device's environment id.
    pub active_workspace: String,
    /// The switcher's paired-device rows (the local machine is implied, first).
    pub workspaces: Vec<WorkspaceEntry>,
    /// Raw registry rows of the paired devices (the local row excluded). Kept for
    /// Settings → Sync: the host, the added-at, and the link re-encode.
    pub paired_envs: Vec<Environment>,
    /// First `WatchEnvironments` frame has landed — an empty list is only
    /// "nothing paired" once this is true.
    pub env_list_synced: bool,
    /// Device rows of the local workspace, kept while a paired device is
    /// displayed so the switcher's local row keeps its real hostname.
    local_devices: Vec<Device>,
    /// Connection of the workspace being displayed; `None` while a paired device
    /// is being dialed — the local engine is never a fallback for a remote
    /// workspace, or its rows would land under the wrong label.
    display_engine: Option<EngineHandle>,
    /// One multiplexed connection per paired device, alive while it is paired.
    env_clients: HashMap<String, Arc<RpcClient>>,
    /// Watches of the active paired device (empty while the local workspace is).
    env_watches: Vec<Task<()>>,
    /// A standing device stream per paired device: it carries the switcher's
    /// online dot, the workspace's device rows, and its connection.
    env_presence: HashMap<String, Task<()>>,
    /// Device rows of the paired devices, kept warm for status and switching.
    env_devices: HashMap<String, Vec<Device>>,
    /// Per-device navigation memory (space/device/chat selection).
    env_memory: HashMap<String, WorkspaceMemory>,
    /// The `WatchEnvironments` stream of the local engine.
    workspaces_task: Option<Task<()>>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<komet_update::UpdateStatus>,
    /// Cumulative context & token usage metrics per chat id.
    pub context_usage: HashMap<String, komet_proto::ContextUsageStats>,
    /// Written by the changes/files pane, read by the composer.
    review_comments: HashMap<String, Vec<ReviewComment>>,
    /// File surfaces whose editor-backed comments currently cite a buffer
    /// revision that has not reached disk yet. A chat remains blocked until
    /// every surface waiting on a workspace write has finished or cancelled.
    review_comment_flushes: HashMap<String, HashSet<u64>>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`,
    /// `security-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    pub workspace_state: Option<komet_proto::WorkspaceState>,
    /// The sandbox level the next message is sent with. It follows the open
    /// chat (see [`access_for_selection`]) and is updated by
    /// [`AppState::sync_access_mode`]; it is not the default for new chats.
    pub access_mode: komet_proto::SandboxLevel,
    /// The level new chats start at, from the Security settings.
    pub new_chat_access: komet_proto::SandboxLevel,
    /// Access-chip click for the current selection (`None` = new-chat canvas).
    /// Held until the persisted row (or Security default) catches up, so a
    /// chats watch frame cannot snap Full access back to Sandboxed.
    pending_access: Option<(Option<String>, komet_proto::SandboxLevel)>,
    /// In-flight `setChatConfig` echo. A chats watch frame that left before
    /// the mutate landed would otherwise replace the stamped row and snap
    /// the access chip back to Sandboxed.
    pending_chat_config: Option<(String, komet_proto::ChatConfig)>,
    engine: Option<EngineHandle>,
    boot_config: Option<EngineBootConfig>,
    /// Watches that follow the engine, not the displayed workspace: assembly
    /// result, auth, updates, this machine's device id.
    watch_tasks: Vec<Task<()>>,
    /// The local workspace's row watches — stopped while a paired device is
    /// displayed, so its frames can't repaint another workspace's lists.
    data_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
    /// Ordinal of the auto-retry attempt in flight, 0 when the chain is idle
    /// (see [`Self::schedule_reconnect`] and the bounded schedule in
    /// `komet_proto::view`).
    pub reconnect_attempt: u32,
    /// Wall clock the pending retry is due at — what the banner counts down
    /// against. `None` whenever no attempt is queued.
    pub reconnect_deadline: Option<DateTime<Utc>>,
    /// A chain attempt is queued or dialing right now. This, and not the status
    /// label, is what keeps two dying watches from forking two retry chains —
    /// the status stays `Reconnecting` across a failure so the countdown never
    /// jumps.
    reconnect_pending: bool,
    /// The sleeping retry attempt. Kept so a manual Retry (which restarts
    /// `bootstrap`) can cancel it: dropping a gpui task cancels it, whereas a
    /// detached one wakes later and forks a second chain.
    reconnect_task: Option<Task<()>>,
    /// Messages the user fired while the link was down: kept whole here, named
    /// by the reason the UI showed, and tagged with the chat they were typed
    /// in so a later drain cannot deliver them to another chat. Handed back
    /// once by [`Self::take_queued_sends_for`]. A cut-off send must not be a
    /// lost paragraph.
    queued_sends: Vec<QueuedSend>,
}

/// One parked message (transparency plan Task 10). The reason is stored with it
/// because that is what makes the wait legible: "queued — reconnecting (2)"
/// reads as a deliberate hold, a silent input box reads as a dead button. The
/// chat key is the owner: `""` is the new-chat canvas, every other key is the
/// chat that was selected when the send was held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSend {
    pub chat_key: String,
    pub text: String,
    pub reason: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            workspace_scope: None,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_space: None,
            no_project: false,
            selected_device: None,
            selected_chat: None,
            transcript: Vec::new(),
            active_plan: None,
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            upload_progress: None,
            local_device_id: None,
            home_device_id: None,
            active_workspace: LOCAL_ENV_ID.to_string(),
            workspaces: Vec::new(),
            paired_envs: Vec::new(),
            env_list_synced: false,
            local_devices: Vec::new(),
            display_engine: None,
            env_clients: HashMap::new(),
            env_watches: Vec::new(),
            env_presence: HashMap::new(),
            env_devices: HashMap::new(),
            env_memory: HashMap::new(),
            workspaces_task: None,
            update: None,
            context_usage: HashMap::new(),
            review_comments: HashMap::new(),
            review_comment_flushes: HashMap::new(),
            data_dir: None,
            workspace_state: None,
            access_mode: komet_proto::SandboxLevel::WorkspaceWrite,
            new_chat_access: komet_proto::SandboxLevel::WorkspaceWrite,
            pending_access: None,
            pending_chat_config: None,
            engine: None,
            boot_config: None,
            watch_tasks: Vec::new(),
            data_tasks: Vec::new(),
            transcript_task: None,
            reconnect_attempt: 0,
            reconnect_deadline: None,
            reconnect_pending: false,
            reconnect_task: None,
            queued_sends: Vec::new(),
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
        }
    }

    pub fn review_comments(&self, key: &str) -> &[ReviewComment] {
        self.review_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, key: &str, comment: ReviewComment) {
        self.review_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_review_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.review_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.review_comments.remove(key);
                self.review_comment_flushes.remove(key);
            }
        }
    }

    pub fn update_review_comment_line(&mut self, key: &str, id: &str, line: u32) {
        if let Some(comment) = self
            .review_comments
            .get_mut(key)
            .and_then(|comments| comments.iter_mut().find(|comment| comment.id == id))
        {
            comment.line = line;
        }
    }

    pub fn rename_review_comment_path(&mut self, key: &str, old_path: &str, new_path: &str) {
        if let Some(comments) = self.review_comments.get_mut(key) {
            for comment in comments
                .iter_mut()
                .filter(|comment| comment.is_file() && comment.path == old_path)
            {
                comment.path = new_path.to_string();
            }
        }
    }

    pub fn take_review_comments(&mut self, key: &str) -> Vec<ReviewComment> {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_review_comments(&mut self, key: &str) {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key);
    }

    pub fn begin_review_comment_flush(&mut self, key: &str, source: u64) {
        self.review_comment_flushes
            .entry(key.to_string())
            .or_default()
            .insert(source);
    }

    pub fn finish_review_comment_flush(&mut self, key: &str, source: u64) {
        let remove_key = self
            .review_comment_flushes
            .get_mut(key)
            .is_some_and(|sources| {
                sources.remove(&source);
                sources.is_empty()
            });
        if remove_key {
            self.review_comment_flushes.remove(key);
        }
    }

    pub fn review_comment_flush_pending(&self, key: &str) -> bool {
        self.review_comment_flushes.contains_key(key)
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        if self.no_project {
            return None;
        }
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    /// The device the new-session canvas targets: the picked project's host
    /// when one is selected, else the explicit device pick, else this device.
    pub fn effective_device_id(&self) -> Option<String> {
        if let Some(space) = self.selected_space_row() {
            return Some(space.device_id.clone());
        }
        self.selected_device
            .clone()
            .or_else(|| self.local_device_id.clone())
    }

    /// The device a per-machine RPC must name. On this computer the engine
    /// answers for itself and needs no target; inside a paired device the
    /// environment runtime has no agent installs of its own (the controller is a
    /// viewer there), so an untargeted call would describe the wrong machine.
    pub fn harness_target_device(&self) -> Option<String> {
        if self.is_local_workspace() {
            return None;
        }
        self.effective_device_id()
    }

    /// Pick the composer's target device. Keeps the project pick consistent:
    /// a project on another device can't survive the switch — fall back to
    /// the first project on the new device, else "no project".
    pub fn select_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let project_moves = self
            .selected_space_row()
            .is_some_and(|s| s.device_id != device_id);
        if project_moves {
            let first = self
                .spaces_sorted()
                .iter()
                .find(|s| s.device_id == device_id)
                .map(|s| s.id.clone());
            self.no_project = first.is_none();
            if first.is_some() {
                self.selected_space = first;
            }
        }
        self.selected_device = Some(device_id);
        cx.notify();
    }

    /// Record the id this machine answers with for `LocalDevice`. It is always
    /// the home row; it only becomes the *displayed* workspace's device while
    /// the local workspace is up, because the probe can land after a switch and
    /// would otherwise aim a paired device's chats at the controller.
    fn note_home_device(&mut self, id: String) {
        self.home_device_id = Some(id.clone());
        if self.active_workspace == LOCAL_ENV_ID {
            self.local_device_id = Some(id);
        }
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// Spaces in display order — case-insensitive alphabetical, the order
    /// both space selectors (sidebar filter, composer picker) list rows in.
    /// Ties break on id so the order is stable across renders.
    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space in tab (creation) order. Chats with a
    /// dangling/missing `space_id` are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The machine this app runs on is trivially online; unknown
    /// devices get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.home_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// The "@ device" tag for a space — shared by the space pickers' rows,
    /// the sidebar filter trigger, and the composer's space chip. Returns
    /// `(tag, offline)`; staleness renders as a disconnected GLYPH at the
    /// call sites (user request), never words in the tag.
    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline = !self.device_online(&space.device_id, now);
        let device = self
            .device_name(&space.device_id)
            .unwrap_or("Unknown device");
        (format!("@ {device}"), offline)
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list). A send in
    /// flight ([`Self::begin_pending_send`]) reads as Working — the queued
    /// command is as good as running.
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.send_pending(&chat.id, now) {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE space,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| match c.space_id.as_deref() {
                // Project-less sessions are first-class rows.
                None => true,
                Some(id) => self.space_row(id).is_some(),
            })
            .map(|c| (self.display_status_for(c, now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row. A send in flight reads as
    /// Working (see [`Self::display_status_for`]).
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        if self.send_pending(chat_id, now) {
            return Indicator::Working;
        }
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection, self.workspace_scope, self.auth.as_ref())
    }

    /// The connection the views must use: the displayed workspace's, which is the
    /// engine's own for the local workspace. `None` while a paired device is
    /// being dialed — a remote workspace never falls back to local rows.
    pub fn engine(&self) -> Option<&EngineHandle> {
        if self.active_workspace != LOCAL_ENV_ID {
            // A paired device that is not connected has no connection to borrow.
            // Falling back to the local engine would write the next chat into the
            // wrong workspace.
            return self.display_engine.as_ref();
        }
        self.engine.as_ref()
    }

    /// This machine's engine: lifecycle (mode, shutdown, reconnect) and the
    /// environment RPC surface, which is local-only by protocol.
    pub fn local_engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    pub fn is_local_workspace(&self) -> bool {
        self.active_workspace == LOCAL_ENV_ID
    }

    /// Which connection the watches of the displayed workspace belong to.
    fn watch_owner(&self) -> WatchOwner {
        if self.is_local_workspace() {
            WatchOwner::Engine
        } else {
            WatchOwner::Workspace(self.active_workspace.clone())
        }
    }

    /// A paired device's switcher row (`None` for the local workspace).
    pub fn workspace_row(&self, env_id: &str) -> Option<&WorkspaceEntry> {
        self.workspaces.iter().find(|row| row.id == env_id)
    }

    /// The switcher's list as the sidebar renders it: this machine first, then
    /// every paired device.
    pub fn workspace_switcher(&self) -> Vec<WorkspaceEntry> {
        let mut rows = vec![WorkspaceEntry {
            id: LOCAL_ENV_ID.to_string(),
            label: self.local_workspace_label(),
            host: String::new(),
            status: WorkspaceStatus::Online,
            device_count: self.local_devices.len().max(1),
            version: workspace_version(&self.local_devices, self.home_device_id.as_deref()),
        }];
        rows.extend(self.workspaces.iter().cloned());
        rows
    }

    /// Keep the local workspace label aligned with the device name itself, without
    /// the "This computer" prefix that duplicates the paired-device row below it.
    pub fn local_workspace_label(&self) -> String {
        self.home_device_id
            .as_deref()
            .and_then(|id| self.local_devices.iter().find(|d| d.id == id))
            .map(|d| d.name.clone())
            .or_else(|| self.local_devices.first().map(|d| d.name.clone()))
            .unwrap_or_else(|| "This computer".to_string())
    }

    /// The switcher trigger's label: the workspace being displayed.
    pub fn active_workspace_label(&self) -> String {
        if self.is_local_workspace() {
            return self.local_workspace_label();
        }
        self.workspace_row(&self.active_workspace)
            .map(|row| row.label.clone())
            .unwrap_or_else(|| "Device".to_string())
    }

    /// Reachability of the displayed workspace, for the trigger's dot.
    pub fn active_workspace_status(&self) -> WorkspaceStatus {
        if self.is_local_workspace() {
            return WorkspaceStatus::Online;
        }
        self.workspace_row(&self.active_workspace)
            .map(|row| row.status)
            .unwrap_or(WorkspaceStatus::Connecting)
    }

    /// Drop every account-scoped view and subscription after its runtime has
    /// stopped. The next bootstrap must never render rows from the previous
    /// account while the local profile is opening.
    pub fn prepare_runtime_replacement(&mut self, cx: &mut Context<Self>) {
        self.engine = None;
        self.watch_tasks.clear();
        self.data_tasks.clear();
        self.transcript_task = None;
        self.clear_workspaces();
        self.connection = ConnectionStatus::Connecting;
        self.workspace_scope = None;
        self.auth = None;
        self.devices.clear();
        self.spaces.clear();
        self.chats.clear();
        self.sessions.clear();
        self.selected_space = None;
        self.no_project = false;
        self.selected_device = None;
        self.selected_chat = None;
        self.auto_selected = false;
        self.pending_access = None;
        self.pending_chat_config = None;
        self.sync_access_mode();
        self.chats_synced = false;
        self.spaces_synced = false;
        self.transcript.clear();
        self.active_plan = None;
        self.echoes.clear();
        self.pending_sends.clear();
        self.queued_sends.clear();
        self.reconnect_task = None;
        self.reconnect_attempt = 0;
        self.reconnect_pending = false;
        self.reconnect_deadline = None;
        self.local_device_id = None;
        self.home_device_id = None;
        self.update = None;
        cx.notify();
    }

    // ---- gpui glue ----

    /// What a dropped link is allowed to throw away: the pumps, and the
    /// workspace relays that rode on them — `attach_engine` re-makes both, and
    /// the transcript of the selected chat is re-subscribed there. What this
    /// does NOT touch is what the user is looking at: the transcript, the
    /// echoes and the outbox survive the gap, so a reconnect never blinks
    /// (transparency plan Task 10).
    fn drop_engine_subscriptions(&mut self) {
        self.watch_tasks.clear();
        self.data_tasks.clear();
        // Paired-device connections ride the same daemon, so they go too and
        // re-attach from the next environment frame.
        self.clear_workspaces();
        self.transcript_task = None;
    }

    /// Re-dial the engine after the link dropped, on the BOUNDED backoff chain:
    /// attempt `n` waits `reconnect_base_delay_ms(n)` ±20 % jitter, the ordinal
    /// and the deadline ride on `ConnectionStatus::Reconnecting` so the wait is
    /// countable, and after `RECONNECT_MAX_ATTEMPTS` the chain stops with
    /// `Failed { retryable: true }` — an endless silent redial reads as an app
    /// that answers nothing. Guarded: one attempt runs at a time, and only from
    /// a previously-Remote or Failed state (InProcess engines are already local).
    /// The transcript, the echoes and the outbox survive: only subscriptions
    /// die, and `attach_engine` re-makes them.
    fn schedule_reconnect(&mut self, reason: String, cx: &mut Context<Self>) {
        if self.reconnect_pending || self.connection == ConnectionStatus::Connecting {
            return; // a chain attempt is already queued or dialing
        }
        self.reconnect_pending = true;
        let Some(config) = self.boot_config.clone() else {
            self.reconnect_pending = false;
            self.connection = ConnectionStatus::Failed {
                message: reason,
                retryable: false,
            };
            cx.notify();
            return;
        };
        // Draw the jitter from the clock: two viewports reconnecting into the
        // same daemon must not peak on it at the same millisecond.
        let unit = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| (since.subsec_micros() % 1_000) as f64 / 1_000.0)
            .unwrap_or(0.5);
        let Some((attempt, delay_ms)) = next_reconnect_step(self.reconnect_attempt, unit) else {
            tracing::error!(attempt = self.reconnect_attempt, %reason, "engine reconnect chain exhausted");
            self.reconnect_deadline = None;
            self.connection = ConnectionStatus::Failed {
                message: reason,
                retryable: true,
            };
            cx.notify();
            return;
        };
        self.reconnect_attempt = attempt;
        self.drop_engine_subscriptions();
        self.reconnect_deadline =
            Some(Utc::now() + chrono::Duration::milliseconds(delay_ms as i64));
        self.connection = ConnectionStatus::Reconnecting {
            attempt,
            retry_in_ms: delay_ms,
            reason,
        };
        // Drop the dead engine handle so `attach_engine` can replace it cleanly.
        // Shutdown is best-effort (remote daemon is already gone).
        if let Some(old) = self.engine.take() {
            let old = old;
            cx.spawn(async move |_, _| old.shutdown().await).detach();
        }
        cx.notify();
        // Re-use the existing bootstrap path (probe → remote-or-embed). We
        // need an `App` context, but we only have `Context<Self>` here — spawn
        // via gpui_tokio's `Tokio` handle and then hop back to gpui for the
        // state update, mirroring `bootstrap`.
        let state = cx.entity().clone();
        self.reconnect_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay_ms))
                .await;
            let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            state.update(cx, |s, cx| {
                // The attempt is over either way: release the guard before
                // deciding, or the chain could never advance past attempt 1.
                s.reconnect_pending = false;
                s.reconnect_deadline = None;
                match outcome {
                    Ok(handle) => s.attach_engine(handle, cx),
                    Err(message) => {
                        tracing::warn!(%message, "engine reconnect attempt failed");
                        // Fall through the chain: this attempt is the one that
                        // just failed, so the next call schedules attempt + 1.
                        s.schedule_reconnect(message, cx);
                    }
                }
            });
        }));
    }

    /// The user pressed Retry: the chain restarts from the top of the schedule
    /// (attempt 0 → 500 ms) rather than continuing where it gave up.
    pub fn reset_reconnect_chain(&mut self) {
        self.reconnect_attempt = 0;
        self.reconnect_deadline = None;
    }

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        let boot_config = config.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            // A manual Retry (and a fresh boot) restarts the chain from the top
            // of the schedule, not from where the last one gave up — and
            // cancels whatever attempt was still sleeping, so its outcome
            // cannot replace the new engine or fork a second chain.
            s.reconnect_task = None;
            s.reconnect_attempt = 0;
            s.reconnect_pending = false;
            s.reconnect_deadline = None;
            s.workspace_scope = None;
            s.auth = None;
            s.data_dir = Some(data_dir.clone());
            s.new_chat_access =
                crate::settings::security::SecurityDefaults::load(&data_dir).default_sandbox;
            s.pending_access = None;
            s.sync_access_mode();
            s.boot_config = Some(boot_config);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    // A cold boot gets no auto-retry chain (the user is looking
                    // at the gate card), but it must never be a dead end.
                    s.connection = ConnectionStatus::Failed {
                        message,
                        retryable: true,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet (chats/devices/auth land with the
    /// workspace doc in M4) fail their subscribe and are skipped gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        let engine_info = handle.engine_info();
        self.workspace_scope = Some(engine_info.workspace_scope);
        self.local_device_id = Some(engine_info.device_id.clone());
        self.home_device_id = Some(engine_info.device_id.clone());
        self.engine = Some(handle.clone());
        // A daemon restart takes its `?env=` connections with it: the viewport
        // lands back on the local workspace and the environment list re-attaches
        // its own pumps below.
        self.clear_workspaces();
        let mut watch_tasks = Vec::with_capacity(8);
        if let Some(task) = spawn_deferred_engine_watch(cx, handle.clone()) {
            watch_tasks.push(task);
        }
        watch_tasks.extend([
            // Auth frames parse tolerantly — engine and proto tags differ today.
            spawn_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                methods::AUTH_STATUS,
                AppState::apply_auth_value,
            ),
            spawn_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
        ]);
        self.watch_tasks = watch_tasks;
        self.start_local_data_watches(cx);
        // Paired devices live on this engine: (re)attach their list and their
        // background streams once the handle is in place.
        self.watch_workspaces(cx);
        // EngineInfo is part of the attachment boundary: views must know which
        // data profile they reached before they are allowed to render Ready.
        self.connection = ConnectionStatus::Ready;
        // The chain succeeded (or this is the first boot): the counter restarts
        // from the top of the schedule, and the deadline goes with the banner.
        self.reconnect_attempt = 0;
        self.reconnect_pending = false;
        self.reconnect_deadline = None;
        // Re-subscribe the transcript if a chat was already selected (reconnect path).
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                handle.client_arc(),
                WatchOwner::Engine,
                chat_id,
            ));
        }
        cx.notify();
    }

    /// The local workspace's row watches. Kept separate from [`Self::watch_tasks`]
    /// because they stop while a paired device is displayed.
    fn start_local_data_watches(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.engine.as_ref().map(|handle| handle.client_arc()) else {
            return;
        };
        self.data_tasks = vec![
            spawn_watch(
                cx,
                client.clone(),
                WatchOwner::Engine,
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, client.clone(), WatchOwner::Engine),
            spawn_watch(
                cx,
                client.clone(),
                WatchOwner::Engine,
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                client,
                WatchOwner::Engine,
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
        ];
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        // Each chat uses its own level; a fresh new-chat screen uses the
        // Security default (drop any leftover canvas click).
        self.pending_access = None;
        self.sync_access_mode();
        self.transcript.clear();
        self.active_plan = None;
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its project (or the lack of one); `select_chat(None)`
            // (the new-session canvas) keeps the current project pick.
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                match chat.space_id.clone() {
                    Some(space_id) => {
                        self.selected_space = Some(space_id);
                        self.no_project = false;
                    }
                    None => {
                        self.no_project = true;
                        self.selected_device = Some(chat.device_id.clone());
                    }
                }
            }
            self.mark_chat_seen(id, cx);
        }
        if let Some(chat_id) = chat_id
            && let Some(client) = self.engine().map(|handle| handle.client_arc())
        {
            self.transcript_task = Some(spawn_transcript_watch(
                cx,
                client,
                self.watch_owner(),
                chat_id,
            ));
        }
        cx.notify();
    }

    /// Select a project; the caller (shell) decides which chat to land on.
    /// `Some` clears a "Don't work in a project" opt-out and re-aims the
    /// device pick at the project's host; `None` IS that opt-out.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        match &space_id {
            Some(id) => {
                self.no_project = false;
                if let Some(device) = self.space_row(id).map(|s| s.device_id.clone()) {
                    self.selected_device = Some(device);
                }
            }
            None => self.no_project = true,
        }
        if self.selected_space == space_id && space_id.is_some() {
            cx.notify();
            return;
        }
        if space_id.is_some() {
            self.selected_space = space_id;
        }
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    /// Window-focus liveness sweep: ask the engine to probe every open room
    /// (workspace + chat docs). Fire-and-forget; each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so spamming is harmless.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.local_engine().cloned() else {
            return;
        };
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({});
            if let Err(err) = handle.client().call(methods::PROBE_SYNC, params).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine().cloned() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }

    // ---- file editor helpers ----

    /// Read a file's text content from the local filesystem.
    pub fn load_file_content(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    /// Write text content to a file on the local filesystem.
    pub fn save_file_content(&self, path: &str, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| e.to_string())
    }
}

pub mod watches;

pub use watches::access_for_selection;
pub(crate) use watches::{
    spawn_chats_watch, spawn_deferred_engine_watch, spawn_local_device_probe,
    spawn_transcript_watch, spawn_watch,
};

#[cfg(test)]
mod tests;
