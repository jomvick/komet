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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
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
    /// Human label for the project (repo/worktree root or cwd) — recorded
    /// once into `memory_projects.root_path` for the settings UI.
    root: String,
    /// Whether `root` was already pushed to the store this engine run.
    root_recorded: bool,
    agent: String,
    model: Option<String>,
    run_started_ms: i64,
    /// tool_call_id → (tool_name, relative_path) for pairing results.
    /// `tool_name` is `None` when the harness-supplied name was
    /// credential-shaped and got filtered out.
    pending_tools: HashMap<String, (Option<String>, Option<String>)>,
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
        let root = komet_memory::project_root_for(cwd)
            .to_string_lossy()
            .into_owned();
        let ctx = self
            .chats
            .entry(chat_id.to_string())
            .or_insert_with(|| ChatMemoryCtx {
                project_id: project_id.clone(),
                cwd: cwd.to_string(),
                canon_cwd: canon_cwd.clone(),
                root: root.clone(),
                root_recorded: false,
                agent: harness.as_str().to_string(),
                model: None,
                run_started_ms: 0,
                pending_tools: HashMap::new(),
            });
        if ctx.cwd != cwd {
            ctx.cwd = cwd.to_string();
            ctx.project_id = project_id;
            ctx.canon_cwd = canon_cwd;
            ctx.root = root;
            ctx.root_recorded = false;
        }
        ctx.agent = harness.as_str().to_string();
    }

    /// Take the pending project-root label for `chat_id` (once per chat per
    /// engine run) so the indexer can persist it for the settings UI.
    fn take_root_pending(&mut self, chat_id: &str) -> Option<(String, String)> {
        let ctx = self.chats.get_mut(chat_id)?;
        if ctx.root_recorded || ctx.project_id.is_empty() || ctx.root.is_empty() {
            return None;
        }
        ctx.root_recorded = true;
        Some((ctx.project_id.clone(), ctx.root.clone()))
    }

    fn ctx(&mut self, chat_id: &str) -> Option<&mut ChatMemoryCtx> {
        self.chats.get_mut(chat_id)
    }

    /// Chat ids whose live projection context belongs to `project_id` —
    /// covers chats with records still sitting in the writer queue (no
    /// `memory_chat_state` row exists for them yet).
    fn chats_for_project(&self, project_id: &str) -> Vec<String> {
        self.chats
            .iter()
            .filter(|(_, ctx)| ctx.project_id == project_id)
            .map(|(chat_id, _)| chat_id.clone())
            .collect()
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
                        root: String::new(),
                        root_recorded: true,
                        agent: harness.as_str().to_string(),
                        model: None,
                        run_started_ms: 0,
                        pending_tools: HashMap::new(),
                    });
                ctx.project_id = project_id.clone();
                if !cwd.trim().is_empty() {
                    ctx.cwd = cwd.clone();
                    ctx.canon_cwd = komet_memory::canonical_cwd(cwd);
                    ctx.root = komet_memory::project_root_for(cwd)
                        .to_string_lossy()
                        .into_owned();
                    ctx.root_recorded = false;
                }
                ctx.agent = harness.as_str().to_string();
                ctx.model = clean_meta(model);
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
                    tool_name,
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
                    .unwrap_or_else(|| (Some("unknown".to_string()), None));
                vec![MemoryItem::Record(Box::new(MemoryRecord {
                    project_id: ctx.project_id.clone(),
                    chat_id: chat_id.to_string(),
                    source_seq: seq,
                    agent: ctx.agent.clone(),
                    model: ctx.model.clone(),
                    kind: "tool_result",
                    occurred_at_ms: now,
                    tool_name,
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

/// Free-form metadata (model ids, MCP/unknown tool names) is harness-
/// supplied text: it can carry a credential (`mcp:svc/ghp_…`) straight into
/// the store and `search_text`. Drop anything the secret filter flags —
/// `None` reads as "not recorded", which is the honest answer.
fn clean_meta(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || komet_memory::contains_secret_value(value) {
        return None;
    }
    Some(value.chars().take(128).collect())
}

/// Reduce a [`ToolCall`] to (tool_name, project-relative path?). Command
/// text, patterns, queries, URLs and MCP inputs are never carried over.
/// `tool_name` is `None` when the harness-supplied name was credential-
/// shaped. `canon_cwd` is the chat's cached canonical cwd.
fn project_tool_call(
    call: &komet_proto::ToolCall,
    cwd: &str,
    canon_cwd: &std::path::Path,
) -> (Option<String>, Option<String>) {
    use komet_proto::ToolCall as T;
    let file_path = |p: &str| komet_memory::project_relative(cwd, canon_cwd, p);
    match call {
        T::Exec { .. } => (Some("exec".into()), None),
        T::ReadFile { path } => (Some("read_file".into()), file_path(path)),
        T::WriteFile { path, .. } => (Some("write_file".into()), file_path(path)),
        T::EditFile { path, .. } => (Some("edit_file".into()), file_path(path)),
        T::ApplyPatch { path } => (
            Some("apply_patch".into()),
            path.as_deref().and_then(file_path),
        ),
        T::Search { .. } => (Some("search".into()), None),
        T::Glob { .. } => (Some("glob".into()), None),
        T::WebFetch { .. } => (Some("web_fetch".into()), None),
        T::WebSearch { .. } => (Some("web_search".into()), None),
        T::Todo { .. } => (Some("todo".into()), None),
        T::Mcp { server, tool, .. } => (clean_meta(&format!("mcp:{server}/{tool}")), None),
        T::Unknown { name, .. } => {
            let name = name.trim();
            (
                clean_meta(if name.is_empty() { "unknown" } else { name }),
                None,
            )
        }
    }
}

/// Journal seq a queue item was projected from. `None` for the purge
/// barriers (they never come out of [`MemoryProjector::project`] anyway).
fn item_seq(item: &MemoryItem) -> Option<u64> {
    match item {
        MemoryItem::Record(r) => Some(r.source_seq),
        MemoryItem::Checkpoint { seq, .. } | MemoryItem::Resume { seq, .. } => Some(*seq),
        _ => None,
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
/// Retention maintenance cadence (checked on the gap tick).
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

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
    /// A `catch_up` request arrived while a pass was running: the pass
    /// re-triggers itself once instead of dropping the request (the 60s
    /// gap tick would eventually cover it, but a live overflow shouldn't
    /// wait that long).
    catchup_pending: AtomicBool,
    last_catchup: Mutex<Option<Instant>>,
    /// (project_id, exclude_chat_id, computed_at, text) — see
    /// [`Self::briefing`].
    briefing_cache: Mutex<Option<BriefingCacheEntry>>,
    /// Gap markers to persist, sent by the publish path. A channel send
    /// never blocks, unlike a SQLite write that would wait on the very
    /// writer (or an admin transaction) that caused the overflow.
    gap_tx: Sender<(String, u64)>,
    /// Global kill switch (Settings → Memory). OFF stops recording,
    /// briefing injection and catch-up; stored data is kept.
    enabled: AtomicBool,
    /// Raw-event retention in days (settings-configurable).
    retention_days: AtomicU32,
    /// Device-level config (`<data_dir>/memory-config.json`) — mutated by
    /// the settings RPCs and re-saved on every change.
    config: Mutex<komet_memory::MemoryConfig>,
    data_dir: PathBuf,
    /// The directory the store was actually opened at (may differ from
    /// `<data_dir>/memory` when a custom location is configured).
    store_dir: PathBuf,
    /// Set by "Index history": the next catch-up pass also replays journals
    /// the index has never seen (pre-feature history is opt-in, not silent).
    import_untracked: AtomicBool,
}

impl MemoryIndexer {
    /// Open the store and wire journal catch-up. The store location and the
    /// global switches come from `<data_dir>/memory-config.json`; a custom
    /// location is applied here — files are copied BEFORE the first open, so
    /// a bad path falls back to the default dir instead of corrupting data.
    pub fn open(
        data_dir: &std::path::Path,
        journal: Arc<RunJournal>,
    ) -> Result<Arc<Self>, komet_memory::MemoryError> {
        let data_dir = data_dir.to_path_buf();
        let config = komet_memory::MemoryConfig::load(&data_dir);
        let default_dir = data_dir.join("memory");
        let store_dir = config.store_dir(&data_dir);
        if store_dir != default_dir
            && let Err(err) = komet_memory::relocate_store(&default_dir, &store_dir)
        {
            tracing::warn!(
                error = %err,
                from = %default_dir.display(),
                to = %store_dir.display(),
                "memory: store relocation failed; opening destination anyway"
            );
        }
        let svc = Arc::new(match MemoryService::open(&store_dir) {
            Ok(svc) => svc,
            Err(err) if store_dir != default_dir => {
                tracing::warn!(
                    error = %err,
                    dir = %store_dir.display(),
                    "memory: custom location unavailable, falling back to default"
                );
                MemoryService::open(&default_dir)?
            }
            Err(err) => return Err(err),
        });
        let effective_dir = svc.dir().to_path_buf();
        // Gap markers are persisted by a dedicated thread: the publish path
        // only does a non-blocking channel send.
        let (gap_tx, gap_rx) = channel::<(String, u64)>();
        let gap_svc = Arc::clone(&svc);
        std::thread::Builder::new()
            .name("komet-memory-gapwriter".into())
            .spawn(move || {
                for (chat_id, seq) in gap_rx {
                    if let Err(err) = gap_svc.note_gap(&chat_id, seq) {
                        tracing::warn!(
                            chat = %chat_id,
                            error = %err,
                            "memory: gap marker write failed"
                        );
                    }
                }
            })
            .ok();
        let indexer = Arc::new(Self {
            svc,
            journal,
            projector: Mutex::new(MemoryProjector {
                chats: HashMap::new(),
                cwd_projects: HashMap::new(),
            }),
            lag: Mutex::new(HashMap::new()),
            catchup_running: AtomicBool::new(false),
            catchup_pending: AtomicBool::new(false),
            last_catchup: Mutex::new(None),
            briefing_cache: Mutex::new(None),
            gap_tx,
            enabled: AtomicBool::new(config.enabled),
            retention_days: AtomicU32::new(config.retention_days),
            config: Mutex::new(config),
            data_dir,
            store_dir: effective_dir,
            import_untracked: AtomicBool::new(false),
        });
        // Periodic gap check: a failed writer batch persists gap markers but
        // nothing re-triggers catch-up until the next overflow or restart.
        // The tick only scans the journal when gaps actually exist. It holds
        // only a Weak ref: when the engine drops the indexer the thread must
        // exit instead of pinning the store open forever.
        let tick = Arc::downgrade(&indexer);
        std::thread::Builder::new()
            .name("komet-memory-gaptick".into())
            .spawn(move || {
                let mut last_prune: Option<Instant> = None;
                loop {
                    std::thread::sleep(GAP_TICK_INTERVAL);
                    let Some(tick) = tick.upgrade() else {
                        return;
                    };
                    if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_INTERVAL) {
                        last_prune = Some(Instant::now());
                        let retention_ms = i64::from(tick.retention_days.load(Ordering::Relaxed))
                            * 24
                            * 60
                            * 60
                            * 1000;
                        match tick.svc.prune_older_than(now_ms() - retention_ms) {
                            Ok(n) if n > 0 => {
                                tracing::info!(removed = n, "memory: retention prune")
                            }
                            Ok(_) => {}
                            Err(err) => {
                                tracing::warn!(error = %err, "memory: retention prune failed")
                            }
                        }
                    }
                    match tick.svc.chats_with_gaps() {
                        Ok(gaps)
                            if !gaps.is_empty()
                                || tick.import_untracked.load(Ordering::Relaxed) =>
                        {
                            tick.catch_up();
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "memory: gap scan failed")
                        }
                    }
                }
            })
            .ok();
        // Chat deletes that arrived while the store was unavailable (or
        // whose forget failed) left intent in `memory-forget.pending`
        // next to the journals — apply it now that the index exists.
        indexer.drain_pending_forgets();
        Ok(indexer)
    }

    /// Apply forget intents recorded while this indexer was unavailable.
    /// The file lives beside the journals (`{journals}/memory-forget.pending`,
    /// one chat id per line); failures stay in the file for the next start.
    fn drain_pending_forgets(&self) {
        let path = self.journal.dir().join("memory-forget.pending");
        let Ok(content) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut failed = Vec::new();
        for chat_id in content.lines().map(str::trim).filter(|l| !l.is_empty()) {
            match self.forget_chat(chat_id) {
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "memory: pending forget failed");
                    failed.push(chat_id.to_string());
                }
            }
        }
        if failed.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else if let Err(err) = std::fs::write(&path, failed.join("\n") + "\n") {
            tracing::warn!(error = %err, "memory: pending-forget rewrite failed");
        }
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
    /// No-op while the global switch is off (the journal stays authoritative
    /// — re-enabling resumes from the cursor, nothing is lost).
    pub fn index_event(self: &Arc<Self>, chat_id: &str, seq: u64, event: &AgentEvent) {
        if seq == 0 || !self.enabled.load(Ordering::Relaxed) || !projectable(event) {
            return;
        }
        let now = now_ms();
        let (items, root_pending) = {
            let mut projector = lock(&self.projector);
            let items = projector.project(chat_id, seq, event, now);
            (items, projector.take_root_pending(chat_id))
        };
        if let Some((project_id, root)) = root_pending {
            let _ = self.svc.touch_project(&project_id, &root);
        }
        for item in items {
            let Some(item_seq) = item_seq(&item) else {
                continue;
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
        if new_low {
            // Non-blocking: the gap writer thread persists it.
            let _ = self.gap_tx.send((chat_id.to_string(), seq));
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
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if self.catchup_running.swap(true, Ordering::SeqCst) {
            // A pass is already running: flag a rerun instead of dropping
            // the request — drops that land mid-pass would otherwise wait
            // for the 60s gap tick.
            self.catchup_pending.store(true, Ordering::SeqCst);
            return;
        }
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("komet-memory-catchup".into())
            .spawn(move || {
                this.catch_up_inner();
                this.catchup_running.store(false, Ordering::SeqCst);
                if this.catchup_pending.swap(false, Ordering::SeqCst) {
                    this.catch_up();
                }
            })
            .ok();
    }

    fn catch_up_inner(&self) {
        // "Index history" is a one-shot expansion: only this pass replays
        // journals the index has never seen. Everything else — gaps,
        // cursors, tombstones — is always eligible.
        let import_all = self.import_untracked.swap(false, Ordering::SeqCst);
        let chat_ids = match self.journal.chat_ids() {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(error = %err, "memory: journal chat list failed");
                return;
            }
        };
        let known: std::collections::HashSet<String> = if import_all {
            std::collections::HashSet::new()
        } else {
            self.svc
                .known_chats()
                .unwrap_or_default()
                .into_iter()
                .collect()
        };
        for chat_id in &chat_ids {
            if !import_all && !known.contains(chat_id) {
                continue;
            }
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
        // Heal from the lowest known gap — persisted OR still only in memory
        // (the gap writer thread may not have flushed it yet) — else from
        // the cursor.
        let mem_gap = lock(&self.lag).get(chat_id).copied();
        let gap = match (cursor.gap_seq, mem_gap) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let from = match gap {
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
        // Replay the WHOLE journal through a throwaway projector, keeping
        // only items past `from`. Two correctness requirements force the
        // full pass:
        // - Context: the chat's SessionStarted (project, agent, model) and
        //   in-flight ToolCalls usually sit BEFORE the cursor — a tail-only
        //   replay would drop every ToolResult/Done for want of a ctx.
        // - Isolation: replaying stale history through the LIVE projector
        //   would overwrite the running chat's cwd/agent/model and its
        //   pending-tool pairings, mis-attributing subsequent live events.
        let entries = match self.journal.replay_ts(chat_id, 0) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "memory: journal replay failed");
                return;
            }
        };
        if entries.is_empty() {
            if cursor.gap_seq.is_some() {
                let _ = self.svc.clear_gap(chat_id);
            }
            return;
        }
        let reached = entries.last().map(|(seq, _, _)| *seq).unwrap_or(from);
        // Lines written before journal timestamps existed (ts_ms = 0) fall
        // back to the file's mtime (≈ the run's end) rather than catch-up
        // time, so an old backfill doesn't look like it happened today.
        let mtime = self
            .journal
            .journal_mtime_ms(chat_id)
            .unwrap_or_else(now_ms);
        let mut items: Vec<MemoryItem> = Vec::new();
        // A journal last written before the retention horizon would be
        // inserted only to be pruned hours later: skip the projection and
        // still advance the cursor through the Resume below.
        let expired = mtime
            < now_ms()
                - i64::from(self.retention_days.load(Ordering::Relaxed)) * 24 * 60 * 60 * 1000;
        if !expired {
            // Local projector: no lock, no shared state — live index_event
            // calls are completely unaffected by the replay.
            let mut replay = MemoryProjector {
                chats: HashMap::new(),
                cwd_projects: HashMap::new(),
            };
            for (seq, ts, event) in &entries {
                let at = if *ts > 0 { *ts } else { mtime };
                for item in replay.project(chat_id, *seq, event, at) {
                    if item_seq(&item).is_some_and(|s| s > from) {
                        items.push(item);
                    }
                }
            }
            if let Some((project_id, root)) = replay.take_root_pending(chat_id) {
                let _ = self.svc.touch_project(&project_id, &root);
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
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(None);
        }
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

    /// Drop the cached briefing. Called on every settings/forget mutation:
    /// a user who disables memory or forgets a project must not get the old
    /// briefing injected for another 30 seconds.
    fn invalidate_briefing(&self) {
        *lock(&self.briefing_cache) = None;
    }

    pub fn set_enabled(
        &self,
        project_id: &str,
        enabled: bool,
    ) -> Result<(), komet_memory::MemoryError> {
        self.svc.set_enabled(project_id, enabled)?;
        self.invalidate_briefing();
        Ok(())
    }

    pub fn is_enabled(&self, project_id: &str) -> Result<bool, komet_memory::MemoryError> {
        self.svc.is_enabled(project_id)
    }

    pub fn set_exclusions(
        &self,
        project_id: &str,
        exclusions: &[String],
    ) -> Result<(), komet_memory::MemoryError> {
        self.svc.set_exclusions(project_id, exclusions)?;
        self.invalidate_briefing();
        Ok(())
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
        let removed = self.svc.forget_chat(chat_id, through)?;
        self.invalidate_briefing();
        Ok(removed)
    }

    /// Forget a whole project's derived memory. The horizon covers every
    /// chat that could still produce rows for the project: chats the store
    /// knows, chats with live projector context (queued records included),
    /// and — via `u64::MAX` for journal-less chats — chats whose journal is
    /// already gone. Each gets a tombstone so neither catch-up nor queued
    /// writes can resurrect rows; projector contexts go too.
    pub fn forget_project(&self, project_id: &str) -> Result<usize, komet_memory::MemoryError> {
        let mut chats: std::collections::HashSet<String> =
            self.svc.project_chats(project_id)?.into_iter().collect();
        chats.extend(lock(&self.projector).chats_for_project(project_id));
        let mut horizons = Vec::with_capacity(chats.len());
        for chat_id in chats {
            match self.journal.last_seq(&chat_id) {
                Ok(Some(seq)) => horizons.push((chat_id, seq)),
                // Journal gone/empty: the chat cannot produce future events,
                // so the horizon covers every seq it ever had.
                Ok(None) => horizons.push((chat_id, u64::MAX)),
                // Unreadable journal: skip rather than risk a wrong horizon.
                Err(_) => {}
            }
        }
        {
            let mut projector = lock(&self.projector);
            projector
                .chats
                .retain(|_, ctx| ctx.project_id != project_id);
        }
        let removed = self.svc.forget_project(project_id, &horizons)?;
        self.invalidate_briefing();
        Ok(removed)
    }

    pub fn stats(
        &self,
        project_id: &str,
    ) -> Result<komet_memory::MemoryStats, komet_memory::MemoryError> {
        self.svc.stats(project_id)
    }

    // ── settings (Settings → Memory) ─────────────────────────────────────

    /// Journal chats the index has never seen — the "Index history"
    /// button's reason to exist. `0` once everything is tracked.
    pub fn untracked_journal_chats(&self) -> usize {
        let Ok(journals) = self.journal.chat_ids() else {
            return 0;
        };
        let known: std::collections::HashSet<String> = self
            .svc
            .known_chats()
            .unwrap_or_default()
            .into_iter()
            .collect();
        journals.iter().filter(|id| !known.contains(*id)).count()
    }

    /// "Index history": the next catch-up pass replays every journal,
    /// including chats the index never tracked (pre-feature history).
    /// One-shot — the flag is consumed by the pass it triggers.
    pub fn request_history_import(self: &Arc<Self>) {
        self.import_untracked.store(true, Ordering::SeqCst);
        self.catch_up();
    }

    /// Global kill switch. OFF stops recording/briefing/catch-up; stored
    /// data is kept. Persists to `memory-config.json` immediately.
    pub fn set_global_enabled(&self, enabled: bool) -> Result<(), komet_memory::MemoryError> {
        self.enabled.store(enabled, Ordering::Relaxed);
        {
            let mut config = lock(&self.config);
            config.enabled = enabled;
            config.save(&self.data_dir).map_err(|e| {
                komet_memory::MemoryError::Other(format!("save memory config: {e}"))
            })?;
        }
        self.invalidate_briefing();
        Ok(())
    }

    pub fn global_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Retention in days (clamped to the config bounds). Applies to the
    /// next prune tick and to replay-expiry checks.
    pub fn set_retention_days(&self, days: u32) -> Result<(), komet_memory::MemoryError> {
        let days = days.clamp(
            komet_memory::RETENTION_DAYS_MIN,
            komet_memory::RETENTION_DAYS_MAX,
        );
        self.retention_days.store(days, Ordering::Relaxed);
        self.invalidate_briefing();
        {
            let mut config = lock(&self.config);
            config.retention_days = days;
            config.save(&self.data_dir).map_err(|e| {
                komet_memory::MemoryError::Other(format!("save memory config: {e}"))
            })?;
        }
        Ok(())
    }

    pub fn retention_days(&self) -> u32 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// Point the store at a new location. Persisted to the config file and
    /// applied at the NEXT engine start (the running store is never moved
    /// under live connections). `None` resets to `<data_dir>/memory`.
    pub fn set_location(&self, location: Option<String>) -> Result<(), komet_memory::MemoryError> {
        {
            let mut config = lock(&self.config);
            config.location = location;
            config.save(&self.data_dir).map_err(|e| {
                komet_memory::MemoryError::Other(format!("save memory config: {e}"))
            })?;
        }
        Ok(())
    }

    /// The directory the store is open at right now.
    pub fn store_dir(&self) -> &std::path::Path {
        &self.store_dir
    }

    /// The configured custom location, if any — may differ from
    /// [`Self::store_dir`] until the next restart applies it.
    pub fn pending_location(&self) -> Option<String> {
        lock(&self.config).location.clone()
    }

    /// All known projects with settings + footprint (settings page list).
    pub fn list_projects(
        &self,
    ) -> Result<Vec<komet_memory::ProjectInfo>, komet_memory::MemoryError> {
        self.svc.list_projects()
    }

    /// Store-wide event/chat counts (per-project `stats` exists too).
    pub fn totals(&self) -> Result<(i64, i64), komet_memory::MemoryError> {
        self.svc.totals()
    }

    /// "Forget all": wipe every project's derived rows AND tombstone every
    /// chat that could still produce rows — journal-backed chats at their
    /// tail, journal-less chats (queued records, gap markers, cursor rows)
    /// at `u64::MAX`. Live chats keep recording: their NEXT events carry
    /// seqs above the horizon. Project settings are kept.
    pub fn forget_all(&self) -> Result<usize, komet_memory::MemoryError> {
        let mut chats: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Ok(journals) = self.journal.chat_ids() {
            chats.extend(journals);
        }
        if let Ok(known) = self.svc.known_chats() {
            chats.extend(known);
        }
        chats.extend(lock(&self.lag).keys().cloned());
        chats.extend(lock(&self.projector).chats.keys().cloned());
        let mut horizons = Vec::with_capacity(chats.len());
        for chat_id in chats {
            match self.journal.last_seq(&chat_id) {
                Ok(Some(seq)) => horizons.push((chat_id, seq)),
                // Journal confirmed gone/empty → no future events → cover
                // every seq it ever had. An unreadable journal is skipped
                // rather than risking a wrong horizon on a live chat.
                Ok(None) => horizons.push((chat_id, u64::MAX)),
                Err(_) => {}
            }
        }
        {
            let mut projector = lock(&self.projector);
            projector.chats.clear();
            projector.cwd_projects.clear();
        }
        let removed = self.svc.forget_all(&horizons)?;
        self.invalidate_briefing();
        Ok(removed)
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
    fn credential_shaped_names_and_models_are_dropped() {
        let mut p = projector();
        p.note_chat("c1", "/repo", HarnessId::ClaudeCode);
        let token = "ghp_aaaaaaaaaaaaaaaaaaaa";
        // Model id carrying a token must not persist.
        let start = AgentEvent::SessionStarted {
            harness: HarnessId::ClaudeCode,
            model: token.into(),
            tools: vec![],
            cwd: "/repo".into(),
            session_id: "s1".into(),
            assistant_message_id: "m1".into(),
        };
        let items = p.project("c1", 1, &start, 1000);
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.model, None);
        assert!(!format!("{rec:?}").contains(token));

        // MCP server/tool names are free-form: a token inside must not
        // reach tool_name (and therefore search_text).
        for (seq, call) in [
            (
                2u64,
                ProtoToolCall::Mcp {
                    server: "svc".into(),
                    tool: token.into(),
                    input: None,
                },
            ),
            (
                3,
                ProtoToolCall::Unknown {
                    name: format!("prefix_{token}"),
                    input: None,
                },
            ),
        ] {
            let items = p.project(
                "c1",
                seq,
                &AgentEvent::ToolCall {
                    id: format!("t{seq}"),
                    call,
                },
                1000,
            );
            let MemoryItem::Record(rec) = &items[0] else {
                panic!("expected record");
            };
            assert_eq!(rec.tool_name, None, "seq {seq} leaked a credential");
            assert!(!format!("{rec:?}").contains(token));
        }

        // Safe free-form names still record.
        let items = p.project(
            "c1",
            4,
            &AgentEvent::ToolCall {
                id: "t4".into(),
                call: ProtoToolCall::Mcp {
                    server: "github".into(),
                    tool: "create_issue".into(),
                    input: None,
                },
            },
            1001,
        );
        let MemoryItem::Record(rec) = &items[0] else {
            panic!("expected record");
        };
        assert_eq!(rec.tool_name.as_deref(), Some("mcp:github/create_issue"));
    }

    /// The regression CodeRabbit flagged: a cursor ahead of `SessionStarted`
    /// must not lose the tool pairing — catch-up rebuilds ctx from seq 0
    /// through a dedicated replay projector.
    #[test]
    fn catch_up_rebuilds_context_from_full_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal =
            Arc::new(RunJournal::open(dir.path().join("journals")).expect("journal open"));
        let indexer = MemoryIndexer::open(dir.path(), journal.clone()).expect("indexer open");
        let cwd = "/repo";
        journal
            .append(
                "c1",
                &AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: "opus".into(),
                    tools: vec![],
                    cwd: cwd.into(),
                    session_id: "s1".into(),
                    assistant_message_id: "m1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                "c1",
                &AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ProtoToolCall::EditFile {
                        path: "/repo/a.rs".into(),
                        old_string: None,
                        new_string: None,
                    },
                },
            )
            .unwrap();
        journal
            .append(
                "c1",
                &AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false,
                    output: None,
                    diff: None,
                },
            )
            .unwrap();
        journal
            .append(
                "c1",
                &AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                    reason: None,
                },
            )
            .unwrap();

        // Pretend seqs 1-2 were already processed (cursor=2) — the live
        // projector knows nothing about this chat, exactly like a restart.
        indexer
            .service()
            .enqueue(MemoryItem::Checkpoint {
                chat_id: "c1".into(),
                seq: 2,
            })
            .unwrap();
        wait_for(|| indexer.service().chat_cursor("c1").unwrap().last_seq == 2);

        indexer.catch_up_inner();
        let project_id = komet_memory::project_id_for(cwd);
        wait_for(|| {
            indexer
                .service()
                .recent(&project_id, 50, None)
                .map(|e| e.iter().any(|ev| ev.kind == "session_end"))
                .unwrap_or(false)
        });
        let events = indexer.service().recent(&project_id, 50, None).unwrap();
        // seqs 3-4 only: the tool result must pair with the ToolCall seen
        // during the context rebuild, and Done must emit session_end.
        assert!(
            events.iter().all(|e| e.kind != "session_start"),
            "pre-cursor events must not be re-indexed: {events:?}"
        );
        let result = events
            .iter()
            .find(|e| e.kind == "tool_result")
            .expect("tool_result missing");
        assert_eq!(result.tool_name.as_deref(), Some("edit_file"));
        assert_eq!(result.relative_path.as_deref(), Some("a.rs"));
        assert!(events.iter().any(|e| e.kind == "session_end"));
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for memory writer");
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
