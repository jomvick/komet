//! Engine glue for komet-memory (issue #78, Phase 1).
//!
//! [`MemoryIndexer`] sits on the journal/broadcast boundary: every event
//! `Inner::publish` journals is offered to the allow-listed projection here.
//! The projection is synchronous and allocation-light — it only builds a
//! compact [`MemoryRecord`] and `try_send`s it to the store's bounded queue.
//! Nothing on this path touches SQLite; on queue overflow the journal stays
//! authoritative and [`MemoryIndexer::catch_up`] replays the gap.
//!
//! Replay caveat: journal lines carry no wall-clock timestamp, so events
//! recovered by catch-up get `occurred_at_ms` ≈ the journal file's mtime
//! (its last write — a good approximation of the run's end, and far better
//! than stamping a months-old backfill "now"). Live events get real
//! publish-time stamps. Ordering (and therefore briefing recency within a
//! catch-up batch) stays correct via `source_seq`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use komet_memory::{MemoryItem, MemoryRecord, MemoryService};
use komet_proto::{AgentEvent, HarnessId};

use crate::now_ms;
use crate::run_journal::RunJournal;

/// Per-chat projection state: project identity plus in-flight tool calls
/// (needed to attach name/path/outcome to `ToolResult` rows).
struct ChatMemoryCtx {
    project_id: String,
    cwd: String,
    /// Canonical cwd, resolved once per chat — `canonicalize` is a syscall
    /// and must not run per tool call on the publish path.
    canon_cwd: PathBuf,
    agent: String,
    model: Option<String>,
    run_started_ms: i64,
    /// tool_call_id → (tool_name, relative_path) for pairing results.
    pending_tools: HashMap<String, (String, Option<String>)>,
}

/// cwd → project_id memo (canonicalize is a syscall; chats reuse cwds).
struct MemoryProjector {
    chats: HashMap<String, ChatMemoryCtx>,
    cwd_projects: HashMap<String, String>,
}

impl MemoryProjector {
    fn project_id(&mut self, cwd: &str) -> String {
        if let Some(id) = self.cwd_projects.get(cwd) {
            return id.clone();
        }
        let id = komet_memory::project_id_for(cwd);
        self.cwd_projects.insert(cwd.to_string(), id.clone());
        id
    }

    /// Seed/refresh chat context at dispatch time (cwd + harness are known
    /// before the first `SessionStarted` arrives).
    fn note_chat(&mut self, chat_id: &str, cwd: &str, harness: HarnessId) {
        let project_id = self.project_id(cwd);
        let canon_cwd = komet_memory::canonical_cwd(cwd);
        let ctx = self
            .chats
            .entry(chat_id.to_string())
            .or_insert_with(|| ChatMemoryCtx {
                project_id: project_id.clone(),
                cwd: cwd.to_string(),
                canon_cwd: canon_cwd.clone(),
                agent: harness.as_str().to_string(),
                model: None,
                run_started_ms: 0,
                pending_tools: HashMap::new(),
            });
        if ctx.cwd != cwd {
            ctx.cwd = cwd.to_string();
            ctx.project_id = project_id;
            ctx.canon_cwd = canon_cwd;
        }
        ctx.agent = harness.as_str().to_string();
    }

    fn ctx(&mut self, chat_id: &str) -> Option<&mut ChatMemoryCtx> {
        self.chats.get_mut(chat_id)
    }

