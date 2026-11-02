//! Engine runtime glue: replacement, reconnect chain, bootstrap, attach and
//! the local data watches.

use super::*;

impl AppState {
    /// What a dropped link is allowed to throw away: the pumps, and the
    /// workspace relays that rode on them — `attach_engine` re-makes both, and
    /// the transcript of the selected chat is re-subscribed there. What this
    /// does NOT touch is what the user is looking at: the transcript, the
    /// echoes and the outbox survive the gap, so a reconnect never blinks
    /// (transparency plan Task 10).
    pub(super) fn drop_engine_subscriptions(&mut self) {
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
    pub(super) fn schedule_reconnect(&mut self, reason: String, cx: &mut Context<Self>) {
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
    pub(super) fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
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
    pub(super) fn start_local_data_watches(&mut self, cx: &mut Context<Self>) {
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
}
