use chrono::{DateTime, Utc};
use komet_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use komet_proto::{AuthState, Chat, Device, Session, Space, WorkspaceScope};

use super::{
    AppState, ConnectionStatus, PENDING_SEND_TTL_MS, PendingSend, QueuedSend, ThreadCompactions,
    UploadProgress, WorkspaceStatus, access_for_selection, format_ms, parse_auth_state, sort_chats,
    sort_spaces,
};

impl AppState {
    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        // Keep the optimistic setChatConfig stamp until the watch frame
        // carries the same config (or the chat disappears). Otherwise a
        // snapshot that left before the mutate landed snaps the access chip
        // from Full access back to Sandboxed.
        self.pending_chat_config = match self.pending_chat_config.take() {
            Some((id, pending)) => match chats.iter_mut().find(|c| c.id == id) {
                Some(chat) if chat.config.as_ref() == Some(&pending) => None,
                Some(chat) => {
                    chat.config = Some(pending.clone());
                    Some((id, pending))
                }
                None => None,
            },
            None => None,
        };
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // A minted chat whose CreateChat is still in flight lacks its
            // row: keep the optimistic selection (the send runs under it).
            // An expired/stale pending entry reads as truly vanished.
            let pending_mint = self.pending_sends.get(selected).is_some_and(|p| {
                chrono::Utc::now()
                    .signed_duration_since(p.started)
                    .num_milliseconds()
                    <= PENDING_SEND_TTL_MS
            });
            if !pending_mint {
                // Selected chat vanished (deleted elsewhere): drop selection + transcript.
                self.selected_chat = None;
                self.pending_access = None;
                self.transcript.clear();
                self.active_plan = None;
                self.transcript_task = None;
            }
        }
        // The update may change the open chat's level from another device, or
        // remove the open chat, so recompute the level.
        self.sync_access_mode();
    }

    /// Recompute [`AppState::access_mode`] from the current selection.
    pub fn sync_access_mode(&mut self) {
        let resolved = access_for_selection(
            &self.chats,
            self.selected_chat.as_deref(),
            self.new_chat_access,
        );
        let pending = self.pending_access.take();
        self.access_mode = match pending {
            Some((owner, level)) if owner.as_deref() == self.selected_chat.as_deref() => {
                if resolved == level {
                    resolved
                } else {
                    // Click has not landed in the saved row yet (new-chat draft,
                    // unknown harness, or a stale chats frame).
                    self.pending_access = Some((owner, level));
                    level
                }
            }
            _ => resolved,
        };
    }

    /// Advance the access chip. Watch frames keep this level until the open
    /// chat's saved config (or the new-chat default) matches it.
    pub fn set_access_mode(&mut self, level: komet_proto::SandboxLevel) {
        self.pending_access = Some((self.selected_chat.clone(), level));
        self.access_mode = level;
    }

    /// Security-settings default. On the new-chat screen the chip follows
    /// the value the user just picked.
    pub fn set_new_chat_access(&mut self, level: komet_proto::SandboxLevel) {
        self.new_chat_access = level;
        if self.selected_chat.is_none() {
            self.pending_access = None;
        }
        self.sync_access_mode();
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        for session in &sessions {
            if let Some(stats) = &session.context_usage {
                self.context_usage
                    .insert(session.chat_id.clone(), stats.clone());
            }
        }
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        // Heal a vanished selection (project deleted elsewhere): fall back to
        // the first project; its chats died with it, so a matching chat
        // selection is healed by the accompanying chats frame (`apply_chats`).
        // The picker lists projects per-device, so healing prefers one on the
        // picked device — a global fallback would silently re-aim the canvas
        // at another machine.
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.first_space_on_picked_device();
        }
        // First frame with no selection yet: pick the first project so the
        // canvas never boots project-less by accident — unless the user
        // deliberately opted out.
        if self.selected_space.is_none() && !self.no_project {
            self.selected_space = self.first_space_on_picked_device();
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: komet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config.clone());
        }
        self.pending_chat_config = Some((chat_id.to_string(), config));
        self.sync_access_mode();
    }

    /// Drop an in-flight config echo (mutate failed) so the next watch frame
    /// is allowed to replace the stamped row.
    pub fn discard_pending_chat_config(&mut self) {
        self.pending_chat_config = None;
        self.sync_access_mode();
    }

    pub fn apply_devices(&mut self, mut devices: Vec<Device>) {
        if self.is_local_workspace() {
            self.local_devices = devices.clone();
        }
        // A local-only workspace has no remote device identity to distinguish.
        // Keep the engine's legacy sentinel out of the UI while preserving real
        // hostnames and user-assigned device names.
        if self.workspace_scope == Some(WorkspaceScope::Local)
            && let Some(local_id) = self.local_device_id.as_deref()
            && let Some(device) = devices.iter_mut().find(|device| device.id == local_id)
            && device.name == "unknown-device"
        {
            device.name = "Local".to_string();
        }
        self.devices = devices;
    }

    /// First project on the composer's picked device (falling back through
    /// the local device, then any project at all — better a cross-device
    /// project than a surprise project-less canvas). Display order.
    fn first_space_on_picked_device(&self) -> Option<String> {
        let device = self
            .selected_device
            .as_deref()
            .or(self.local_device_id.as_deref());
        let sorted = self.spaces_sorted();
        device
            .and_then(|d| sorted.iter().find(|s| s.device_id == d).copied())
            .or_else(|| sorted.first().copied())
            .map(|s| s.id.clone())
    }

    pub fn apply_update(&mut self, status: komet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_context_usage(&mut self, chat_id: &str, stats: komet_proto::ContextUsageStats) {
        self.context_usage.insert(chat_id.to_string(), stats);
    }

    /// Derives or returns the current context usage metrics for the selected chat.
    pub fn current_context_usage(&self) -> komet_proto::ContextUsageStats {
        let estimated = self.transcript_token_estimate();
        if let Some(chat_id) = &self.selected_chat
            && let Some(mut stats) = self.context_usage.get(chat_id).cloned()
        {
            // Cursor (and similar) bill tool-loop prompt tokens far past the
            // window, then the adapter used to clamp that to 100%. That is
            // not occupancy — the session keeps going. Prefer the transcript
            // estimate so the ring can move instead of pinning full.
            let billed_overshoot = stats.source == komet_proto::ContextUsageSource::Approximate
                && stats.context_limit > 0
                && stats.input_tokens > stats.context_limit
                && stats.used() >= stats.context_limit;
            if billed_overshoot && estimated > 0 {
                stats.used_tokens = estimated;
                stats.source = komet_proto::ContextUsageSource::Estimated;
            }
            return stats;
        }
        let model_name = self
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
            .and_then(|cfg| cfg.model.as_deref())
            .unwrap_or("claude-3-7-sonnet");
        let limit = komet_proto::default_context_limit_for_model(model_name);
        let mut stats = komet_proto::ContextUsageStats::new(limit);
        stats.source = komet_proto::ContextUsageSource::Estimated;
        stats.input_tokens = estimated;
        stats
    }

    fn transcript_token_estimate(&self) -> u64 {
        let mut total_chars: usize = 0;
        for entry in &self.transcript {
            for part in &entry.parts {
                match part {
                    komet_doc::MessagePart::Text { text, .. } => {
                        total_chars += text.len();
                    }
                    komet_doc::MessagePart::Tool {
                        output: Some(out), ..
                    } => {
                        total_chars += out.len();
                    }
                    _ => {}
                }
            }
        }
        (total_chars / 4) as u64
    }

    /// What the thread itself carries about compaction: the boundaries the doc
    /// holds, and why the newest one happened. Harnesses are inconsistent —
    /// some report a count in their usage stats, some report nothing — so the
    /// Context popover reads the doc too: a boundary in the transcript is a
    /// fact, while a missing stat is only silence.
    pub fn thread_compactions(&self) -> ThreadCompactions {
        let mut out = ThreadCompactions::default();
        for entry in &self.transcript {
            for part in &entry.parts {
                if let komet_doc::MessagePart::Compaction {
                    status, trigger, ..
                } = part
                {
                    match status {
                        komet_proto::CompactionStatus::InProgress => out.in_progress = true,
                        _ => out.count += 1,
                    }
                    // Newest wins: entries arrive in order, so the last
                    // settled boundary's trigger is the one the popover shows.
                    if let Some(trigger) = trigger {
                        out.last_trigger = Some(trigger.clone());
                    }
                }
            }
        }
        out
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&komet_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.refresh_active_plan();
        self.ack_pending_send_from_transcript();
    }

    /// Recompute [`Self::active_plan`] from the transcript. Pure backward scan
    /// (last `Todo` wins), so it is cheap while the plan sits near the tail.
    fn refresh_active_plan(&mut self) {
        self.active_plan = komet_doc::latest_plan(&self.transcript);
    }

    /// Apply a `WatchDocMessages` delta frame in place. `Err` = this copy has
    /// diverged; the watch task resubscribes for a fresh reset.
    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        let had_entries = self.transcript.len();
        komet_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        // Plans update in place on the streaming tail; only that case (or a
        // new/removed entry) can change the answer. Skipping the scan keeps
        // frame-time cost flat for long chats.
        let tail_has_plan = self.transcript.last().is_some_and(|entry| {
            entry.parts.iter().any(|part| {
                matches!(
                    part,
                    komet_doc::MessagePart::Tool {
                        call: komet_proto::ToolCall::Todo { .. },
                        ..
                    }
                )
            })
        });
        if tail_has_plan || self.transcript.len() != had_entries {
            self.refresh_active_plan();
        }
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.ack_pending_send_from_transcript();
        Ok(())
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Truncate transcript from a target message ID and clear pending echoes.
    /// Used when editing a prompt to erase the prompt and all its subsequent responses.
    pub fn truncate_transcript_from(&mut self, chat_id: &str, target_message_id: &str) {
        if let Some(idx) = self
            .transcript
            .iter()
            .position(|e| e.id == target_message_id)
        {
            self.transcript.truncate(idx);
        }
        self.refresh_active_plan();
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.clear();
        }
    }

    /// Composer send fired: overlay the chat as Working until the host writes
    /// the user message back into the transcript (or the TTL lapses). A remote
    /// send has no live session row until the host drains the queued command —
    /// that gap read as "no live run" and flashed the Completed dot, and any
    /// phantom Working→Idle edge in it rang the done-chime on send (user
    /// report 2026-08-05).
    pub fn begin_pending_send(&mut self, chat_id: &str, message_id: &str, now: DateTime<Utc>) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started: now,
            },
        );
    }

    /// Send failed — drop the overlay so the dot tells the truth again. Only
    /// removes the overlay this message started: a quick resend must not lose
    /// its own overlay to the first send's failure cleanup.
    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|p| p.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Is a send still in flight for this chat (unacked, inside the TTL)?
    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
        })
    }

    /// When the in-flight send (if any, inside the TTL) was fired — the
    /// elapsed-timer base while the overlay reads as Working. The session
    /// row's `started_at` still belongs to the PREVIOUS turn during this
    /// window, and showing it made a fresh send open at the old turn's
    /// half-hour mark.
    pub fn pending_send_started(&self, chat_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.pending_sends
            .get(chat_id)
            .filter(|p| {
                now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
            })
            .map(|p| p.started)
    }

    /// The host executed the queued command iff the sent message's id showed
    /// up in the transcript (it writes the message before — causally with —
    /// the Working status; sessions.rs dispatch paths).
    fn ack_pending_send_from_transcript(&mut self) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(pending) = self.pending_sends.get(chat_id)
            && self.transcript.iter().any(|e| e.id == pending.message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- offline outbox (transparency plan Task 10) ----

    /// Why a send cannot leave right now, in words — `None` when the link is
    /// Ready. The composer queues against this string instead of showing a
    /// button that does nothing; naming the reason is the whole point. Reads
    /// [`Self::link_status`], so a paired environment that is offline blocks
    /// exactly like a dead engine.
    pub fn send_block_reason(&self) -> Option<String> {
        match self.link_status() {
            ConnectionStatus::Ready => None,
            ConnectionStatus::Connecting => Some("connecting".to_string()),
            ConnectionStatus::Reconnecting {
                attempt,
                retry_in_ms,
                ..
            } => Some(format!(
                "reconnecting ({attempt}, {})",
                format_ms(retry_in_ms)
            )),
            ConnectionStatus::Degraded { reason } => Some(format!("link degraded: {reason}")),
            ConnectionStatus::Failed { retryable, .. } => Some(if retryable {
                "engine unreachable — press Retry".to_string()
            } else {
                "engine unreachable".to_string()
            }),
        }
    }

    /// Park a message the link cannot carry, under the chat it was typed in.
    /// Returns the reason shown on the row. Duplicates of the
    /// immediately-preceding entry for the same chat are refused: a user
    /// pressing Return twice in a row while nothing visibly happened must not
    /// queue the same paragraph twice.
    pub fn queue_send(&mut self, chat_key: String, text: String, use_mcp: bool) -> String {
        let reason = self.send_block_reason().unwrap_or_default();
        if self
            .queued_sends
            .last()
            .is_some_and(|last| last.chat_key == chat_key && last.text == text)
        {
            return reason;
        }
        self.queued_sends.push(QueuedSend {
            chat_key,
            text,
            reason,
            use_mcp,
        });
        self.queued_sends.last().unwrap().reason.clone()
    }

    /// The parked messages, oldest first.
    pub fn queued_sends(&self) -> &[QueuedSend] {
        &self.queued_sends
    }

    /// Hand over the parked messages owned by `chat_key` and leave the other
    /// chats' entries in the outbox. Taking before sending is the dedupe
    /// guarantee: the texts are out of the state before the first send runs,
    /// so a link that dies mid-drain has `send()` re-park that one text once —
    /// nothing replays, nothing is lost.
    pub fn take_queued_sends_for(&mut self, chat_key: &str) -> Vec<QueuedSend> {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued_sends)
            .into_iter()
            .partition(|queued| queued.chat_key == chat_key);
        self.queued_sends = rest;
        mine
    }

    /// The status the shell narrates: the engine link, or — while a paired
    /// environment is displayed — that environment's reachability spoken in the
    /// SAME vocabulary. A paired env that is offline is `Degraded` (this app
    /// works, the thing you are looking at does not), never a dead gate card, and
    /// its rows stay hidden: only the env's own pump can fill them. Pure.
    pub fn link_status(&self) -> ConnectionStatus {
        if self.is_local_workspace() {
            return self.connection.clone();
        }
        match self.active_workspace_status() {
            WorkspaceStatus::Connecting => ConnectionStatus::Connecting,
            WorkspaceStatus::Offline => ConnectionStatus::Degraded {
                reason: format!(
                    "{} is offline",
                    self.workspace_row(&self.active_workspace)
                        .map(|row| row.label.clone())
                        .unwrap_or_else(|| "Environment".to_string())
                ),
            },
            // Reachable environment: the app-level link still decides — a
            // reconnecting engine takes its relay with it.
            WorkspaceStatus::Online => self.connection.clone(),
        }
    }

    /// Attachment upload starting: expose its progress to the working label.
    pub fn begin_upload_progress(
        &mut self,
        total: u64,
        done: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.upload_progress = Some(UploadProgress { done, total });
    }

    /// Upload leg over (success or failure) — the label goes back to plain
    /// send/working wording.
    pub fn end_upload_progress(&mut self) {
        self.upload_progress = None;
    }

    /// Percent of the in-flight attachment upload, clamped to 99 — the last
    /// point belongs to the commit + queue, so "100% but still spinning"
    /// never shows. `None` when no upload is in flight (or it's empty).
    pub fn upload_progress_percent(&self) -> Option<u8> {
        let progress = self.upload_progress.as_ref()?;
        if progress.total == 0 {
            return None;
        }
        let done = progress
            .done
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(progress.total);
        Some(((done * 100) / progress.total).min(99) as u8)
    }
}