    /// Project one journaled event into 0..=2 queue items. `now_ms` is the
    /// caller's clock (publish time live, catch-up time on replay).
    fn project(
        &mut self,
        chat_id: &str,
        seq: u64,
        event: &AgentEvent,
        now: i64,
    ) -> Vec<MemoryItem> {
        match event {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                ..
            } => {
                // Catch-up replays journals for chats this engine instance
                // never dispatched: seed the ctx from the event itself so the
                // whole tail can project. Live path already has a ctx from
                // `note_chat`.
                if !cwd.trim().is_empty() {
                    self.project_id(cwd);
                }
                let project_id = if cwd.trim().is_empty() {
                    self.ctx(chat_id)
                        .map(|c| c.project_id.clone())
                        .unwrap_or_default()
                } else {
                    self.project_id(cwd)
                };
                if project_id.is_empty() {
                    return Vec::new();
                }
                let ctx = self
                    .chats
                    .entry(chat_id.to_string())
                    .or_insert_with(|| ChatMemoryCtx {
                        project_id: project_id.clone(),
                        cwd: String::new(),
                        canon_cwd: komet_memory::canonical_cwd(cwd),
                        agent: harness.as_str().to_string(),
                        model: None,
                        run_started_ms: 0,
                        pending_tools: HashMap::new(),
                    });
                ctx.project_id = project_id.clone();
                if !cwd.trim().is_empty() {
                    ctx.cwd = cwd.clone();
                    ctx.canon_cwd = komet_memory::canonical_cwd(cwd);
                }
                ctx.agent = harness.as_str().to_string();
                ctx.model = Some(model.clone());
                ctx.run_started_ms = now;
                ctx.pending_tools.clear();
                let (agent, model_out) = (ctx.agent.clone(), ctx.model.clone());
                if project_id.is_empty() {
                    return Vec::new();
                }
                vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id,
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent,
                    model: model_out,
                    kind: "session_start",
                    occurred_at_ms: now,
                    tool_name: None,
                    relative_path: None,
                    outcome: None,
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    reasoning_tokens: None,
                    duration_ms: None,
                }))]
            }
            AgentEvent::Done { status, .. } => {
                let Some(ctx) = self.ctx(chat_id) else {
                    return Vec::new();
                };
                let duration = (ctx.run_started_ms > 0).then(|| now - ctx.run_started_ms);
                let mut items = vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id: ctx.project_id.clone(),
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent: ctx.agent.clone(),
                    model: ctx.model.clone(),
                    kind: "session_end",
                    occurred_at_ms: now,
                    tool_name: None,
                    relative_path: None,
                    outcome: Some(done_status_slug(*status).to_string()),
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    reasoning_tokens: None,
                    duration_ms: duration,
                }))];
                // Run boundary: advance the replay cursor past whatever
                // non-indexed tail this run produced.
                items.push(MemoryItem::Checkpoint {
                    chat_id: chat_id.to_string(),
                    seq,
                });
                items
            }
            AgentEvent::ToolCall { id, call } => {
                let Some(ctx) = self.ctx(chat_id) else {
                    return Vec::new();
                };
                let (tool_name, rel_path) = project_tool_call(call, &ctx.cwd, &ctx.canon_cwd);
                if ctx.pending_tools.len() >= 512 {
                    ctx.pending_tools.clear();
                }
                ctx.pending_tools
                    .insert(id.clone(), (tool_name.clone(), rel_path.clone()));
                vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id: ctx.project_id.clone(),
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent: ctx.agent.clone(),
                    model: ctx.model.clone(),
                    kind: "tool_call",
                    occurred_at_ms: now,
                    tool_name: Some(tool_name),
                    relative_path: rel_path,
                    outcome: None,
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    reasoning_tokens: None,
                    duration_ms: None,
                }))]
            }
            AgentEvent::ToolResult { id, is_error, .. } => {
                let Some(ctx) = self.ctx(chat_id) else {
                    return Vec::new();
                };
                let (tool_name, rel_path) = ctx
                    .pending_tools
                    .remove(id)
                    .unwrap_or_else(|| ("unknown".to_string(), None));
                vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id: ctx.project_id.clone(),
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent: ctx.agent.clone(),
                    model: ctx.model.clone(),
                    kind: "tool_result",
                    occurred_at_ms: now,
                    tool_name: Some(tool_name),
                    relative_path: rel_path,
                    outcome: Some(if *is_error { "error" } else { "ok" }.to_string()),
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    reasoning_tokens: None,
                    duration_ms: None,
                }))]
            }
            AgentEvent::Usage {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_tokens,
                ..
            } => {
                let Some(ctx) = self.ctx(chat_id) else {
                    return Vec::new();
                };
                vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id: ctx.project_id.clone(),
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent: ctx.agent.clone(),
                    model: ctx.model.clone(),
                    kind: "usage",
                    occurred_at_ms: now,
                    tool_name: None,
                    relative_path: None,
                    outcome: None,
                    input_tokens: Some(*input_tokens as i64),
                    cached_input_tokens: Some(*cached_input_tokens as i64),
                    output_tokens: Some(*output_tokens as i64),
                    reasoning_tokens: Some(*reasoning_tokens as i64),
                    duration_ms: None,
                }))]
            }
            AgentEvent::Subagent { event, .. } => self.project(chat_id, seq, event, now),
            _ => Vec::new(),
        }
    }
}

