use std::collections::HashSet;

use gpui::Context;
use komet_engine::environments::LOCAL_ENV_ID;
use komet_proto::{Device, EngineInfo, Environment};
use komet_rpc::methods;

use super::{
    AppState, EngineHandle, WatchOwner, WorkspaceEntry, WorkspaceMemory, WorkspaceStatus,
    spawn_chats_watch, spawn_transcript_watch, spawn_watch, url_host, workspace_home_device,
    workspace_version,
};

impl AppState {
    // ---- paired-device workspaces ----

    /// Follow the engine's environment list: the switcher's rows, one standing
    /// device stream per paired device, and the cleanup when one is removed.
    pub(crate) fn watch_workspaces(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.engine.as_ref().map(|handle| handle.client_arc()) else {
            return;
        };
        self.workspaces_task = Some(cx.spawn(async move |this, cx| {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            loop {
                let mut rx = match client
                    .subscribe(methods::WATCH_ENVIRONMENTS, serde_json::json!({}))
                    .await
                {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, "environment list unavailable; retrying");
                        if this.update(cx, |_, _| {}).is_err() {
                            return;
                        }
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue;
                    }
                };
                while let Some(value) = rx.recv().await {
                    match serde_json::from_value::<Vec<Environment>>(value) {
                        Ok(envs) => {
                            let _ = this.update(cx, |state, cx| state.apply_environments(envs, cx));
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "dropping malformed environments frame");
                        }
                    }
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor().timer(RETRY_DELAY).await;
            }
        }));
    }

    /// Rebuild the switcher's rows from the engine's list, keep a presence pump
    /// per row, and drop everything a removed device owned.
    fn apply_environments(&mut self, envs: Vec<Environment>, cx: &mut Context<Self>) {
        self.paired_envs = envs
            .iter()
            .filter(|env| env.id != LOCAL_ENV_ID)
            .cloned()
            .collect();
        let mut rows = Vec::new();
        for env in envs {
            if env.id == LOCAL_ENV_ID {
                continue;
            }
            if env.namespace.is_none() {
                // No namespace means the runtime never opened; there is nothing
                // to connect to until the device is paired again.
                continue;
            }
            let status = self
                .workspace_row(&env.id)
                .map(|row| row.status)
                .unwrap_or(WorkspaceStatus::Connecting);
            let devices: &[Device] = self
                .env_devices
                .get(&env.id)
                .map_or(&[] as &[Device], Vec::as_slice);
            let own = workspace_home_device(devices, self.home_device_id.as_deref());
            let version = workspace_version(devices, own.as_deref());
            rows.push(WorkspaceEntry {
                id: env.id,
                label: env.label,
                host: url_host(&env.url),
                status,
                device_count: devices.len(),
                version,
            });
        }
        let listed: HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
        let vanished: Vec<String> = self
            .workspaces
            .iter()
            .map(|row| row.id.clone())
            .filter(|id| !listed.contains(id))
            .collect();
        self.workspaces = rows;
        self.env_list_synced = true;
        for id in &vanished {
            self.forget_workspace(id);
        }
        let ids: Vec<String> = self.workspaces.iter().map(|row| row.id.clone()).collect();
        for id in ids {
            self.ensure_presence(cx, &id);
        }
        if vanished.contains(&self.active_workspace) {
            // The displayed device is unpaired: its rows are nothing now.
            self.switch_workspace(LOCAL_ENV_ID.to_string(), cx);
        }
        cx.notify();
    }

    /// Drop a removed device's connection, pumps, rows and navigation memory.
    fn forget_workspace(&mut self, env_id: &str) {
        self.env_presence.remove(env_id);
        self.env_clients.remove(env_id);
        self.env_devices.remove(env_id);
        self.env_memory.remove(env_id);
        if self.active_workspace == env_id {
            self.stop_workspace_watches();
            self.transcript_task = None;
        }
    }

    /// Forget every paired device. The engine that served them is gone (runtime
    /// replacement or daemon restart); the next environment frame re-attaches.
    pub(crate) fn clear_workspaces(&mut self) {
        let was_remote = self.active_workspace != LOCAL_ENV_ID;
        self.env_presence.clear();
        self.env_clients.clear();
        self.env_devices.clear();
        self.env_memory.clear();
        self.workspaces.clear();
        self.paired_envs.clear();
        self.env_list_synced = false;
        self.workspaces_task = None;
        self.stop_workspace_watches();
        if was_remote {
            self.active_workspace = LOCAL_ENV_ID.to_string();
            self.clear_lists();
        }
    }

    /// One standing device stream per paired device, created on demand. It owns
    /// that environment's connection: the switcher's dot, the workspace's device
    /// rows, and the client its other watches ride all come from this one dial,
    /// and it is the only thing that re-dials a device that went away.
    fn ensure_presence(&mut self, cx: &mut Context<Self>, env_id: &str) {
        if self.env_presence.contains_key(env_id) {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let env_id = env_id.to_string();
        let task_env_id = env_id.clone();
        let task = cx.spawn(async move |this, cx| {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
            loop {
                let client = match engine.env_client(&env_id).await {
                    Ok(client) => client,
                    Err(err) => {
                        tracing::debug!(%env_id, error = %err, "device workspace unreachable");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue;
                    }
                };
                match client
                    .subscribe(methods::WATCH_DEVICES, serde_json::json!({}))
                    .await
                {
                    Err(err) => {
                        tracing::debug!(%env_id, error = %err, "device stream unavailable");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            match serde_json::from_value::<Vec<Device>>(value) {
                                Ok(devices) => {
                                    let client = client.clone();
                                    let alive = this.update(cx, |state, cx| {
                                        state.env_clients.insert(env_id.clone(), client);
                                        state.record_workspace_devices(&env_id, devices, cx);
                                    });
                                    if alive.is_err() {
                                        return;
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(%env_id, error = %err, "dropping malformed device frame");
                                }
                            }
                        }
                        tracing::debug!(%env_id, "device stream ended; re-dialing");
                        if this
                            .update(cx, |state, cx| state.handle_workspace_lost(&env_id, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(RETRY_DELAY).await;
            }
        });
        self.env_presence.insert(task_env_id, task);
    }

    /// Device rows of one paired workspace. This machine is a controller there,
    /// never a device — so the workspace's own machine becomes the composer's
    /// pick, and [`AppState::local_device_id`] follows it.
    fn record_workspace_devices(
        &mut self,
        env_id: &str,
        devices: Vec<Device>,
        cx: &mut Context<Self>,
    ) {
        self.set_workspace_status(env_id, WorkspaceStatus::Online);
        let own = workspace_home_device(&devices, self.home_device_id.as_deref());
        if let Some(row) = self.workspaces.iter_mut().find(|row| row.id == env_id) {
            row.device_count = devices.len();
            row.version = workspace_version(&devices, own.as_deref());
        }
        self.env_devices.insert(env_id.to_string(), devices.clone());
        if self.active_workspace == env_id {
            let home = own;
            self.apply_devices(devices);
            if self.selected_device.is_none() {
                self.selected_device = home.clone();
            }
            self.local_device_id = home;
            self.attach_workspace_client(env_id, cx);
        }
        cx.notify();
    }

    fn set_workspace_status(&mut self, env_id: &str, status: WorkspaceStatus) {
        if let Some(row) = self.workspaces.iter_mut().find(|row| row.id == env_id) {
            row.status = status;
        }
    }

    /// The device stream said the connection is gone: show it in the switcher and
    /// stop speaking through it. The rows stay as a read-only mirror — the
    /// presence pump re-dials, the local workspace is untouched.
    pub(crate) fn handle_workspace_lost(&mut self, env_id: &str, cx: &mut Context<Self>) {
        self.env_clients.remove(env_id);
        self.set_workspace_status(env_id, WorkspaceStatus::Offline);
        if self.active_workspace == env_id {
            self.stop_workspace_watches();
            self.transcript_task = None;
        }
        cx.notify();
    }

    /// Display another workspace. Every row list is cleared first: not one line
    /// of the outgoing workspace may render under the incoming device's label.
    pub fn switch_workspace(&mut self, env_id: String, cx: &mut Context<Self>) {
        let is_local = env_id == LOCAL_ENV_ID;
        if self.active_workspace == env_id && (is_local || self.display_engine.is_some()) {
            return;
        }
        let outgoing = self.active_workspace.clone();
        if outgoing != LOCAL_ENV_ID {
            self.env_memory.insert(
                outgoing,
                WorkspaceMemory {
                    selected_space: self.selected_space.clone(),
                    selected_device: self.selected_device.clone(),
                    selected_chat: self.selected_chat.clone(),
                    no_project: self.no_project,
                },
            );
        }
        self.stop_workspace_watches();
        self.data_tasks.clear();
        self.transcript_task = None;
        self.active_workspace = env_id.clone();
        self.clear_lists();
        if is_local {
            self.local_device_id = self.home_device_id.clone();
            self.start_local_data_watches(cx);
        } else {
            // Warm rows mean the composer can target that machine before its
            // first frame lands; the watches start with the connection.
            let devices = self.env_devices.get(&env_id).cloned().unwrap_or_default();
            self.local_device_id = workspace_home_device(&devices, self.home_device_id.as_deref());
            self.selected_device = self.local_device_id.clone();
            if let Some(memory) = self.env_memory.get(&env_id) {
                self.selected_space = memory.selected_space.clone();
                self.selected_device = memory
                    .selected_device
                    .clone()
                    .or_else(|| self.local_device_id.clone());
                self.selected_chat = memory.selected_chat.clone();
                self.no_project = memory.no_project;
            }
            self.attach_workspace_client(&env_id, cx);
        }
        cx.notify();
    }

    fn clear_lists(&mut self) {
        self.chats.clear();
        self.spaces.clear();
        self.devices.clear();
        self.sessions.clear();
        self.transcript.clear();
        self.active_plan = None;
        self.context_usage.clear();
        self.echoes.clear();
        self.pending_sends.clear();
        self.review_comments.clear();
        self.upload_progress = None;
        self.workspace_state = None;
        self.selected_chat = None;
        self.selected_space = None;
        self.selected_device = None;
        self.no_project = false;
        self.auto_selected = false;
        self.chats_synced = false;
        self.spaces_synced = false;
        self.pending_access = None;
        self.pending_chat_config = None;
        self.sync_access_mode();
    }

    /// Aim the display at a paired device's connection, once it exists.
    fn attach_workspace_client(&mut self, env_id: &str, cx: &mut Context<Self>) {
        if self.active_workspace != env_id || self.display_engine.is_some() {
            return;
        }
        let Some(client) = self.env_clients.get(env_id).cloned() else {
            return;
        };
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let engine_info = EngineInfo {
            device_id: self.local_device_id.clone().unwrap_or_default(),
            workspace_scope: engine.engine_info().workspace_scope,
        };
        let owner = WatchOwner::Workspace(env_id.to_string());
        self.display_engine = Some(EngineHandle::for_workspace(
            client.clone(),
            engine_info,
            engine.mode(),
        ));
        self.env_watches = vec![
            spawn_watch(
                cx,
                client.clone(),
                owner.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, client.clone(), owner.clone()),
            spawn_watch(
                cx,
                client.clone(),
                owner.clone(),
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
        ];
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, client, owner, chat_id));
        }
    }

    fn stop_workspace_watches(&mut self) {
        self.env_watches.clear();
        self.display_engine = None;
    }
}
