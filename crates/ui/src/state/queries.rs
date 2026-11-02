//! Read-side selectors: chats, spaces, devices, sessions and the gate phase.

use super::*;

impl AppState {
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
    pub(super) fn note_home_device(&mut self, id: String) {
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
    pub(super) fn watch_owner(&self) -> WatchOwner {
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
}