/// Cheap pre-filter so the hot path (text deltas etc.) never touches the
/// projector lock. Mirrors the allow-list in [`MemoryProjector::project`].
fn projectable(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::SessionStarted { .. }
        | AgentEvent::Done { .. }
        | AgentEvent::ToolCall { .. }
        | AgentEvent::ToolResult { .. }
        | AgentEvent::Usage { .. } => true,
        AgentEvent::Subagent { event, .. } => projectable(event),
        _ => false,
    }
}

/// Reduce a [`ToolCall`] to (tool_name, project-relative path?). Command
/// text, patterns, queries, URLs and MCP inputs are never carried over.
/// `canon_cwd` is the chat's cached canonical cwd (see [`ChatMemoryCtx`]).
fn project_tool_call(
    call: &komet_proto::ToolCall,
    cwd: &str,
    canon_cwd: &std::path::Path,
) -> (String, Option<String>) {
    use komet_proto::ToolCall as T;
    let file_path = |p: &str| komet_memory::project_relative(cwd, canon_cwd, p);
    match call {
        T::Exec { .. } => ("exec".into(), None),
        T::ReadFile { path } => ("read_file".into(), file_path(path)),
        T::WriteFile { path, .. } => ("write_file".into(), file_path(path)),
        T::EditFile { path, .. } => ("edit_file".into(), file_path(path)),
        T::ApplyPatch { path } => ("apply_patch".into(), path.as_deref().and_then(file_path)),
        T::Search { .. } => ("search".into(), None),
        T::Glob { .. } => ("glob".into(), None),
        T::WebFetch { .. } => ("web_fetch".into(), None),
        T::WebSearch { .. } => ("web_search".into(), None),
        T::Todo { .. } => ("todo".into(), None),
        T::Mcp { server, tool, .. } => {
            let name = format!("mcp:{server}/{tool}");
            (name.chars().take(128).collect(), None)
        }
        T::Unknown { name, .. } => {
            let name = name.trim();
            let name = if name.is_empty() { "unknown" } else { name };
            (name.chars().take(128).collect(), None)
        }
    }
}

