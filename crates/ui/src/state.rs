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

mod comments;
pub mod engine;
mod queries;
mod reducers;
mod runtime;
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