fn done_status_slug(status: komet_proto::DoneStatus) -> &'static str {
    match status {
        komet_proto::DoneStatus::Completed => "completed",
        komet_proto::DoneStatus::Interrupted => "interrupted",
        komet_proto::DoneStatus::Errored => "errored",
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Catch-up throttle: after a queue overflow, replay at most this often.
const CATCHUP_MIN_INTERVAL: Duration = Duration::from_secs(5);
/// Gap-marker tick: how often the indexer checks for persisted enqueue
/// gaps that need a journal replay (writer failures leave these behind).
const GAP_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// Briefing results are cached briefly: a dispatch storm (or a retry loop)
/// must not re-run four aggregate queries per run launch.
const BRIEFING_CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached briefing: (project_id, exclude_chat_id, computed_at, text).
type BriefingCacheEntry = (String, Option<String>, Instant, Option<String>);

/// The engine-facing memory indexer: projection + queue + journal catch-up.
pub struct MemoryIndexer {
    svc: Arc<MemoryService>,
    journal: Arc<RunJournal>,
    projector: Mutex<MemoryProjector>,
    /// chat_id → lowest seq that failed `try_send` (first drop wins).
    lag: Mutex<HashMap<String, u64>>,
    catchup_running: AtomicBool,
    last_catchup: Mutex<Option<Instant>>,
    /// (project_id, exclude_chat_id, computed_at, text) — see
    /// [`Self::briefing`].
    briefing_cache: Mutex<Option<BriefingCacheEntry>>,
}

impl MemoryIndexer {
    /// Open the store under `<data_dir>/memory` and wire journal catch-up.
    pub fn open(
        data_dir: &std::path::Path,
        journal: Arc<RunJournal>,
    ) -> Result<Arc<Self>, komet_memory::MemoryError> {
        let svc = Arc::new(MemoryService::open(data_dir.join("memory"))?);
        let indexer = Arc::new(Self {
            svc,
            journal,
            projector: Mutex::new(MemoryProjector {
                chats: HashMap::new(),
                cwd_projects: HashMap::new(),
            }),
            lag: Mutex::new(HashMap::new()),
            catchup_running: AtomicBool::new(false),
            last_catchup: Mutex::new(None),
            briefing_cache: Mutex::new(None),
        });
        // Periodic gap check: a failed writer batch persists gap markers but
        // nothing re-triggers catch-up until the next overflow or restart.
        // The tick only scans the journal when gaps actually exist.
        let tick = Arc::clone(&indexer);
        std::thread::Builder::new()
            .name("komet-memory-gaptick".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(GAP_TICK_INTERVAL);
                    match tick.svc.chats_with_gaps() {
                        Ok(gaps) if !gaps.is_empty() => tick.catch_up(),
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "memory: gap scan failed")
                        }
                    }
                }
            })
            .ok();
        Ok(indexer)
    }

    pub fn service(&self) -> &Arc<MemoryService> {
        &self.svc
    }

    /// Record the chat's run context at dispatch (cwd + harness known before
    /// `SessionStarted`). Cheap; called once per dispatch.
    pub fn note_chat(&self, chat_id: &str, cwd: &str, harness: HarnessId) {
        lock(&self.projector).note_chat(chat_id, cwd, harness);
    }

    /// Offer one journaled event to the index. Called from `Inner::publish`
    /// AFTER the journal append succeeded; never blocks on SQLite.
    pub fn index_event(self: &Arc<Self>, chat_id: &str, seq: u64, event: &AgentEvent) {
        if seq == 0 || !projectable(event) {
            return;
        }
        let now = now_ms();
        let items = lock(&self.projector).project(chat_id, seq, event, now);
        for item in items {
            let item_seq = match &item {
                MemoryItem::Record(r) => r.source_seq,
                MemoryItem::Checkpoint { seq, .. } | MemoryItem::Resume { seq, .. } => *seq,
            };
            if !self.svc.try_enqueue(item) {
                self.note_drop(chat_id, item_seq);
                return;
            }
        }
    }

    /// A queue item failed `try_send`: remember the lowest dropped seq and
    /// schedule a throttled catch-up. The gap marker is persisted only when
    /// the lag entry is new or moves lower — one tiny write per overflow
    /// episode, never one per dropped event (the writer is exactly what is
    /// busy during an overflow, and `note_gap` would wait on it).
    fn note_drop(self: &Arc<Self>, chat_id: &str, seq: u64) {
        let new_low = {
            let mut lag = lock(&self.lag);
            match lag.get(chat_id) {
                Some(existing) if *existing <= seq => false,
                _ => {
                    lag.insert(chat_id.to_string(), seq);
                    true
                }
            }
        };
        if new_low && let Err(err) = self.svc.note_gap(chat_id, seq) {
            tracing::warn!(chat = %chat_id, error = %err, "memory: gap marker write failed");
        }
        self.maybe_catch_up();
    }

    fn maybe_catch_up(self: &Arc<Self>) {
        let due = {
            let mut last = lock(&self.last_catchup);
            let now = Instant::now();
            let due = last.is_none_or(|t| now.duration_since(t) >= CATCHUP_MIN_INTERVAL);
            if due {
                *last = Some(now);
            }
            due
        };
        if due {
            self.catch_up();
        }
    }

    /// Replay journal tails the queue missed (overflow or restart). Runs on a
    /// dedicated thread — journal parsing is blocking IO and must not sit on
    /// the async executor.
    pub fn catch_up(self: &Arc<Self>) {
        if self.catchup_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("komet-memory-catchup".into())
            .spawn(move || {
                this.catch_up_inner();
                this.catchup_running.store(false, Ordering::SeqCst);
            })
            .ok();
    }

    fn catch_up_inner(&self) {
        let chat_ids = match self.journal.chat_ids() {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(error = %err, "memory: journal chat list failed");
                return;
            }
        };
        for chat_id in &chat_ids {
            self.catch_up_chat(chat_id);
        }
        // Gaps whose journal is gone can never be replayed — clear them or
        // the 60s tick would rescan forever.
        if let Ok(gaps) = self.svc.chats_with_gaps() {
            for chat_id in gaps {
                if !chat_ids.contains(&chat_id) {
                    let _ = self.svc.clear_gap(&chat_id);
                }
            }
        }
    }

    fn catch_up_chat(&self, chat_id: &str) {
        let cursor = match self.svc.chat_cursor(chat_id) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "memory: cursor read failed");
                return;
            }
        };
        let journal_last = match self.journal.last_seq(chat_id) {
            Ok(Some(seq)) => seq,
            _ => return, // no journal or empty
        };
        // Heal from the persisted gap when present, else from the cursor.
        let from = match cursor.gap_seq {
            Some(g) => g.saturating_sub(1),
            None => cursor.last_seq,
        };
        if journal_last <= from {
            // Nothing new (also guards the journal era-fallback: a cursor
            // ahead of the file's tail must not trigger a full re-replay).
            // With a gap marker the journal tail doesn't even reach the
            // dropped seq — the lines are gone, nothing left to heal.
            if cursor.gap_seq.is_some() {
                let _ = self.svc.clear_gap(chat_id);
            }
            return;
        }
        let events = match self.journal.replay(chat_id, from) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "memory: journal replay failed");
                return;
            }
        };
        if events.is_empty() {
            if cursor.gap_seq.is_some() {
                let _ = self.svc.clear_gap(chat_id);
            }
            return;
        }
        let reached = events.last().map(|(seq, _)| *seq).unwrap_or(from);
        // Journal lines have no timestamps: stamp the replay with the
        // file's mtime (≈ the run's end) rather than catch-up time, so an
        // old backfill doesn't look like it happened today.
        let now = self
            .journal
            .journal_mtime_ms(chat_id)
            .unwrap_or_else(now_ms);
        // Project the replay in chunks, releasing the projector lock between
        // them: a long backfill must not starve live `index_event` calls
        // (which share the same mutex) for its whole duration.
        let mut items: Vec<MemoryItem> = Vec::new();
        for chunk in events.chunks(256) {
            {
                let mut projector = lock(&self.projector);
                for (seq, event) in chunk {
                    items.extend(projector.project(chat_id, *seq, event, now));
                }
            }
        }
        // Enqueue everything; on the first failure stop — the remaining tail
        // stays unindexed and the next catch-up resumes from the cursor.
        let mut complete = true;
        for item in items {
            if self.svc.enqueue(item).is_err() {
                complete = false;
                break;
            }
        }
        if complete {
            let _ = self.svc.enqueue(MemoryItem::Resume {
                chat_id: chat_id.to_string(),
                seq: reached,
            });
            // Heal any in-memory lag entries this replay covered.
            let mut lag = lock(&self.lag);
            lag.retain(|_, seq| *seq > reached);
        } else {
            // Queue rejected mid-replay: leave lag markers so the next pass
            // retries this chat.
            let mut lag = lock(&self.lag);
            lag.entry(chat_id.to_string()).or_insert(from + 1);
        }
    }

    // ── queries / settings passthroughs ──────────────────────────────────

    /// Handoff briefing for a run about to start in `cwd`, excluding the
    /// receiving chat's own activity, or `None`. Cached for
    /// [`BRIEFING_CACHE_TTL`] — the digest queries are the most expensive
    /// read in the store and dispatch can be bursty.
    pub fn briefing(
        &self,
        cwd: &str,
        exclude_chat_id: Option<&str>,
    ) -> Result<Option<String>, komet_memory::MemoryError> {
        let project_id = lock(&self.projector).project_id(cwd);
        let exclude = exclude_chat_id.map(str::to_string);
        {
            let cache = lock(&self.briefing_cache);
            if let Some((pid, cid, at, text)) = cache.as_ref()
                && pid == &project_id
                && cid == &exclude
                && at.elapsed() < BRIEFING_CACHE_TTL
            {
                return Ok(text.clone());
            }
        }
        let text = self.svc.briefing(&project_id, exclude_chat_id)?;
        *lock(&self.briefing_cache) = Some((project_id, exclude, Instant::now(), text.clone()));
        Ok(text)
    }

    pub fn set_enabled(
        &self,
        project_id: &str,
        enabled: bool,
    ) -> Result<(), komet_memory::MemoryError> {
        self.svc.set_enabled(project_id, enabled)
    }

    pub fn is_enabled(&self, project_id: &str) -> Result<bool, komet_memory::MemoryError> {
        self.svc.is_enabled(project_id)
    }

    pub fn set_exclusions(
        &self,
        project_id: &str,
        exclusions: &[String],
    ) -> Result<(), komet_memory::MemoryError> {
        self.svc.set_exclusions(project_id, exclusions)
    }

    pub fn search(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<komet_memory::MemoryEvent>, komet_memory::MemoryError> {
        self.svc.search(project_id, query, limit)
    }

    /// "Who touched this path?" — recorded ops under a project-relative
    /// path prefix, newest first.
    pub fn touched(
        &self,
        project_id: &str,
        path_prefix: &str,
        limit: usize,
    ) -> Result<Vec<komet_memory::MemoryEvent>, komet_memory::MemoryError> {
        self.svc.touched(project_id, path_prefix, limit)
    }

    /// Forget one chat's memory rows AND its projector context — chat
    /// deletion must not leak `ChatMemoryCtx` entries (pending tool pairings,
    /// per-chat cwd cache) for the engine's lifetime. The store keeps a
    /// tombstone cursor at the journal tail so catch-up cannot resurrect
    /// the deleted range.
    pub fn forget_chat(&self, chat_id: &str) -> Result<usize, komet_memory::MemoryError> {
        lock(&self.projector).chats.remove(chat_id);
        let through = self.journal.last_seq(chat_id).ok().flatten();
        self.svc.forget_chat(chat_id, through)
    }

    /// Forget a whole project's derived memory. Each known chat gets a
    /// tombstone at its journal tail (same anti-resurrection guarantee as
    /// [`Self::forget_chat`]); projector contexts for the project go too.
    pub fn forget_project(&self, project_id: &str) -> Result<usize, komet_memory::MemoryError> {
        let chats = self.svc.project_chats(project_id)?;
        let mut horizons = Vec::with_capacity(chats.len());
        for chat_id in chats {
            if let Ok(Some(seq)) = self.journal.last_seq(&chat_id) {
                horizons.push((chat_id, seq));
            }
        }
        {
            let mut projector = lock(&self.projector);
            projector
                .chats
                .retain(|_, ctx| ctx.project_id != project_id);
        }
        self.svc.forget_project(project_id, &horizons)
    }

    pub fn stats(
        &self,
        project_id: &str,
    ) -> Result<komet_memory::MemoryStats, komet_memory::MemoryError> {
        self.svc.stats(project_id)
    }
}

/// Wrap a raw briefing in the delimited context block injected at the
/// harness boundary. Kept separate from the user's message: the doc write
/// happens before this is appended to the run request's prompt.
pub fn memory_context_block(briefing: &str) -> String {
    format!("<komet-memory>\n{briefing}\n</komet-memory>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::{DoneStatus, ToolCall as ProtoToolCall};

    fn projector() -> MemoryProjector {
        MemoryProjector {
            chats: HashMap::new(),
            cwd_projects: HashMap::new(),
        }
    }

    #[test]
    fn tool_call_projection_never_carries_payloads() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::ClaudeCode);
        let event = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ProtoToolCall::Exec {
                command: "rm -rf / && echo sk-secret".into(),
            },
        };
        let items = p.project("c1", 7, &event, 1000);
        assert_eq!(items.len(), 1);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.kind, "tool_call");
        assert_eq!(rec.tool_name.as_deref(), Some("exec"));
        assert!(rec.relative_path.is_none());
        // The command string must not appear anywhere in the record.
        let dbg = format!("{rec:?}");
        assert!(!dbg.contains("rm -rf"));
        assert!(!dbg.contains("sk-secret"));
    }

    #[test]
    fn file_tool_paths_are_relativized_and_filtered() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::Codex);
        let event = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ProtoToolCall::WriteFile {
                path: "/repo/src/main.rs".into(),
                content: Some("secret body".into()),
            },
        };
        let items = p.project("c1", 1, &event, 1000);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.relative_path.as_deref(), Some("src/main.rs"));
        assert!(!format!("{rec:?}").contains("secret body"));

        // A path outside the project is dropped entirely.
        let outside = AgentEvent::ToolCall {
            id: "t2".into(),
            call: ProtoToolCall::ReadFile {
                path: "/etc/shadow".into(),
            },
        };
        let items = p.project("c1", 2, &outside, 1001);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert!(rec.relative_path.is_none());

        // Secret-looking paths are dropped by policy.
        let secret = AgentEvent::ToolCall {
            id: "t3".into(),
            call: ProtoToolCall::ReadFile {
                path: "/repo/.env".into(),
            },
        };
        let items = p.project("c1", 3, &secret, 1002);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert!(rec.relative_path.is_none());
    }

    #[test]
    fn tool_result_pairs_with_call_for_outcome() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::ClaudeCode);
        p.project(
            "c1",
            1,
            &AgentEvent::ToolCall {
                id: "t1".into(),
                call: ProtoToolCall::EditFile {
                    path: "/repo/a.rs".into(),
                    old_string: None,
                    new_string: None,
                },
            },
            1000,
        );
        let items = p.project(
            "c1",
            2,
            &AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: true,
                output: Some("boom".into()),
                diff: None,
            },
            1001,
        );
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.kind, "tool_result");
        assert_eq!(rec.outcome.as_deref(), Some("error"));
        assert_eq!(rec.tool_name.as_deref(), Some("edit_file"));
        assert_eq!(rec.relative_path.as_deref(), Some("a.rs"));
        assert!(!format!("{rec:?}").contains("boom"));
    }

    #[test]
    fn session_lifecycle_records_boundaries() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::ClaudeCode);
        let start = AgentEvent::SessionStarted {
            harness: HarnessId::ClaudeCode,
            model: "opus".into(),
            tools: vec![],
            cwd: "/repo".into(),
            session_id: "s1".into(),
            assistant_message_id: "m1".into(),
        };
        let items = p.project("c1", 1, &start, 1000);
        assert!(matches!(&items[0], MemoryItem::Record(r) if r.kind == "session_start"));

        let done = AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            reason: None,
        };
        let items = p.project("c1", 2, &done, 46000);
        assert_eq!(items.len(), 2);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.kind, "session_end");
        assert_eq!(rec.outcome.as_deref(), Some("completed"));
        assert_eq!(rec.duration_ms, Some(45000));
        assert!(matches!(&items[1], MemoryItem::Checkpoint { seq: 2, .. }));
    }

    #[test]
    fn subagent_events_project_through_wrapper() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::ClaudeCode);
        let inner = AgentEvent::ToolCall {
            id: "sub1".into(),
            call: ProtoToolCall::ReadFile {
                path: "/repo/x.rs".into(),
            },
        };
        let wrapped = AgentEvent::Subagent {
            parent_tool_use_id: "parent".into(),
            event: Box::new(inner),
        };
        let items = p.project("c1", 9, &wrapped, 1000);
        assert_eq!(items.len(), 1);
        assert!(projectable(&wrapped));
        assert!(!projectable(&AgentEvent::TextDelta { text: "x".into() }));
    }
}
