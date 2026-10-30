//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Pragmatic port of komet's `sessions.ts` (spec: feature-inventory §3.2):
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer
//!   — except the broadcast-only frames ([`AgentEvent::ToolProgress`], a tool's
//!   live output tail, and [`AgentEvent::RunPhase`], the working phase the
//!   trailer names): neither has durable meaning, so neither is ever journaled
//!   and neither is ever folded;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). Komet's pulse
//! loop is ported as the 15s liveness heartbeat in `drive_run`; its stall watchdog is
//! deliberately NOT ported (rejected in review — agents may legitimately wait on
//! something for far longer than any timeout, and a live child IS the working signal).
//! Every dying path must instead carry its own visible error (child crash with stderr,
//! spawn failure, stream error, engine-restart recovery).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use komet_doc::{
    CompactionSummaryPolicy, DocError, InputResponse, MessagePart, MessageRole, MessageStatus,
    ProviderSessionMark, STREAM_COMMIT_MS, SegmentWriter, SessionDoc, SubagentStatus,
    fold_event_into_parts_with, sanitize_tool_call,
};
use komet_harness::{CancellationToken, Harness, RunControls, SteerMessage};
use komet_proto::{
    AgentEvent, CompactionStatus, ContextUsageSource, DoneReason, DoneStatus, ErrorKind, HarnessId,
    RunPhase, RunRequest, Session, SessionStatus, UserInputAnswer, UserInputQuestion,
};

use crate::agent_accounts::AgentAccounts;
use crate::composio::ComposioApi;
use crate::doc_host::{ChatDocHandle, DocHost};
use crate::mcp::catalog::McpCatalog;
use crate::mcp::host::EngineMcpHost;
use crate::mcp::policy::McpPolicy;
use crate::mcp::server::{AskDecision, AskFn, McpEndpoint, PermissionAsk, RunningEndpoint};
use crate::mcp::{McpRegistry, McpSecretStore, ResolvedMcpServer};
use crate::registry::HarnessRegistry;
use crate::run_journal::RunJournal;
use crate::{EngineError, new_id, now_ms};

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the live run's steering mailbox.
    Accepted,
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

/// Coalescing window for ephemeral tool output: at most one broadcast per
/// window per chat, however many chunks the harness streamed inside it.
const TOOL_PROGRESS_FLUSH: std::time::Duration = std::time::Duration::from_millis(100);

/// Tail retained per tool id: the open chip shows the END of a stream, and an
/// unbounded buffer would grow with `cat` of a huge file.
const TOOL_PROGRESS_CAP: usize = 4096;

/// Frames that exist only while a subscriber is looking: a tool's live output
/// tail, and the run's working phase. They are broadcast, never journaled and
/// never folded — the durable facts around them (`ToolCall`, the settled
/// `ToolResult`, the doc's own `Working` row status) say everything the
/// transcript needs. Descends `Subagent` wrappers because a child prints the
/// same kind of frame, and a long subagent run would otherwise fill the journal
/// with text nobody will ever replay.
fn is_ephemeral(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::ToolProgress { .. } | AgentEvent::RunPhase { .. } => true,
        AgentEvent::Subagent { event, .. } => is_ephemeral(event),
        _ => false,
    }
}

/// The phase the live run is in after this harness event, `Some` only when the
/// event CHANGES it — so a streamed turn broadcasts one frame, not one per
/// delta. `None` on `Done`: the row settles, so there is no wait left to name.
/// A harness that never signals stays on the phase it was dispatched with.
fn phase_change(event: &AgentEvent, current: Option<RunPhase>) -> Option<RunPhase> {
    let next = match event {
        AgentEvent::SessionStarted { .. } => RunPhase::WaitingModel,
        AgentEvent::TextDelta { text } | AgentEvent::ReasoningDelta { text }
            if !text.is_empty() =>
        {
            RunPhase::Streaming
        }
        AgentEvent::ToolCall { .. } => RunPhase::Tool,
        _ => return None,
    };
    (current != Some(next)).then_some(next)
}

/// Live counters of one subagent feed, held in the host's memory for the length
/// of the run. Neither the doc nor the journal carries them: the spawn chip
/// reads them ONCE at the child's terminal event, and `WatchActivity` snapshots
/// them per frame — so "3 tools · 12 s" is visible while the child runs without
/// the parent doc being rewritten for every one of those tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentFeed {
    /// Tools this feed ran — its own `ToolCall`s, never a nested child's.
    pub tool_count: u64,
    /// Host clock at the feed's first frame. A cycle's length is the difference
    /// of two readings of this same clock, so a viewer never has to trust one.
    pub started_at_ms: i64,
    /// Lifecycle the engine last read off this feed's own wrappers.
    pub status: SubagentStatus,
    /// Chip label of the last tool this feed started ("Read", "Run", …) — the
    /// spawn chip's one-line answer to "what is the child doing now". The LABEL
    /// only, never its arguments: a command line can carry a secret, and the
    /// arguments a viewer wants are one click away, in the transcript tab.
    pub last_tool: Option<&'static str>,
}

/// What one hub event did to a feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedHit {
    /// The feed started a tool; the payload is its chip label.
    Tool(&'static str),
    Status(SubagentStatus),
    /// The parent's spawn chip: opens the feed once, never restarts it.
    Opened,
}

/// The feeds one hub event touched, keyed by the parent they belong to. Same
/// attribution rule as [`crate::run_journal::project_feed`]: an event counts for
/// the feed its DIRECT wrapper names, so a sub-subagent's tools land on the
/// child that ran them and never inflate the root's chip.
pub(crate) fn touched_feeds(event: &AgentEvent) -> Vec<(String, FeedHit)> {
    let mut hits = Vec::new();
    if let AgentEvent::ToolCall { call, id, .. } = event
        && call.is_subagent_spawn()
    {
        // The spawn itself opens the feed: from here the chip is live, and the
        // clock starts before the child's first frame arrives. A re-emitted
        // spawn (title/rawInput refresh) must not reset a settled feed, so this
        // is an open-only hit, not a `Running` transition.
        hits.push((id.clone(), FeedHit::Opened));
    }
    if let AgentEvent::Subagent {
        parent_tool_use_id,
        event,
    } = event
    {
        let hit = match &**event {
            AgentEvent::ToolCall { call, .. } => {
                Some(FeedHit::Tool(komet_proto::view::tool_chip_content(call).0))
            }
            AgentEvent::Done { status, .. } => Some(FeedHit::Status(match status {
                DoneStatus::Errored => SubagentStatus::Failed,
                _ => SubagentStatus::Done,
            })),
            // A steer resurrects: the child works again, and the numbers the
            // chip will carry are the new cycle's, not the settled one's.
            AgentEvent::UserMessage { .. } => Some(FeedHit::Status(SubagentStatus::Running)),
            _ => None,
        };
        if let Some(hit) = hit {
            hits.push((parent_tool_use_id.clone(), hit));
        }
        hits.extend(touched_feeds(event));
    }
    hits
}

/// Read-only handle on the host's live counters, so the RPC layer can snapshot a
/// feed while streaming it without reaching into engine state.
#[derive(Clone, Default)]
pub struct SubagentFeeds {
    by_chat: Arc<Mutex<HashMap<String, HashMap<String, SubagentFeed>>>>,
}

impl SubagentFeeds {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, HashMap<String, SubagentFeed>>> {
        self.by_chat.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply the feeds an event touched. A feed unknown to this run opens at
    /// `seen_at` — a harness that streams child frames without announcing the
    /// spawn still gets counted, from the first frame it sent.
    pub(crate) fn record(&self, chat_id: &str, hits: &[(String, FeedHit)], seen_at: i64) {
        if hits.is_empty() {
            return;
        }
        let mut by_chat = self.lock();
        let feeds = by_chat.entry(chat_id.to_owned()).or_default();
        for (parent, hit) in hits {
            let feed = feeds.entry(parent.clone()).or_insert(SubagentFeed {
                tool_count: 0,
                started_at_ms: seen_at,
                status: SubagentStatus::Running,
                last_tool: None,
            });
            match *hit {
                // `or_insert` above already opened an absent feed at
                // `seen_at`; an existing one keeps its counters and status.
                FeedHit::Opened => {}
                FeedHit::Tool(name) => {
                    feed.tool_count += 1;
                    feed.last_tool = Some(name);
                }
                FeedHit::Status(status) => {
                    if status == SubagentStatus::Running && feed.status != SubagentStatus::Running {
                        feed.tool_count = 0;
                        feed.started_at_ms = seen_at;
                        // The new cycle has not run a tool yet; the settled
                        // cycle's last one is not an answer to "what now".
                        feed.last_tool = None;
                    }
                    feed.status = status;
                }
            }
        }
    }

    /// The counters of one live feed, as this host measured them. `None` is "no
    /// run here counted this feed" — an old chat, another device, or a feed that
    /// settled and was retired with its run — which the activity frame reports
    /// by simply carrying no numbers rather than zeros.
    pub fn get(&self, chat_id: &str, parent: &str) -> Option<SubagentFeed> {
        self.lock().get(chat_id)?.get(parent).copied()
    }

    /// Retire a run's counters: they describe a turn that is over, and a doc
    /// chip already carries whatever survived of them.
    fn clear(&self, chat_id: &str) {
        self.lock().remove(chat_id);
    }
}

/// The chip's single terminal write: the host stamps the child's measured
/// numbers on its spawn chip when the child settles, and nothing else in the
/// run touches the doc. An absent chip (a spawn this fold never saw) is left
/// absent — the numbers have nothing to describe.
fn stamp_subagent_terminal(
    folded: &mut [MessagePart],
    parent: &str,
    feed: SubagentFeed,
    settled_at: i64,
) {
    for part in folded.iter_mut() {
        if let MessagePart::Tool {
            id,
            call,
            subagent_duration_ms,
            subagent_tool_count,
            ..
        } = part
            && id == parent
            && call.is_subagent_spawn()
        {
            *subagent_tool_count = Some(feed.tool_count);
            *subagent_duration_ms = Some(settled_at.saturating_sub(feed.started_at_ms) as u64);
        }
    }
}

/// Per-tool ephemeral output buffer for one run ([`AgentEvent::ToolProgress`]).
/// Entries keep their arrival order so a coalesced flush broadcasts the tools
/// in the sequence the user would have seen them print.
#[derive(Default)]
struct ToolProgressBuffer {
    order: Vec<String>,
    pending: HashMap<String, String>,
}

impl ToolProgressBuffer {
    /// Fold one chunk in. `append` extends the stream (a delta wire); anything
    /// else is the content so far and replaces it.
    fn push(&mut self, id: &str, text: &str, append: bool) {
        if !self.pending.contains_key(id) {
            self.order.push(id.to_owned());
        }
        let tail = self.pending.entry(id.to_owned()).or_default();
        if append {
            tail.push_str(text);
        } else {
            *tail = text.to_owned();
        }
        trim_tail(tail, TOOL_PROGRESS_CAP);
    }

    /// Drain every buffered tail, in arrival order.
    fn take(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(self.order.len());
        for id in std::mem::take(&mut self.order) {
            if let Some(text) = self.pending.remove(&id) {
                out.push((id, text));
            }
        }
        out
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Drop the front of `s` until it fits in `cap` bytes, on a char boundary.
fn trim_tail(s: &mut String, cap: usize) {
    if s.len() <= cap {
        return;
    }
    let mut cut = s.len() - cap;
    while !s.is_char_boundary(cut) {
        cut += 1;
    }
    s.drain(..cut);
}

/// A question set the harness is parked on. `secret_ids` names the questions
/// whose answer must never leave the host's memory (doc, journal, broadcast);
/// the harness itself still receives the real answers through `resolver`.
struct PendingInput {
    resolver: oneshot::Sender<Vec<UserInputAnswer>>,
    secret_ids: Vec<String>,
}

type PendingInputs = Arc<Mutex<HashMap<String, PendingInput>>>;

/// The durable/broadcast copy of an answer set: a secret field's content is
/// replaced with the withheld marker, never carrying what the user typed.
fn redact_secret_answers(
    answers: Vec<UserInputAnswer>,
    secret_ids: &[String],
) -> Vec<UserInputAnswer> {
    if secret_ids.is_empty() {
        return answers;
    }
    answers
        .into_iter()
        .map(|mut answer| {
            let secret = secret_ids.iter().any(|id| id == &answer.question_id);
            let answered = !answer.labels.is_empty() || answer.free_text.is_some();
            if secret && answered {
                answer.labels.clear();
                answer.free_text = Some(komet_proto::WITHHELD_SECRET_ANSWER.to_owned());
            }
            answer
        })
        .collect()
}

type PendingPermissionChoices =
    Arc<Mutex<HashMap<String, oneshot::Sender<komet_proto::PermissionChoice>>>>;

struct McpPermissionCancellation {
    pending: PendingPermissionChoices,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    sessions: SessionsEngine,
    chat_id: String,
    request_id: String,
}

impl Drop for McpPermissionCancellation {
    fn drop(&mut self) {
        let Some(resolver) = lock(&self.pending).remove(&self.request_id) else {
            return;
        };
        drop(resolver);
        let choice = komet_proto::PermissionChoice::Deny;
        let _ = self.engine_tx.send(AgentEvent::PermissionResolved {
            request_id: self.request_id.clone(),
            choice,
        });
        match self.sessions.doc_handle(&self.chat_id) {
            Ok(handle) => {
                if let Err(error) = handle.doc().resolve_permission(&self.request_id) {
                    tracing::warn!(
                        chat = %self.chat_id,
                        request = %self.request_id,
                        error = %error,
                        "failed to resolve cancelled MCP permission"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    chat = %self.chat_id,
                    request = %self.request_id,
                    error = %error,
                    "failed to find document for cancelled MCP permission"
                );
            }
        }
    }
}

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped (claude keys conversations by project
/// directory — komet sessions.ts:563 "harness session stores are keyed by
/// cwd"), so resume is only injected for runs launched from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    harness: HarnessId,
    session_id: String,
    cwd: String,
}

struct RunHandle {
    run_id: String,
    steerable: bool,
    steer_tx: mpsc::Sender<SteerMessage>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<bool>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
    pending_permissions: PendingPermissionChoices,
    /// Steers accepted into the mailbox but not yet confirmed by a `Steered`
    /// event — the at-least-once ledger. A run can die with accepted steers
    /// still in its mailbox (idle reaper vs. a routed send; a mid-turn error
    /// discarding queued boundary steers): the run task drains this at exit
    /// and re-dispatches each entry as a fresh turn, so an accepted message
    /// can never silently evaporate from a transcript that shows it as sent.
    routed_steers: Arc<Mutex<std::collections::VecDeque<RoutedSteer>>>,
}

/// One accepted-but-unconfirmed steer: enough to re-dispatch it verbatim.
#[derive(Debug, Clone)]
struct RoutedSteer {
    prompt: String,
    message_id: String,
}

struct Inner {
    device_id: String,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    mcp_registry: Arc<McpRegistry>,
    mcp_secrets: Arc<McpSecretStore>,
    agent_accounts: OnceLock<AgentAccounts>,
    /// Set-once (first wins), cleared on runtime retirement: sessions and
    /// doc-host reference each other through Arcs, so this back-edge must be
    /// severable for a replaced engine graph to drop.
    doc_host: Mutex<Option<DocHost>>,
    /// chat_id → live run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    statuses: Mutex<HashMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (komet kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// Latest context-window snapshot per chat (replaced, not summed).
    usage_stats: Mutex<HashMap<String, komet_proto::ContextUsageStats>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    /// Fired with `(chat_id, cwd)` when a user prompt starts a turn (fresh
    /// dispatch or accepted steer) — the diff sync snapshots the checkout tree
    /// for the Changes pane's "Latest turn" scope. Absent in bare tests.
    turn_listener: OnceLock<TurnListener>,
    /// D4 — permission-bridge hang guard: a runtime permission request left
    /// unanswered this long is auto-DENIED (Deny also interrupts the run) and
    /// its doc part resolved. `None` disables the guard entirely.
    permission_timeout: Mutex<Option<std::time::Duration>>,
    /// Opt-in: copy a compaction summary's TEXT into the synchronized doc
    /// instead of leaving it in the host's run journal only. Off by default —
    /// the summary is model prose about the whole conversation, so writing it
    /// into the oplog replicates it to every device forever, and a recap that
    /// quotes what the user asked to have compacted travels further than the
    /// transcript it came from.
    compaction_summary_in_doc: AtomicBool,
    /// Live per-subagent counters (see [`SubagentFeeds`]) — the host's only
    /// record of a child's tool count and elapsed time while it runs.
    subagent_feeds: SubagentFeeds,
}

/// Turn-start hook: called with `(chat_id, cwd)`.
pub type TurnListener = Arc<dyn Fn(&str, &str) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
        mcp_registry: Arc<McpRegistry>,
        mcp_secrets: Arc<McpSecretStore>,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                device_id,
                journal,
                registry,
                mcp_registry,
                mcp_secrets,
                agent_accounts: OnceLock::new(),
                doc_host: Mutex::new(None),
                runs: Mutex::new(HashMap::new()),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                sessions_tx,
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                usage_stats: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                turn_listener: OnceLock::new(),
                permission_timeout: Mutex::new(Some(std::time::Duration::from_secs(10 * 60))),
                compaction_summary_in_doc: AtomicBool::new(matches!(
                    std::env::var("KOMET_COMPACTION_SUMMARY_IN_DOC")
                        .ok()
                        .as_deref()
                        .unwrap_or("0")
                        .trim(),
                    "1" | "true" | "yes" | "on"
                )),
                subagent_feeds: SubagentFeeds::default(),
            }),
        }
    }

    /// Whether compaction summary text is written into the doc (see the
    /// `compaction_summary_in_doc` field). Device-local, like the permission
    /// guard above: it decides what THIS host puts in its own doc.
    pub fn set_compaction_summary_in_doc(&self, enabled: bool) {
        self.inner
            .compaction_summary_in_doc
            .store(enabled, Ordering::Relaxed);
    }

    pub fn set_agent_accounts(&self, accounts: AgentAccounts) {
        if self.inner.agent_accounts.set(accounts).is_err() {
            tracing::warn!("agent accounts service was already wired to sessions");
        }
    }

    /// Test-only constructor with ephemeral MCP stores.
    pub fn new_for_tests(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
    ) -> Self {
        Self::new(
            device_id,
            journal,
            registry,
            Arc::new(McpRegistry::empty(std::env::temp_dir())),
            Arc::new(McpSecretStore::ephemeral()),
        )
    }

    /// D4 — tune (or disable with `None`) the permission-bridge hang guard.
    /// Already-pending requests keep the timeout they were created with.
    pub fn set_permission_timeout(&self, timeout: Option<std::time::Duration>) {
        *lock(&self.inner.permission_timeout) = timeout;
    }

    /// Retrieve context & token usage metrics for a chat session.
    pub fn usage_for(&self, chat_id: &str) -> Option<komet_proto::ContextUsageStats> {
        lock(&self.inner.usage_stats).get(chat_id).cloned()
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        // First set wins (the OnceLock contract this slot replaced).
        let mut slot = lock(&self.inner.doc_host);
        if slot.is_none() {
            *slot = Some(host);
        }
    }

    /// Sever the doc-host back-edge (runtime retirement; the doc host's
    /// `shutdown_workers` clears its own sessions edge). Every access site
    /// already treats a missing doc host as "not wired".
    pub fn clear_doc_host(&self) {
        lock(&self.inner.doc_host).take();
    }

    /// Wire the chat auto-titler (called once at engine assembly). After each
    /// completed exchange the run task fires it for still-untitled chats.
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    /// Wire the turn-start listener (called once at engine assembly).
    pub fn set_turn_listener(&self, listener: TurnListener) {
        let _ = self.inner.turn_listener.set(listener);
    }

    fn note_turn_start(&self, chat_id: &str, cwd: &str) {
        if let Some(listener) = self.inner.turn_listener.get() {
            listener(chat_id, cwd);
        }
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        let host = self
            .inner
            .doc_host()
            .ok_or_else(|| EngineError::Other("doc host not wired into sessions engine".into()))?;
        host.open(chat_id)
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(|s| {
            matches!(
                s.status,
                komet_proto::SessionStatus::Working | komet_proto::SessionStatus::AwaitingInput
            )
        })
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Explicitly tombstone the harness session for a chat (e.g. on prompt edit).
    pub fn tombstone_harness_session(&self, chat_id: &str) {
        self.inner.invalidate_harness_session(chat_id);
    }

    /// IDs assigned to the chat for MCP servers (`ChatConfig.mcp_server_ids`).
    fn mcp_ids_for_chat(&self, chat_id: &str) -> Vec<String> {
        if let Some(host) = self.inner.doc_host()
            && let Some(ws) = host.workspace()
            && let Some(cfg) = ws.chat_config(chat_id)
        {
            return cfg.mcp_server_ids;
        }
        Vec::new()
    }

    /// Resolve MCP servers assigned to a chat (`ChatConfig.mcp_server_ids` → `ResolvedMcpServer`).
    /// Filters disabled / invalid entries and injects secrets from `McpSecretStore`.
    /// Never persists secrets; resolved values are in-memory only for the harness launch.
    /// Public/UI view should use `PublicMcpServerConfig`.
    pub fn prepare_mcp_for_run(&self, chat_id: &str) -> Vec<ResolvedMcpServer> {
        // Empty ids still resolve `always_load` servers — those are a global
        // assignment, not a per-chat opt-in.
        let ids = self.mcp_ids_for_chat(chat_id);
        self.inner
            .mcp_registry
            .resolve(&ids, &self.inner.mcp_secrets)
    }

    /// Build the `harness_mcp_servers` vector for the next harness launch.
    /// Kept separate from `RunRequest.mcp` (internal Komet MCP) — Task 4/5 wires this into
    /// `acp`/`claude`/`codex` via `into_harness_config`.
    pub fn harness_mcp_servers_for_run(&self, chat_id: &str) -> Vec<crate::mcp::McpServerConfig> {
        self.prepare_mcp_for_run(chat_id)
            .into_iter()
            .map(|r| r.into_harness_config())
            .collect()
    }

    /// Per-run localhost Komet MCP. Bind failure is visible then the run
    /// continues without internal tools — the agent's own MCP config is
    /// unaffected.
    async fn start_internal_mcp(
        &self,
        chat_id: &str,
        run_id: &str,
        ask: AskFn,
    ) -> Option<RunningEndpoint> {
        let Some(workspace) = self.inner.workspace() else {
            tracing::warn!(chat = %chat_id, "internal MCP skipped: workspace not wired");
            return None;
        };
        let mut catalog =
            McpCatalog::komet_default(Arc::new(EngineMcpHost::new(self.clone(), workspace)));
        match self.inner.mcp_secrets.composio_platform_api_key() {
            Ok(Some(api_key)) => {
                let api = ComposioApi::new(api_key, self.inner.device_id.clone());
                let session = match api {
                    Ok(api) => {
                        match tokio::time::timeout(std::time::Duration::from_secs(10), async {
                            let session = api.create_session().await?;
                            let tools = session.tools().await?;
                            Ok::<_, anyhow::Error>((session, tools))
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(anyhow::anyhow!("Composio startup timed out")),
                        }
                    }
                    Err(error) => Err(error),
                };
                match session {
                    Ok((session, tools)) => {
                        catalog = catalog.with_composio(session, tools);
                    }
                    Err(error) => {
                        let message = format!("Composio tools unavailable: {error}");
                        tracing::warn!(chat = %chat_id, error = %error, "Composio session unavailable");
                        self.inner.publish(
                            chat_id,
                            &AgentEvent::Error {
                                message,
                                kind: ErrorKind::Unknown,
                                detail: None,
                                retryable: true,
                            },
                        );
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(chat = %chat_id, error = %error, "Composio credential unavailable");
            }
        }
        match McpEndpoint::start(run_id, Arc::new(catalog), McpPolicy::komet_default(), ask).await {
            Ok(endpoint) => Some(endpoint),
            Err(err) => {
                let message = format!("Komet MCP unavailable: {err}");
                tracing::warn!(chat = %chat_id, error = %err, "internal MCP endpoint failed");
                self.inner.publish(
                    chat_id,
                    &AgentEvent::Error {
                        message,
                        kind: ErrorKind::Unknown,
                        detail: None,
                        retryable: false,
                    },
                );
                None
            }
        }
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = self.hub_receiver(chat_id);
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }

    /// The chat's live receiver, subscribed before any journal projection so a
    /// frame appended in between lands in the stream (dedupe by `seq`).
    fn hub_receiver(&self, chat_id: &str) -> broadcast::Receiver<JournaledEvent> {
        let mut hubs = lock(&self.inner.hubs);
        hubs.entry(chat_id.to_string())
            .or_insert_with(|| broadcast::channel(1024).0)
            .subscribe()
    }

    /// Latest journal seq for a chat (0 when the journal is missing/empty).
    /// Lets live-only streams (issue #31) subscribe from "now" without a
    /// full replay.
    pub fn latest_seq(&self, chat_id: &str) -> u64 {
        self.inner
            .journal
            .last_event(chat_id)
            .ok()
            .flatten()
            .map(|(seq, _)| seq)
            .unwrap_or(0)
    }

    /// Targeted journal lookup for one tool part (issue #10): empty when the
    /// journal is missing, rotated, or simply has no such id — never an error.
    pub fn tool_events(&self, chat_id: &str, part_id: &str) -> Vec<AgentEvent> {
        self.inner.journal.events_for_part(chat_id, part_id)
    }

    /// The compaction recap this host's run journal kept for one cycle. `None`
    /// is "there is nothing here" (the agent compacted without writing one, the
    /// file rotated, or this device never had the journal) — the caller answers
    /// `unavailable` rather than showing an empty summary as if the agent had
    /// said nothing.
    pub fn compaction_summary(&self, chat_id: &str, compaction_id: &str) -> Option<String> {
        self.inner
            .journal
            .compaction_summary(chat_id, compaction_id)
    }

    /// One subagent's feed, projected from this host's run journal — the only
    /// copy of a child's content that exists (there is no child doc, and the
    /// parent's doc holds nothing but its spawn chip). `None` means "this host
    /// has nothing to show": a viewer on another device, a chat whose journal
    /// died with it, or a rotated-away file. The RPC renders that as an explicit
    /// `Unavailable`, never as an empty transcript.
    pub fn subagent_events(
        &self,
        chat_id: &str,
        parent_tool_use_id: &str,
    ) -> Option<Vec<(u64, AgentEvent)>> {
        self.inner
            .journal
            .subagent_events(chat_id, parent_tool_use_id)
    }

    /// Live view of one subagent's feed: **subscribe first, then project**, so a
    /// nested event appended in between shows up in the live stream and the
    /// replay overlaps (dedupe by `seq`) instead of leaving a hole — the same
    /// ordering rule [`SessionsEngine::subscribe`] applies to a whole chat.
    ///
    /// The replay is `None` when this host holds no journal for the chat: an
    /// honest "nothing to show here", distinct from `Some(vec![])` = "the
    /// journal is here, this feed is empty (yet)".
    ///
    /// The live receiver is the CHAT's stream, unfiltered: it also carries the
    /// parent's traffic. Consumers keep the frames whose wrapper tags their own
    /// `parent_tool_use_id` (one hop: a nested wrapper belongs to its own feed).
    pub fn subscribe_subagent(
        &self,
        chat_id: &str,
        parent_tool_use_id: &str,
        after_seq: u64,
    ) -> Result<
        (
            Option<Vec<JournaledEvent>>,
            broadcast::Receiver<JournaledEvent>,
        ),
        EngineError,
    > {
        // Subscribe to the hub directly: `subscribe` would parse the whole
        // chat journal into a replay that this call discards (the feed's own
        // projection below reads the file again).
        let live = self.hub_receiver(chat_id);
        let Some(journal) = self.subagent_events(chat_id, parent_tool_use_id) else {
            return Ok((None, live));
        };
        let mut replay: Vec<_> = journal
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        // Same cursor rule as the chat's replay: a cursor ahead of this feed's
        // last event belongs to an earlier era (the file was replaced), so it
        // gets the whole feed rather than nothing.
        let last = replay.last().map(|event| event.seq).unwrap_or(0);
        if after_seq != 0 && after_seq <= last {
            replay.retain(|event| event.seq > after_seq);
        }
        Ok((Some(replay), live))
    }

    /// Whether a run is live on this host for `chat_id` — the one case where a
    /// missing journal is "not yet" rather than "not here".
    pub fn is_running(&self, chat_id: &str) -> bool {
        lock(&self.inner.runs).contains_key(chat_id)
    }

    /// The host's live per-subagent counters, for a stream that wants to say
    /// "3 tools · 12 s" while the child runs. Cloneable and read-only: holding
    /// one is not holding the engine.
    pub fn subagent_feeds(&self) -> SubagentFeeds {
        self.inner.subagent_feeds.clone()
    }

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox
    ///   (komet's persistent-session routing); otherwise any live run is interrupted
    ///   first — never two runtimes driving one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, false)
            .await
    }

    /// [`Self::dispatch`] with the startup-crash retry marker: the retry
    /// re-dispatches with `startup_retry = true`, which makes that attempt
    /// final (its own startup death surfaces instead of retrying again).
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        startup_retry: bool,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(chat_id, harness_id, request, message_id, startup_retry))
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        mut message_id: Option<String>,
        startup_retry: bool,
    ) -> Result<String, EngineError> {
        // External server secrets are resolved on the host immediately before
        // launch. Composio is handled by Komet's own MCP bridge so tool calls
        // can pass through the permission panel instead of reaching it directly.
        request.mcp_external = self
            .prepare_mcp_for_run(chat_id)
            .into_iter()
            .filter(|server| server.config.id != "composio")
            .map(resolved_mcp_to_proto)
            .collect();
        // Project-less chats store cwd `~` (the creating device can't know the
        // host's home); expand it here, on the host, where the run spawns.
        request.cwd = expand_home(&request.cwd);
        // Fail-fast gate: an explicit `sandbox_options` table that violates
        // its own provider rules never reaches a harness spawn. The options
        // WIN over the coarse sandbox level (and `auto_approve` grants
        // nothing), so validation judges the table alone.
        if let Err(validation) = komet_proto::validate_run_request(&request) {
            tracing::warn!(chat = %chat_id, ?validation, "run rejected: sandbox validation failed");
            let message = validation.to_string();
            self.inner.publish(
                chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                    kind: ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                },
            );
            self.inner.publish(
                chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message.clone()),
                    session_id: None,
                    reason: None,
                },
            );
            self.set_status(chat_id, SessionStatus::Idle, false);
            return Err(EngineError::Other(message));
        }
        // A provider with EXPLICIT non-empty options is caught by
        // validate_run_request's `request.harness` check above. When the
        // harness is resolved IMPLICITLY (request.harness absent → the chat's
        // stored config), enforce the same rule against the resolved
        // harness_id so e.g. a Cursor session cannot silently run with
        // OpenCode/Codex options it cannot apply.
        if request
            .sandbox_options
            .as_ref()
            .is_some_and(|o| !o.is_empty())
            && matches!(
                harness_id,
                komet_proto::HarnessId::Cursor
                    | komet_proto::HarnessId::Grok
                    | komet_proto::HarnessId::Hermes
                    | komet_proto::HarnessId::Pi
                    | komet_proto::HarnessId::Antigravity
                    | komet_proto::HarnessId::Cline
                    | komet_proto::HarnessId::Qoder
            )
        {
            let validation = komet_proto::ValidationError::ProviderOptionsRejected {
                provider: harness_id,
            };
            let message = validation.to_string();
            self.inner.publish(
                chat_id,
                &komet_proto::AgentEvent::Error {
                    message: message.clone(),
                    kind: komet_proto::ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                },
            );
            self.inner.publish(
                chat_id,
                &komet_proto::AgentEvent::Done {
                    status: komet_proto::DoneStatus::Errored,
                    result: None,
                    error: Some(message.clone()),
                    session_id: None,
                    reason: None,
                },
            );
            self.set_status(chat_id, komet_proto::SessionStatus::Idle, false);
            return Err(EngineError::Other(message));
        }
        // Symmetric reasoning validation: a requested level must be a known
        // unified id. Per-model support is enforced by each harness (omit the
        // flag rather than fail the run — leftover Medium on Haiku/Claude-via-
        // Antigravity/gpt-5.5 must not become "Run failed"). Grok/Pi advertise
        // real ladders; do not treat them as empty.
        if let Some(level) = request.reasoning {
            let supported = komet_proto::ReasoningLevel::ALL;
            if let Err(validation) = komet_proto::validate_reasoning(Some(level), supported) {
                tracing::warn!(chat = %chat_id, ?validation, "run rejected: reasoning validation failed");
                let message = validation.to_string();
                self.inner.publish(
                    chat_id,
                    &AgentEvent::Error {
                        message: message.clone(),
                        kind: ErrorKind::Unknown,
                        detail: None,
                        retryable: false,
                    },
                );
                self.inner.publish(
                    chat_id,
                    &AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(message.clone()),
                        session_id: None,
                        reason: None,
                    },
                );
                self.set_status(chat_id, SessionStatus::Idle, false);
                return Err(EngineError::Other(message));
            }
        }
        // Every dispatched prompt is a turn — routed steer or fresh run alike.
        self.note_turn_start(chat_id, &request.cwd);
        let routed = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.steerable,
                h.steer_tx.clone(),
                h.routed_steers.clone(),
            )
        });
        if let Some((run_id, steerable, steer_tx, ledger)) = routed {
            let message = SteerMessage {
                prompt: request.prompt.clone(),
                message_id: message_id.clone(),
            };
            if steerable && steer_tx.try_send(message).is_ok() {
                // The run can vanish between the send and here (the idle
                // reaper, a parked child death): the ledger entry below is
                // the at-least-once guarantee — the run task's exit drain
                // re-dispatches any accepted steer no `Steered` confirmed.
                let user_id = message_id.clone().unwrap_or_else(new_id);
                lock(&ledger).push_back(RoutedSteer {
                    prompt: request.prompt.clone(),
                    message_id: user_id.clone(),
                });
                let handle = self.doc_handle(chat_id)?;
                handle.write_user_message(&user_id, &request.prompt, now_ms())?;
                if self.is_live(chat_id, &run_id) {
                    // Working BEFORE the lastMessageAt bump: both ride the
                    // workspace doc from this one peer, so causal order makes it
                    // impossible for an observer to hold [new message, old status]
                    // — that gap read as unseen-with-no-live-run = a phantom
                    // "completed" flash on every remote send (2026-07-31).
                    self.set_status(chat_id, SessionStatus::Working, false);
                    self.inner.note_message(chat_id, &request.prompt);
                    return Ok(run_id);
                }
                // The run died around the send. If its exit drain already
                // claimed the entry, that re-dispatch owns the message —
                // otherwise reclaim it and fall through to a fresh run.
                let reclaimed = {
                    let mut ledger = lock(&ledger);
                    let before = ledger.len();
                    ledger.retain(|s| s.message_id != user_id);
                    ledger.len() != before
                };
                if !reclaimed {
                    self.inner.note_message(chat_id, &request.prompt);
                    return Ok(run_id);
                }
                // Keep the already-written doc entry's id for the fresh run
                // below (write_user_message dedupes by id).
                message_id = Some(user_id);
            }
            // Mailbox closed (runtime mid-teardown / non-steering harness) or
            // the routed run died with the message reclaimed: replace it.
            self.interrupt(chat_id).await?;
        }

        let harness = self.inner.registry.resolve(harness_id)?;
        // Send-path sync (models-refresh T5): a non-empty `request.model`
        // must be offered by the harness's LIVE catalog. A remembered id the
        // CLI no longer offers never dispatches — the UI sends None (harness
        // default) instead, but a stale id can still arrive from an older
        // client or a raced Rescan. Mock is exempt (tests / scripted runs).
        if harness_id != komet_proto::HarnessId::Mock
            && let Some(model) = request.model.as_deref().filter(|m| !m.is_empty())
        {
            let live = harness.models().await.map_err(|e| {
                EngineError::Other(format!(
                    "model catalog unavailable for {harness_id:?} CLI: {e}"
                ))
            })?;
            if !live.iter().any(|m| m.id == model) {
                let message = format!("model {model} not offered by {harness_id:?} CLI — Rescan");
                tracing::warn!(chat = %chat_id, model = %model, "run rejected: model not in live catalog");
                self.inner.publish(
                    chat_id,
                    &AgentEvent::Error {
                        message: message.clone(),
                        kind: ErrorKind::Unknown,
                        detail: None,
                        retryable: false,
                    },
                );
                self.inner.publish(
                    chat_id,
                    &AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(message.clone()),
                        session_id: None,
                        reason: None,
                    },
                );
                self.set_status(chat_id, SessionStatus::Idle, false);
                return Err(EngineError::Other(message));
            }
        }
        // External MCP stays with the agent (Claude Desktop, Codex config,
        // Cursor mcp.json, …). Komet only injects the per-run internal server
        // via `request.mcp` after `run_id` is minted.
        let handle = self.doc_handle(chat_id)?;
        let user_id = message_id.unwrap_or_else(new_id);
        handle.write_user_message(&user_id, &request.prompt, now_ms())?;
        let active_provider = handle
            .doc()
            .active_provider()
            .and_then(|slug| komet_proto::HarnessId::from_slug(&slug))
            .or_else(|| self.inner.harness_owner(chat_id));
        let switched_from = active_provider.filter(|active| *active != harness_id);
        if let Some(from) = switched_from {
            handle.doc().mark_provider_switched()?;
            let marker = MessagePart::Switch {
                id: format!("switch-{}", new_id()),
                from: from.display_name().into(),
                to: harness_id.display_name().into(),
                from_harness: Some(from.as_str().into()),
                to_harness: Some(harness_id.as_str().into()),
            };
            handle.doc().append_switch_part(&user_id, &marker)?;
            // Preserve sessions created before provider marks were introduced:
            // the active provider's current session is still discoverable via
            // the single-provider cache and becomes its per-provider mark.
            if let Some(session_id) = self.inner.resume_for(chat_id, &request.cwd, from) {
                let seen = handle
                    .doc()
                    .read_entries()?
                    .iter()
                    .rev()
                    .find(|entry| entry.id != user_id)
                    .map(|entry| entry.id.clone());
                handle.doc().set_provider_session(
                    from.as_str(),
                    &ProviderSessionMark {
                        session_id,
                        cwd: request.cwd.clone(),
                        seen,
                    },
                )?;
            }
        }
        handle.doc().set_active_provider(harness_id.as_str())?;

        // Engine-owned resume (komet sessions.ts:736 — every dispatch read the
        // chat's stored harness session): callers always send `resume: None`;
        // the engine threads the chat's prior harness session back in so a new
        // process (app restart) continues the same harness conversation. The
        // startup-crash retry injects too — a stale id is the harness's
        // problem now (`session/load` falls back to `session/new` internally),
        // and starting the retry fresh silently dropped a good conversation.
        let mut resume_injected = false;
        if request.resume.is_none() {
            request.resume = if handle.doc().provider_switched() {
                handle
                    .doc()
                    .provider_session(harness_id.as_str())
                    .filter(|mark| mark.cwd.is_empty() || mark.cwd == request.cwd)
                    .map(|mark| mark.session_id)
                    .filter(|session_id| !session_id.is_empty())
            } else {
                self.inner.resume_for(chat_id, &request.cwd, harness_id)
            };
            resume_injected = request.resume.is_some();
        }
        if handle.doc().provider_switched()
            && !request.prompt.trim_start().starts_with('/')
            && !request
                .prompt
                .starts_with("Previous conversation for context:")
        {
            let seen = handle
                .doc()
                .provider_session(harness_id.as_str())
                .filter(|mark| mark.cwd.is_empty() || mark.cwd == request.cwd)
                .and_then(|mark| mark.seen);
            request.prompt =
                replay_prompt(handle.doc(), seen.as_deref(), &user_id, &request.prompt)?;
        }
        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let pending_permissions: PendingPermissionChoices = Arc::new(Mutex::new(HashMap::new()));

        // Input bridge: the harness asks questions; we mint the request id, park the
        // resolver for `respond_input`, and surface the event through the run pipeline.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(
                move |questions: Vec<UserInputQuestion>, source: komet_proto::QuestionSource| {
                    let (tx, rx) = oneshot::channel();
                    let request_id = new_id();
                    let secret_ids = questions
                        .iter()
                        .filter(|q| q.secret)
                        .map(|q| q.id.clone())
                        .collect();
                    lock(&pending).insert(
                        request_id.clone(),
                        PendingInput {
                            resolver: tx,
                            secret_ids,
                        },
                    );
                    let _ = engine_tx.send(AgentEvent::InputRequested {
                        request_id,
                        questions,
                        // Which wire asked, as reported by the harness: the
                        // doc part carries it so the transcript says where an
                        // answer was going (native tool vs permission-shaped).
                        source,
                    });
                    rx
                },
            )
        };
        // Permission bridge: the harness asks "may I run X"; we mint the
        // request id, park the resolver for `respond_permission`, and surface
        // `PermissionRequested` through the run pipeline (mirror of the input
        // bridge above).
        let request_permission = Arc::new({
            let pending = pending_permissions.clone();
            let engine_tx = engine_tx.clone();
            let sessions = self.clone();
            let chat = chat_id.to_string();
            let permission_timeout: Option<std::time::Duration> = request
                .permission_timeout_ms
                .map(std::time::Duration::from_millis)
                .or(*lock(&self.inner.permission_timeout));
            Box::new(
                move |kind: komet_proto::PermissionKind,
                      summary: String,
                      choices: Vec<komet_proto::PermissionChoice>,
                      tool_call_id: Option<String>| {
                    let (tx, rx) = oneshot::channel();
                    let request_id = new_id();
                    lock(&pending).insert(request_id.clone(), tx);
                    let expires_at = permission_timeout.map(|timeout| {
                        crate::now_ms()
                            .saturating_add(timeout.as_millis().min(i64::MAX as u128) as i64)
                    });
                    let _ = engine_tx.send(AgentEvent::PermissionRequested {
                        request_id: request_id.clone(),
                        kind,
                        summary,
                        choices,
                        actions: Vec::new(),
                        provider: None,
                        tool_call_id,
                        expires_at,
                    });
                    // D4 — hang guard: an unanswered request auto-denies after
                    // the configured window. `respond_permission` is a no-op
                    // (Ok(false)) if the user already answered — the race is
                    // safe either way, and Deny carries the deny→interrupt
                    // semantics so a hung run can't outlive its prompt.
                    if let Some(timeout) = permission_timeout {
                        let watchdog_sessions = sessions.clone();
                        let watchdog_chat = chat.clone();
                        let watchdog_request_id = request_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(timeout).await;
                            if let Ok(true) = watchdog_sessions.respond_permission(
                                &watchdog_chat,
                                &watchdog_request_id,
                                komet_proto::PermissionChoice::Deny,
                            ) {
                                // The doc part must resolve too, or a dead
                                // approval panel would keep rendering.
                                if let Ok(handle) = watchdog_sessions.doc_handle(&watchdog_chat) {
                                    let _ = handle.doc().resolve_permission(&watchdog_request_id);
                                }
                                tracing::info!(
                                    chat = %watchdog_chat,
                                    request = %watchdog_request_id,
                                    "permission auto-denied: bridge timeout"
                                );
                            }
                        });
                    }
                    (request_id, rx)
                },
            )
        });
        let mcp_ask: AskFn = {
            let request_permission = request_permission.clone();
            let pending = pending_permissions.clone();
            let engine_tx = engine_tx.clone();
            let sessions = self.clone();
            let chat = chat_id.to_string();
            Arc::new(move |ask: PermissionAsk| {
                let (request_id, receiver) = request_permission(
                    komet_proto::PermissionKind::Tool {
                        name: format!("{}: {}", ask.server, ask.tool),
                    },
                    ask.summary,
                    vec![
                        komet_proto::PermissionChoice::Allow,
                        komet_proto::PermissionChoice::Deny,
                    ],
                    // An MCP approval arrives on the server's own channel, which
                    // has no agent tool-call id to link the decision to.
                    None,
                );
                let cancellation = McpPermissionCancellation {
                    pending: pending.clone(),
                    engine_tx: engine_tx.clone(),
                    sessions: sessions.clone(),
                    chat_id: chat.clone(),
                    request_id,
                };
                Box::pin(async move {
                    let _cancellation = cancellation;
                    match receiver.await {
                        Ok(komet_proto::PermissionChoice::Allow) => AskDecision::Allow,
                        Ok(komet_proto::PermissionChoice::AllowAlways { .. }) => {
                            AskDecision::AllowSession
                        }
                        Ok(komet_proto::PermissionChoice::Deny) | Err(_) => AskDecision::Deny,
                    }
                })
            })
        };
        let internal_mcp = self.start_internal_mcp(chat_id, &run_id, mcp_ask).await;
        if let Some(endpoint) = internal_mcp.as_ref() {
            request.mcp = Some(komet_proto::McpInjection {
                server_name: "komet".into(),
                url: endpoint.url().to_string(),
                auth_token: endpoint.auth_token().to_string(),
            });
        }
        let env = if harness_id == HarnessId::Copilot {
            self.inner
                .agent_accounts
                .get()
                .map(AgentAccounts::copilot_token_for_run)
                .transpose()?
                .flatten()
                .map(|token| vec![("COPILOT_GITHUB_TOKEN".into(), token)])
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());
        let interrupt_token = CancellationToken::new();
        let controls = RunControls {
            request_input,
            request_permission: Box::new({
                let request_permission = request_permission.clone();
                move |kind, summary, choices, tool_call_id| {
                    request_permission(kind, summary, choices, tool_call_id).1
                }
            }),
            steering: steer_rx,
            interrupt: interrupt_token.clone(),
            env,
        };

        lock(&self.inner.runs).insert(
            chat_id.to_string(),
            RunHandle {
                run_id: run_id.clone(),
                steerable: harness.supports_steering(),
                steer_tx,
                interrupt_token,
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
                pending_permissions,
                routed_steers: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            },
        );
        self.set_status(chat_id, SessionStatus::Working, true);
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        self.inner.note_message(chat_id, &request.prompt);

        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle.doc_arc(),
            controls,
            engine_rx,
            cancel_rx,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
                startup_retry,
            },
            internal_mcp,
        ));
        Ok(run_id)
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable)
            .map(|h| {
                (
                    h.run_id.clone(),
                    h.steer_tx.clone(),
                    h.routed_steers.clone(),
                )
            });
        let Some((run_id, steer_tx, ledger)) = target else {
            return Ok(SteerOutcome::NotSteerable);
        };
        let message = SteerMessage {
            prompt: prompt.to_string(),
            message_id: message_id.clone(),
        };
        if steer_tx.try_send(message).is_err() {
            return Ok(SteerOutcome::NotSteerable);
        }
        // Accepted: ledger first (at-least-once across a dying run), then the
        // user entry (client-minted id), then Working BEFORE the
        // lastMessageAt bump — same causal-order invariant as the dispatch
        // route (an observer must never hold [new message, settled status]:
        // the phantom "completed" flash, 2026-07-31).
        let user_id = message_id.unwrap_or_else(new_id);
        lock(&ledger).push_back(RoutedSteer {
            prompt: prompt.to_string(),
            message_id: user_id.clone(),
        });
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message(&user_id, prompt, now_ms())?;
        // A routed steer is a turn too. Fired here (not only on the confirmed
        // path) — a reclaim falls back to dispatch, which just re-snapshots.
        if let Some(request) = self.last_request(chat_id) {
            self.note_turn_start(chat_id, &request.cwd);
        }
        if self.is_live(chat_id, &run_id) {
            self.set_status(chat_id, SessionStatus::Working, false);
            self.inner.note_message(chat_id, prompt);
            return Ok(SteerOutcome::Accepted);
        }
        // The run died around the send. Exit drain claimed the entry → its
        // re-dispatch owns the message; still ours → reclaim and report
        // NotSteerable so the executor falls back to a fresh dispatch
        // (same message id — the doc entry dedupes).
        let reclaimed = {
            let mut ledger = lock(&ledger);
            let before = ledger.len();
            ledger.retain(|s| s.message_id != user_id);
            ledger.len() != before
        };
        if reclaimed {
            return Ok(SteerOutcome::NotSteerable);
        }
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted)
    }

    /// Interrupt the live run, if any. The run settles with a synthetic
    /// `Done{interrupted}` and its streaming entry stamped `aborted`; this waits
    /// (bounded) for that settlement so callers observe a consistent doc.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.interrupt_token.clone(),
                h.cancel.clone(),
                h.pending_inputs.clone(),
                h.pending_permissions.clone(),
                h.engine_tx.clone(),
            )
        });
        let Some((run_id, token, cancel, pending, pending_perms, engine_tx)) = target else {
            return Ok(false);
        };
        // Engine-side grace deadline FIRST: the run task must observe
        // `interrupted` before any teardown it triggers can end the stream,
        // or a harness that closes on its emptied question callback would be
        // misclassified as "stream ended without Done" (Errored) instead of
        // settling Interrupted.
        let _ = cancel.send(true);
        // Unpark any blocked question (mirrors komet: harness teardown can await a
        // parked question callback — a run stuck on a question would deadlock the stop).
        let parked: Vec<_> = lock(&pending)
            .drain()
            .map(|(_, pending_input)| pending_input.resolver)
            .collect();
        for resolver in parked {
            let _ = resolver.send(Vec::new());
        }
        // Unpark any blocked permissions (mirrors the question drain above) AND
        // publish each denial into the doc pipeline: without the synthetic
        // `PermissionResolved`, the transcript's `MessagePart::Permission`
        // stays `resolved: false` forever and its approval panel resurrects on
        // every later render of this dead run.
        let parked_perms: Vec<_> = lock(&pending_perms).drain().collect();
        for (request_id, tx) in parked_perms {
            let _ = tx.send(komet_proto::PermissionChoice::Deny);
            let _ = engine_tx.send(AgentEvent::PermissionResolved {
                request_id,
                choice: komet_proto::PermissionChoice::Deny,
            });
        }
        // Harness-level interrupt (protocol + child teardown).
        token.cancel();
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    /// Write the durable answer through the same path for live and orphaned
    /// questions. `device`/`decided_at` come from the command entry — the
    /// answering device, which is not necessarily this host — so a peer can see
    /// whose answer the agent got. Loro list position, never clock order,
    /// selects the merged winner.
    pub fn record_input_response(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: &[UserInputAnswer],
        applied: bool,
        device: &str,
        decided_at: i64,
    ) -> Result<bool, EngineError> {
        let handle = self.doc_handle(chat_id)?;
        Ok(handle.doc().record_input_response(
            request_id,
            &InputResponse {
                answers: answers.to_vec(),
                decided_at,
                device: device.to_string(),
                applied,
            },
        )?)
    }

    /// The single writer for a permission outcome, including the device and
    /// timestamp. Called for live decisions and dead-run fallbacks alike.
    pub fn record_permission_decision(
        &self,
        chat_id: &str,
        request_id: &str,
        choice: &komet_proto::PermissionChoice,
        device: &str,
        decided_at: i64,
    ) -> Result<bool, EngineError> {
        let handle = self.doc_handle(chat_id)?;
        Ok(handle.doc().decide_permission(
            request_id,
            Some(choice),
            Some(device),
            Some(decided_at),
        )?)
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(pending_input) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        if pending_input.resolver.send(answers.clone()).is_err() {
            return Ok(false);
        }
        // The harness has the real answers; everything that outlives this run
        // (journal, live broadcast) gets the secret-free copy.
        let answers = redact_secret_answers(answers, &pending_input.secret_ids);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
            answers,
        });
        Ok(true)
    }

    /// Resolve a pending permission request (Paseo `respondToPermission`).
    /// Returns `false` when no such request is pending (unknown id, run
    /// already settled, or the decision raced a dead run).
    pub fn respond_permission(
        &self,
        chat_id: &str,
        request_id: &str,
        choice: komet_proto::PermissionChoice,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_permissions.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(resolver) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let is_deny = choice == komet_proto::PermissionChoice::Deny;
        if resolver.send(choice.clone()).is_err() {
            return Ok(false);
        }
        let _ = engine_tx.send(AgentEvent::PermissionResolved {
            request_id: request_id.to_string(),
            choice,
        });
        if is_deny {
            // Deny stops the agent — interrupt the run (like Esc) so it
            // doesn't continue with the next tool.
            drop(pending);
            drop(engine_tx);
            // fire-and-forget: don't block the doc_host caller
            let sessions = self.clone();
            let chat = chat_id.to_string();
            tokio::spawn(async move {
                let _ = sessions.interrupt(&chat).await;
            });
        }
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (komet: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            let handle = self.doc_handle(&chat_id)?;
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation (komet recoverDraft, sessions.ts:538).
            if let Some((session_id, cwd, harness)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd, harness);
            }
            // The revival prompt: the last user message (idempotent re-dispatch
            // under the SAME id — `write_user_message` dedupes by id, so the
            // transcript never shows a duplicate).
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. } => Some((e.id.clone(), text.clone())),
                            _ => None,
                        })
                    })
            });
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| e.status == Some(MessageStatus::Streaming))
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            let will_resume = fresh && prompt.is_some() && attempts < MAX_AUTO_RESUME;

            let note = if will_resume {
                "Run interrupted by engine restart — resuming"
            } else {
                "Run interrupted by engine restart"
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.into()),
                session_id: None,
                reason: Some(DoneReason::EngineRestart),
            };
            self.inner.publish(&chat_id, &done);
            let stamped = handle.mark_abandoned_streams(note)?.len();
            self.set_status(&chat_id, SessionStatus::Idle, false);
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            // Multi-device guard: only the chat's HOST device re-dispatches a
            // crashed run. A non-host device booting with a stale journal (its
            // own crashed run from an earlier era, since the chat migrated) has
            // stamped the doc + closed the journal above — but must NEVER spawn
            // a competing run: the host owns the run plane (§7 device routing).
            if let Some(host) = self.inner.doc_host()
                && !host.is_host(&chat_id)
            {
                tracing::info!(
                    chat = %chat_id,
                    "auto-resume skipped: chat hosted by another device"
                );
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: the journal's own cwd (komet's draft config)
                    // — a crash can predate the debounced workspace-row write.
                    .or_else(|| {
                        let (_, cwd, _) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            prompt: String::new(),
                            harness: None,
                            model: None,
                            reasoning: None,
                            model_options: Default::default(),
                            cwd,
                            sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
                            sandbox_options: None,
                            auto_approve: false,
                            attachments: Vec::new(),
                            resume: None,
                            // Auto-resume after an engine restart: reconnects
                            // to the journal's already-existing `cwd` — must
                            // never mint a fresh worktree on top of a session
                            // that already has one (or doesn't need one).
                            worktree: None,
                            permission_timeout_ms: None,
                            mcp: None,
                            mcp_external: Vec::new(),
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.prompt = prompt_text;
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for_request(&chat_id, &request);
                match sessions
                    .dispatch(&chat_id, harness_id, request, Some(user_id))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed")
                    }
                }
            });
        }
        Ok(recovered)
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.interrupt(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

fn resolved_mcp_to_proto(server: crate::mcp::ResolvedMcpServer) -> komet_proto::ResolvedMcpServer {
    let config = server.config;
    komet_proto::ResolvedMcpServer {
        config: komet_proto::McpServerConfig {
            id: config.id,
            name: config.name,
            enabled: config.enabled,
            transport: match config.transport {
                crate::mcp::McpTransport::Stdio => komet_proto::McpTransport::Stdio,
                crate::mcp::McpTransport::Http => komet_proto::McpTransport::Http,
                crate::mcp::McpTransport::Sse => komet_proto::McpTransport::Sse,
            },
            command: config.command,
            args: config.args,
            url: config.url,
            headers: config.headers,
            env: config.env,
            always_load: config.always_load,
        },
        resolved_headers: server.resolved_headers,
        resolved_env: server.resolved_env,
    }
}

fn replay_prompt(
    doc: &SessionDoc,
    seen: Option<&str>,
    current_user_id: &str,
    prompt: &str,
) -> Result<String, DocError> {
    let entries = doc.read_entries()?;
    let start = seen
        .and_then(|id| entries.iter().position(|entry| entry.id == id))
        .map_or(0, |index| index + 1);
    let mut history = Vec::new();
    for entry in entries.iter().skip(start) {
        if entry.id == current_user_id {
            break;
        }
        let role = match entry.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => continue,
        };
        let text = entry
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() && !(role == "User" && text.trim_start().starts_with('/')) {
            history.push(format!("{role}:\n{text}"));
        }
    }
    if history.is_empty() {
        return Ok(prompt.to_string());
    }
    Ok(format!(
        "Previous conversation for context:\n{}\n\nCurrent user request:\n{prompt}",
        history.join("\n\n")
    ))
}

impl Inner {
    fn harness_owner(&self, chat_id: &str) -> Option<HarnessId> {
        lock(&self.harness_sessions)
            .get(chat_id)
            .map(|session| session.harness)
            .or_else(|| {
                self.journal_harness_session(chat_id)
                    .map(|(_, _, harness)| harness)
            })
    }

    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        // Belt on top of the drive_run gate: a progress frame must never reach
        // the journal, whatever path delivered it — including one nested in a
        // subagent wrapper, whose child's live output is the biggest such frame.
        if is_ephemeral(event) {
            self.broadcast_ephemeral(chat_id, event.clone());
            return 0;
        }
        // The summary text is journal-only unless this host opted into the doc:
        // the live feed is not where a recap belongs either, so the broadcast
        // copy carries the cycle's size and availability, not its prose.
        if matches!(event, AgentEvent::ContextCompaction { .. }) {
            return self.publish_compaction(chat_id, event);
        }
        let usage_changed = match event {
            AgentEvent::SessionStarted { model, .. } => {
                let mut usage_stats = lock(&self.usage_stats);
                let default_limit = komet_proto::default_context_limit_for_model(model);
                usage_stats
                    .entry(chat_id.to_string())
                    .or_insert_with(|| komet_proto::ContextUsageStats::new(default_limit));
                true
            }
            AgentEvent::ContextWindow { stats } => {
                let mut usage_stats = lock(&self.usage_stats);
                let entry = usage_stats.entry(chat_id.to_string()).or_insert_with(|| {
                    komet_proto::ContextUsageStats::new(if stats.context_limit > 0 {
                        stats.context_limit
                    } else {
                        200_000
                    })
                });
                entry.apply_snapshot(stats);
                true
            }
            AgentEvent::Usage {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_tokens,
                context_limit,
            } => {
                let mut usage_stats = lock(&self.usage_stats);
                let entry = usage_stats.entry(chat_id.to_string()).or_insert_with(|| {
                    komet_proto::ContextUsageStats::new(context_limit.unwrap_or(200_000))
                });
                entry.apply_turn(
                    *input_tokens,
                    *cached_input_tokens,
                    *output_tokens,
                    *reasoning_tokens,
                    *context_limit,
                );
                true
            }
            _ => false,
        };
        if usage_changed {
            self.broadcast_usage(chat_id);
        }
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// One compaction frame: the WHOLE event goes to the journal (summary text
    /// included — that is what `GetCompactionSummary` reads back), while the
    /// live feed gets the same frame with the prose removed. The recap is model
    /// text about the entire conversation; a broadcast copy would put it on
    /// every subscriber's heap and in every reconnect replay for a chip that
    /// only needs to say "compacted, N bytes available".
    fn publish_compaction(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        let mut stripped = event.clone();
        if let AgentEvent::ContextCompaction {
            summary,
            summary_delta,
            ..
        } = &mut stripped
        {
            *summary = None;
            *summary_delta = None;
        }
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: stripped,
            });
        }
        seq
    }

    /// The summary policy the fold writes the doc with — this host's own choice
    /// (see [`SessionsEngine::set_compaction_summary_in_doc`]).
    fn compaction_summary_policy(&self) -> CompactionSummaryPolicy {
        if self.compaction_summary_in_doc.load(Ordering::Relaxed) {
            CompactionSummaryPolicy::SummaryInDoc
        } else {
            CompactionSummaryPolicy::JournalOnly
        }
    }

    /// Broadcast one event WITHOUT journaling it (ephemeral streams: tool
    /// output progress). A journaled progress frame would rewrite the run
    /// journal thousands of times per turn for text that has no durable
    /// meaning, and replaying it on reconnect would splice stale output into
    /// chips that already resolved. `seq` 0 says "no durable position".
    fn broadcast_ephemeral(&self, chat_id: &str, event: AgentEvent) {
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent { seq: 0, event });
        }
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            self.attach_usage(chat_id, entry);
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn attach_usage(&self, chat_id: &str, session: &mut Session) {
        session.context_usage = lock(&self.usage_stats).get(chat_id).cloned();
    }

    /// Push the latest context-window snapshot onto the live Session row so
    /// WatchSessions (and the footer ring) see it without a status change.
    fn broadcast_usage(&self, chat_id: &str) {
        let stats = lock(&self.usage_stats).get(chat_id).cloned();
        let mut statuses = lock(&self.statuses);
        let Some(entry) = statuses.get_mut(chat_id) else {
            return;
        };
        entry.context_usage = stats;
        let mut list: Vec<Session> = statuses.values().cloned().collect();
        list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        self.sessions_tx.send_replace(list);
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    started_at: None,
                    updated_at: now,
                    context_usage: None,
                });
            // `started_at` is the elapsed-timer base and must only ever mean
            // "this turn". Entering Working from a settled state always
            // restamps (a steer into a parked session is a NEW turn — reusing
            // the old base showed the previous turn's 30min on send), and a
            // settled session drops its base entirely so no later reader can
            // resurrect a stale elapsed.
            let was_active = matches!(
                entry.status,
                SessionStatus::Working | SessionStatus::AwaitingInput
            );
            entry.status = status;
            entry.updated_at = now;
            match status {
                SessionStatus::Working if fresh_start || !was_active => {
                    entry.started_at = Some(now);
                }
                SessionStatus::Working | SessionStatus::AwaitingInput => {}
                SessionStatus::Idle | SessionStatus::Errored => {
                    entry.started_at = None;
                }
            }
            self.attach_usage(chat_id, entry);
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    /// The doc host, once wired. `None` before assembly or after retirement.
    fn doc_host(&self) -> Option<DocHost> {
        lock(&self.doc_host).clone()
    }

    fn workspace(&self) -> Option<crate::workspace_host::WorkspaceHost> {
        self.doc_host().and_then(|host| host.workspace().cloned())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row — the row is what survives an
    /// engine restart (komet sessions.ts:1039).
    fn remember_harness_session(
        &self,
        chat_id: &str,
        session_id: &str,
        cwd: &str,
        harness: HarnessId,
    ) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                harness,
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    /// Forget a harness session that proved unusable during a turn.  The
    /// empty-turn guard is deliberately retryable, but resuming the same ACP
    /// session would replay the failure forever.  Keep an explicit empty
    /// tombstone in the workspace row so the journal fallback cannot revive
    /// the poisoned id on the next dispatch.
    fn invalidate_harness_session(&self, chat_id: &str) {
        lock(&self.harness_sessions).remove(chat_id);
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, "", "");
        }
    }

    // NB: there is deliberately no `forget_harness_session` anymore. The old
    // tombstone fired on "run died before SessionStarted", which — since the
    // ACP conversion made stale ids a harness-internal fallback — only ever
    // meant a child STARTUP failure, and permanently severed good
    // conversations (user incident 2026-08-13). A truly stale id simply
    // yields a fresh session whose SessionStarted overwrites the row.

    /// The session id to resume for a run in `chat_id` launching from `cwd`
    /// (komet sessions.ts:736, looked up on every dispatch):
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str, harness: HarnessId) -> Option<String> {
        let cwd_ok = |session_cwd: &str| session_cwd.is_empty() || session_cwd == cwd;
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (known.harness == harness
                && !known.session_id.is_empty()
                && cwd_ok(&known.cwd))
            .then_some(known.session_id);
        }
        let (session_id, session_cwd, owner) = self.journal_harness_session(chat_id)?;
        if owner != harness || !cwd_ok(&session_cwd) || session_id.is_empty() {
            return None;
        }
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd, owner);
        Some(session_id)
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd of the `SessionStarted` that governs it. `Done.session_id`
    /// inherits the cwd of the most recent `SessionStarted` (same run).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String, HarnessId)> {
        let events = match self.journal.replay(chat_id, 0) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
                return None;
            }
        };
        let mut current_cwd = String::new();
        let mut current_harness = None;
        let mut found: Option<(String, String, HarnessId)> = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    harness,
                    session_id,
                    cwd,
                    ..
                } => {
                    current_cwd = cwd;
                    current_harness = Some(harness);
                    if !session_id.is_empty() {
                        found = Some((session_id, current_cwd.clone(), harness));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    found = Some((session_id, current_cwd.clone(), current_harness?));
                }
                _ => {}
            }
        }
        found
    }

    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
            // The live counters describe a turn that is over: whatever the
            // chips kept at their terminal write is the doc's record now.
            self.subagent_feeds.clear(chat_id);
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
                output,
                diff,
                output_ref,
                output_bytes,
                diff_ref,
                diff_stats,
                subagent_ref,
                subagent_status,
                subagent_tail,
                subagent_duration_ms,
                subagent_tool_count,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
                // Output summaries, diff stats, and sidecar refs are
                // deliberately kept: unlike raw tool inputs they are the
                // transcript's record of what happened, and the strip already
                // bounded them (docs/chat2-sync.md A1).
                output: output.clone(),
                diff: diff.clone(),
                output_ref: output_ref.clone(),
                output_bytes: *output_bytes,
                diff_ref: diff_ref.clone(),
                diff_stats: diff_stats.clone(),
                subagent_ref: subagent_ref.clone(),
                subagent_status: *subagent_status,
                subagent_tail: subagent_tail.clone(),
                // Two numbers about the child's own work — no content, nothing
                // to strip, and the chip's only record of the cycle.
                subagent_duration_ms: *subagent_duration_ms,
                subagent_tool_count: *subagent_tool_count,
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Fold one compaction frame into its durable home — exactly one part per
/// cycle, wherever that cycle first landed:
/// - the live segment already holds it: refine in place, like any other part;
/// - no segment is open (the boundary arrived between two turns): fold the
///   frame into the run's own accumulator and write a `System` entry for it, so
///   the fact keeps its position at the tail and its chunk sizes keep
///   accumulating across frames;
/// - the doc already carries the cycle elsewhere (it began parked and a steer
///   re-opened the session): refine THAT part, never mint a twin in the new
///   assistant entry;
/// - otherwise: fold into the segment (a mid-turn boundary).
///
/// Returns the part id of a `Completed` cycle still missing its post-compaction
/// count, for [`fill_compaction_tokens_after`] to answer later.
#[allow(clippy::too_many_arguments)]
fn route_compaction(
    doc: &SessionDoc,
    folded: &mut Vec<MessagePart>,
    stray: &mut Vec<MessagePart>,
    has_segment: bool,
    event: &AgentEvent,
    device_id: &str,
    policy: CompactionSummaryPolicy,
) -> Option<String> {
    let AgentEvent::ContextCompaction { id, .. } = event else {
        return None;
    };
    let part_id = format!("cmp-{id}");
    let outside_segment = !folded.iter().any(|part| part.id() == part_id)
        && (!has_segment || doc.has_compaction_part(&part_id));
    if !outside_segment {
        fold_event_into_parts_with(folded, event, policy);
        return None;
    }
    fold_event_into_parts_with(stray, event, policy);
    let index = stray.iter().position(|part| part.id() == part_id)?;
    let pending_count = matches!(
        stray[index],
        MessagePart::Compaction {
            status: CompactionStatus::Completed,
            tokens_after: None,
            ..
        }
    );
    if let Err(err) = doc.record_compaction_part(&stray[index], device_id, now_ms()) {
        tracing::warn!(error = %err, "failed to persist an inter-turn compaction");
        return None;
    }
    pending_count.then_some(part_id)
}

/// Stamp the post-compaction count the boundary frame itself did not carry onto
/// the one cycle still waiting for it. The newest unfilled compaction in the
/// live segment wins — a reading answers the most recent squeeze, never an older
/// one — and a cycle that was still waiting by then stays unknown rather than
/// borrowing somebody else's number. A part whose agent reported its own count
/// is never overwritten.
fn fill_compaction_tokens_after(
    folded: &mut [MessagePart],
    pending: &mut Option<String>,
    doc: &SessionDoc,
    tokens_after: u64,
) {
    if let Some(MessagePart::Compaction {
        tokens_after: slot, ..
    }) = folded.iter_mut().rev().find(|part| {
        matches!(
            part,
            MessagePart::Compaction {
                status: CompactionStatus::Completed,
                tokens_after: None,
                ..
            }
        )
    }) {
        *slot = Some(tokens_after);
        // The pending inter-turn cycle is older than this one, so this reading
        // is not its answer either; drop the claim instead of mislabelling.
        *pending = None;
        return;
    }
    let Some(part_id) = pending.take() else {
        return;
    };
    if let Err(err) = doc.apply_compaction_tokens_after(&part_id, tokens_after) {
        tracing::warn!(error = %err, "failed to stamp a compaction's post-window count");
    }
}

/// `~` / `~/…` → this host's home directory. Anything else passes through.
fn expand_home(cwd: &str) -> String {
    match cwd.strip_prefix("~") {
        Some("") => crate::repos::home_dir().to_string_lossy().into_owned(),
        Some(rest) if rest.starts_with('/') => crate::repos::home_dir()
            .join(&rest[1..])
            .to_string_lossy()
            .into_owned(),
        _ => cwd.to_string(),
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so
/// the startup-crash retry re-dispatches idempotently against the same doc
/// entry), whether `dispatch` injected the resume id itself (only
/// engine-injected resumes retry — a caller-specified resume fails loudly),
/// and whether this run already IS the retry (one attempt only).
struct RunResumeState {
    user_message_id: String,
    resume_injected: bool,
    startup_retry: bool,
}

/// Broadcast every buffered tool-output tail as one snapshot per tool, and
/// disarm the flush window. Ephemeral by construction: no journal write, no
/// fold, no doc commit — a reconnect simply starts with nothing streamed.
fn flush_tool_progress(
    inner: &Inner,
    chat_id: &str,
    progress: &mut ToolProgressBuffer,
    flush_at: &mut Option<tokio::time::Instant>,
) {
    for (id, text) in progress.take() {
        inner.broadcast_ephemeral(
            chat_id,
            AgentEvent::ToolProgress {
                id,
                text,
                append: false,
            },
        );
    }
    *flush_at = None;
}

#[allow(clippy::too_many_arguments)]
async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    doc: Arc<SessionDoc>,
    controls: RunControls,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    resume_state: RunResumeState,
    _internal_mcp: Option<RunningEndpoint>,
) {
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    // Kept whole for the startup-crash retry (same user entry; dispatch
    // re-injects the stored resume id). Option so the retry branch (inside
    // the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    // The run is dispatched and the CLI is being spawned: name that wait from
    // the first frame, before the harness has anything to say. Ephemeral, so a
    // viewer that connects mid-run simply reads the row's `Working` status.
    let mut run_phase = Some(RunPhase::Starting);
    inner.broadcast_ephemeral(
        &chat_id,
        AgentEvent::RunPhase {
            phase: RunPhase::Starting,
        },
    );
    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(err) => {
            let message = err.to_string();
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                    kind: ErrorKind::Unknown,
                    detail: None,
                    retryable: false,
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                    reason: None,
                },
            );
            inner.remove_run(&chat_id, &run_id);
            inner.set_status(&chat_id, SessionStatus::Errored, false);
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    // Ephemeral tool output, buffered per tool id and broadcast on a coalesced
    // window (see [`ToolProgressBuffer`]). Nothing here reaches the doc or the
    // journal.
    let mut progress = ToolProgressBuffer::default();
    let mut progress_flush_at: Option<tokio::time::Instant> = None;
    // Every tool id this run has folded, across segment resets. Adapters
    // re-emit shape-bearing `tool_call_update`s (title/rawInput refreshes,
    // long-running completions) as full ToolCall events; once the fold has
    // reset at a steer/park boundary those ids are gone from `folded`, and
    // folding the echo would mint an orphan chip mid-text in the NEXT
    // segment — the mid-word transcript splits.
    let mut seen_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entry_id = new_id();
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    // Compaction cycles with no home in the streaming segment: a boundary that
    // arrived between two turns (it got its own `System` entry) and any later
    // frame of that cycle. The fold keeps accumulating them, so a cycle's chunk
    // sizes add up across frames whatever entry its part ended up in.
    let mut stray_compactions: Vec<MessagePart> = Vec::new();
    // A `Completed` inter-turn cycle whose post-compaction count the agent did
    // not report — waiting for the next native window reading, and only for it.
    let mut pending_tokens_after: Option<String> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (komet runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (komet SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let steerable = harness.supports_steering();
    // TURN-QUIESCE WATCHDOG (2026-08-12 stuck-Working incident): a harness
    // that loses a turn's Done — the adapter never settles `session/prompt`
    // even though the agent finished — strands Working forever: the live
    // heartbeat above keeps the row fresh, and there is no per-turn timeout
    // by design. This is NOT that stall timeout: it never ends the run or
    // errors anything. When the stream has been silent past the window AND
    // the fold shows completed output with nothing in flight (no unresolved
    // tool, no open question), the turn parks exactly like a Done would —
    // segment finalized Complete, status Idle, child and mailbox warm. A
    // false trip (the agent was quietly waiting on something invisible)
    // costs a status dip: the parked-resume path below re-arms Working the
    // moment output flows again, and nothing is lost. `KOMET_TURN_QUIESCE_MS`
    // overrides the window; 0 disables.
    let quiesce_after: Option<std::time::Duration> = match std::env::var("KOMET_TURN_QUIESCE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => Some(std::time::Duration::from_secs(120)),
    };
    let mut last_stream_activity = tokio::time::Instant::now();
    // SELF-CONTINUED turns get a much SHORTER quiesce window. A turn the
    // agent starts on its own (background-task wake) can never receive a
    // harness Done: the adapter has no `session/prompt` outstanding to
    // settle — verified in claude-agent-acp's autonomous-result lane, which
    // consumes the SDK's turn-end without emitting anything; codex shows
    // the same shape. The watchdog is that turn shape's ONLY settle path,
    // so the default 120s window read as 2min of stuck-Working after every
    // background notification (user report 2026-08-13). The in-flight
    // fold gate below still protects running tools; reasoning heartbeats
    // push the window during real thinking. `KOMET_SELF_TURN_QUIESCE_MS`
    // overrides; 0 falls back to the normal window. An explicit
    // `KOMET_TURN_QUIESCE_MS=0` still disables the watchdog entirely.
    let self_quiesce_after: Option<std::time::Duration> =
        match std::env::var("KOMET_SELF_TURN_QUIESCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
            None => Some(std::time::Duration::from_secs(20)),
        };
    let mut self_continued_turn = false;

    let final_status = loop {
        let event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
                // Engine synthesized it because the user stopped the run.
                reason: Some(DoneReason::UserRequested),
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Idle reaper (komet SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                // A stream error while PARKED is a post-turn child death —
                // the turn was already finalized, so the run ends clean
                // instead of stamping a completed session Errored.
                Some(Err(err)) if idle_since.is_some() => {
                    tracing::warn!(chat = %chat_id, error = %err, "parked session child died; ending clean");
                    break SessionStatus::Idle;
                }
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                    reason: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                    reason: Some(DoneReason::UserRequested),
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                    reason: None,
                },
            },
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
                if let Err(err) = sync_segment(
                    doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                }
                dirty = false;
                continue;
            }
            _ = tokio::time::sleep_until(
                progress_flush_at.unwrap_or_else(tokio::time::Instant::now)
            ), if progress_flush_at.is_some() => {
                // Coalesced live-output tick: broadcast, nothing else. Not
                // stream activity — it must not push the quiesce window.
                flush_tool_progress(
                    &inner, &chat_id, &mut progress, &mut progress_flush_at,
                );
                continue;
            }
            // Turn-quiesce watchdog (see the knob above). Armed only when the
            // fold says nothing is in flight: an unresolved tool part is a
            // command still running (legitimately silent for minutes — the
            // rejected stall-timeout case), an unresolved input part is a
            // question awaiting the user. The live-plan chip is exempt from
            // the tool check: it is a singleton that never resolves. An EMPTY
            // fold still arms — a Steered boundary that no output ever
            // follows is one of the wedge shapes — it just parks without
            // writing a segment (an empty finalize would leave a stub entry).
            _ = tokio::time::sleep_until({
                let mut window = quiesce_after.unwrap_or_default();
                if self_continued_turn && let Some(short) = self_quiesce_after {
                    window = window.min(short);
                }
                last_stream_activity + window
            }), if quiesce_after.is_some()
                && idle_since.is_none()
                && !interrupted
                && steerable
                && !folded.iter().any(|p| match p {
                    MessagePart::Tool { id, resolved: false, .. } => {
                        id != komet_proto::LIVE_PLAN_TOOL_ID
                    }
                    MessagePart::Input { resolved: false, .. } => true,
                    _ => false,
                }) =>
            {
                tracing::warn!(
                    chat = %chat_id,
                    quiet_ms = quiesce_after.unwrap_or_default().as_millis() as u64,
                    "turn quiesced: stream silent after completed output with no \
                     turn-end; parking (suspected missing harness Done)"
                );
                if !folded.is_empty() || writer.is_some() {
                    if let Err(err) = finish_segment(
                        doc_ref,
                        writer.take(),
                        &entry_id,
                        &device_id,
                        segment_started,
                        &folded,
                        MessageStatus::Complete,
                    ) {
                        tracing::warn!(chat = %chat_id, error = %err, "quiesce segment finish failed");
                    }
                    inner.note_message(&chat_id, &folded_text(&folded));
                }
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                idle_since = Some(tokio::time::Instant::now());
                self_continued_turn = false;
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                continue;
            }
        };

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled), and
        // push the quiesce watchdog's window out.
        inner.touch_session(&chat_id);
        last_stream_activity = tokio::time::Instant::now();
        // The engine's input bridge is the sole authority on input requests:
        // it mints the id and parks the resolver BEFORE emitting the event,
        // so a legitimate id is always pending here. A harness emitting its
        // own copy (a different id no resolver knows) would fold an
        // unanswerable twin chip into the doc — and answering the twin would
        // never resume the run. Dropped BEFORE the parked gate below: letting
        // it un-park the session and then dropping it would disarm the idle
        // reaper with no turn running (a leaked child no reap ever ends).
        if let AgentEvent::InputRequested { request_id, .. } = &event {
            let pending = lock(&inner.runs)
                .get(&chat_id)
                .map(|h| h.pending_inputs.clone());
            let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
            if !known {
                tracing::warn!(
                    chat = %chat_id,
                    request = %request_id,
                    "dropping harness-emitted InputRequested (unknown id; \
                     the engine input bridge owns this lifecycle)"
                );
                continue;
            }
        }
        if let AgentEvent::PermissionRequested { request_id, .. } = &event {
            let pending = lock(&inner.runs)
                .get(&chat_id)
                .map(|h| h.pending_permissions.clone());
            let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
            if !known {
                tracing::warn!(
                    chat = %chat_id,
                    request = %request_id,
                    "dropping harness-emitted PermissionRequested (unknown id; \
                     the engine permission bridge owns this lifecycle)"
                );
                continue;
            }
        }
        // PARKED: a steer boundary, a terminal Done, or SELF-CONTINUED OUTPUT
        // re-opens the session; everything else stays gated. The ACP child
        // keeps forwarding `session/update` frames after a turn completes,
        // and they split two ways:
        //
        // - Post-turn NOISE — late tool_call_updates for commands folded in a
        //   prior segment, command refreshes, reasoning heartbeats. Treating
        //   those as "the next turn" re-armed Working with no Done ever
        //   coming (the eternally-running session bug) and folded orphan
        //   parts into a phantom segment. Still dropped.
        // - SELF-CONTINUED WORK — Claude Code re-invokes itself when a
        //   background task finishes (turns no prompt started) and streams
        //   real output for them. Dropping those LOST transcript content
        //   (2026-08-12: "Build finished successfully…" streamed by the
        //   agent, absent from the doc). Fresh text or a genuinely new tool
        //   call resumes the session: new segment, Working, and the turn
        //   settles again via Done — or via the quiesce watchdog, which is
        //   what makes this resume safe where the naive version was not.
        //
        // The RESUME_GATE separates the two by arrival time: a finished
        // turn's tail flush lands within milliseconds of its Done, while a
        // self-continued turn starts a whole new agent round trip (seconds
        // at minimum, minutes in the incident). Inside the gate everything
        // non-boundary stays inert, exactly as before.
        const RESUME_GATE: std::time::Duration = std::time::Duration::from_secs(1);
        if idle_since.is_some() {
            let self_continued = idle_since
                .is_some_and(|parked_at| parked_at.elapsed() >= RESUME_GATE)
                && (matches!(
                    &event,
                    AgentEvent::TextDelta { text } if !text.is_empty()
                ) || matches!(
                    &event,
                    AgentEvent::ToolCall { id, .. }
                        if id == komet_proto::LIVE_PLAN_TOOL_ID || !seen_tools.contains(id)
                ));
            if self_continued {
                tracing::info!(
                    chat = %chat_id,
                    "parked session resumed by self-continued agent output"
                );
                idle_since = None;
                self_continued_turn = true;
                // The park cleared the fold; rotate to a fresh entry and
                // fall through — this event is the new segment's first part.
                entry_id = new_id();
                segment_started = now_ms();
                inner.set_status(&chat_id, SessionStatus::Working, true);
            } else {
                match &event {
                    AgentEvent::Steered { .. } => {
                        idle_since = None;
                        inner.set_status(&chat_id, SessionStatus::Working, true);
                    }
                    AgentEvent::Done { .. } => {
                        idle_since = None;
                    }
                    // A question with NO turn behind it (post-turn permission
                    // noise): answer it empty right here. Un-parking into
                    // AwaitingInput would disarm the idle reaper with no Done
                    // ever coming — stranded status, leaked warm child.
                    AgentEvent::InputRequested { request_id, .. } => {
                        let pending_input = lock(&inner.runs)
                            .get(&chat_id)
                            .and_then(|h| lock(&h.pending_inputs).remove(request_id));
                        if let Some(pending_input) = pending_input {
                            let _ = pending_input.resolver.send(Vec::new());
                        }
                        tracing::debug!(chat = %chat_id, "parked session: post-turn input request auto-declined");
                        continue;
                    }
                    // A stale answer settling after its turn already closed:
                    // nothing is running — stay parked.
                    AgentEvent::InputResolved { .. } => continue,
                    // A permission request with NO turn behind it: deny it right here.
                    AgentEvent::PermissionRequested { request_id, .. } => {
                        let resolver = lock(&inner.runs)
                            .get(&chat_id)
                            .and_then(|h| lock(&h.pending_permissions).remove(request_id));
                        if let Some(tx) = resolver {
                            let _ = tx.send(komet_proto::PermissionChoice::Deny);
                        }
                        tracing::debug!(chat = %chat_id, "parked session: post-turn permission request auto-denied");
                        continue;
                    }
                    AgentEvent::PermissionResolved { .. } => continue,
                    // A compaction that fires between two turns is a durable
                    // FACT, not a turn: journal it and give it its own `System`
                    // entry, but stay PARKED — un-parking into Working would
                    // arm the row with no Done behind it (the wedge this gate
                    // exists to prevent). The UI reads the liveliness off the
                    // part's own `inProgress` status.
                    AgentEvent::ContextCompaction { .. } => {
                        inner.publish(&chat_id, &event);
                        if let Some(part_id) = route_compaction(
                            doc_ref,
                            &mut folded,
                            &mut stray_compactions,
                            false,
                            &event,
                            &device_id,
                            inner.compaction_summary_policy(),
                        ) {
                            pending_tokens_after = Some(part_id);
                        }
                        continue;
                    }
                    _ => continue,
                }
            }
        }
        // Live tool output: buffered and broadcast on the coalesced tick
        // above. It is NOT folded, NOT journaled, and never marks the segment
        // dirty — the doc's record of a tool's output stays the one result.
        if let AgentEvent::ToolProgress { id, text, append } = &event {
            progress.push(id, text, *append);
            if progress_flush_at.is_none() {
                progress_flush_at = Some(tokio::time::Instant::now() + TOOL_PROGRESS_FLUSH);
            }
            continue;
        }
        // Anything durable flushes the pending burst first, so a chip's live
        // stream can never arrive after the result that closed it.
        if !progress.is_empty() {
            flush_tool_progress(&inner, &chat_id, &mut progress, &mut progress_flush_at);
        }

        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }

        // Stale tool echoes: a ToolCall/ToolResult naming an id folded in a
        // PRIOR segment (the fold reset at a steer or park since) belongs to
        // a chip that already rendered in its own entry. Folding the echo
        // would splice a phantom chip into the middle of the current
        // segment's streaming text (the mid-word transcript splits). Dropped;
        // same-segment refreshes still land in place.
        let in_segment = |folded: &[MessagePart], id: &str| {
            folded
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id: pid, .. } if pid == id))
        };
        match &event {
            // The live plan chip is a deliberate singleton (every update
            // reuses `LIVE_PLAN_TOOL_ID` so the fold refreshes in place):
            // treating its reappearance after a park/steer reset as a stale
            // echo dropped the todo list for the rest of the run — from the
            // first boundary on, plans never rendered again.
            AgentEvent::ToolCall { id, .. } if id == komet_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolResult { id, .. } if id == komet_proto::LIVE_PLAN_TOOL_ID => {}
            AgentEvent::ToolCall { id, .. } => {
                if !in_segment(&folded, id) && seen_tools.contains(id) {
                    continue;
                }
                seen_tools.insert(id.clone());
            }
            AgentEvent::ToolResult { id, .. }
                if !in_segment(&folded, id) && seen_tools.contains(id) =>
            {
                continue;
            }
            _ => {}
        }

        // Startup-crash retry: a run that dies before ever starting (errored
        // Done, no SessionStarted, nothing streamed) means the AGENT CHILD
        // failed to come up — not that the injected resume id was bad. Since
        // the ACP conversion (2026-08-08) a stale id is handled inside the
        // harness (`session/load` falls back to `session/new`), so the old
        // guess here — tombstone the id, retry fresh — fired only on child
        // startup failures and permanently severed GOOD conversations (user
        // incident 2026-08-13). The id stays; retry ONCE against the same
        // user entry, resume and all, in case the crash was transient. A
        // helper that is down hard fails the retry too and surfaces its
        // crash text (the harness now appends exit status + stderr).
        if resume_state.resume_injected
            && !resume_state.startup_retry
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "run died before session start; retrying once (resume kept)"
            );
            inner.remove_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            tokio::spawn(async move {
                // The user entry write inside dispatch is idempotent by
                // message id; `startup_retry` makes this attempt final.
                if let Err(err) = engine
                    .dispatch_with(&chat, harness_id, retry, Some(message_id), true)
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "startup-crash retry dispatch failed");
                    // No run is coming: leaving the row Working would spin
                    // the session forever with nothing behind it.
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            inner.publish(&chat_id, &event);
            // A steer boundary means a real prompt owns the turn again — its
            // Done will come; the short self-continued window stands down.
            self_continued_turn = false;
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            // The elapsed timer is per user message, not per child process: a
            // steer boundary restarts it (matches the parked-resume path and
            // the composer's optimistic overlay, which already reads 0:00).
            inner.set_status(&chat_id, SessionStatus::Working, true);
            // The boundary confirms delivery of the oldest accepted steer —
            // retire its at-least-once ledger entry.
            if let Some(h) = lock(&inner.runs)
                .get(&chat_id)
                .filter(|h| h.run_id == run_id)
            {
                lock(&h.routed_steers).pop_front();
            }
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                harness: started_harness,
                session_id,
                cwd,
                ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd, *started_harness);
                if !session_id.is_empty()
                    && let Err(error) = doc.set_provider_session(
                        started_harness.as_str(),
                        &ProviderSessionMark {
                            session_id: session_id.clone(),
                            cwd: cwd.clone(),
                            seen: Some(resume_state.user_message_id.clone()),
                        },
                    )
                {
                    tracing::warn!(
                        chat = %chat_id,
                        provider = %started_harness.as_str(),
                        error = %error,
                        "failed to persist provider session mark"
                    );
                }
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                status,
                error,
                ..
            } => {
                // OpenCode can report a successful ACP stop with no wire
                // output.  The harness converts that into the retryable
                // "empty turn" error; do not persist the same poisoned
                // session or every later command will fail identically.
                let poisoned_empty_turn = *status == DoneStatus::Errored
                    && error
                        .as_deref()
                        .is_some_and(|message| message.contains("returned an empty turn"));
                if poisoned_empty_turn {
                    inner.invalidate_harness_session(&chat_id);
                } else {
                    inner.remember_harness_session(&chat_id, session_id, &run_cwd, harness_id);
                }
            }
            AgentEvent::InputRequested { .. } => {
                // Known-id guaranteed: the unknown-id twin was dropped above,
                // before the parked gate.
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            _ => {}
        }

        // Live subagent counters FIRST: a viewer that snapshots a feed when it
        // receives this frame must see what this frame says, not what preceded
        // it.
        let feed_hits = touched_feeds(&event);
        inner.subagent_feeds.record(&chat_id, &feed_hits, now_ms());

        inner.publish(&chat_id, &event);

        // Then the trailer's phase: a viewer that receives this frame sees the
        // phase the event it just folded put the run in. Broadcast-only — see
        // [`phase_change`] and [`AgentEvent::RunPhase`].
        if let Some(next) = phase_change(&event, run_phase) {
            run_phase = Some(next);
            inner.broadcast_ephemeral(&chat_id, AgentEvent::RunPhase { phase: next });
        } else if matches!(&event, AgentEvent::Done { .. }) {
            run_phase = None;
        }

        // Defensive rule from komet: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            let summary_policy = inner.compaction_summary_policy();
            let has_segment = !folded.is_empty() || writer.is_some();
            let pending = if matches!(&event, AgentEvent::ContextCompaction { .. }) {
                route_compaction(
                    doc_ref,
                    &mut folded,
                    &mut stray_compactions,
                    has_segment,
                    &event,
                    &device_id,
                    summary_policy,
                )
            } else {
                fold_event_into_parts_with(&mut folded, &event, summary_policy);
                None
            };
            if pending.is_some() {
                pending_tokens_after = pending;
            }
            // R2 sidecar PARKED (2026-08-10, product call): the fold's
            // summary/stats ARE the doc's whole record — no refs stamped, no
            // uploads. Full outputs survive only in the host's local run
            // journal. To reintroduce: `komet_doc::sidecar_payload(&event)`
            // → `apply_sidecar_refs` → `doc_host.upload_tool_sidecar`, all
            // still in place and tested.
        }

        // The spawn chip's ONE terminal write, and the only moment this run
        // touches the doc for a subagent's numbers: the fold has already
        // settled the chip's lifecycle for this event, so stamping after it
        // means the status and its numbers land in the same coalesced commit.
        for (parent, hit) in &feed_hits {
            let FeedHit::Status(status) = *hit else {
                continue;
            };
            if status == SubagentStatus::Running {
                continue;
            }
            if let Some(feed) = inner.subagent_feeds.get(&chat_id, parent) {
                stamp_subagent_terminal(&mut folded, parent, feed, now_ms());
            }
        }

        if let AgentEvent::ContextWindow { stats } = &event
            && stats.source == ContextUsageSource::Native
            && stats.used() > 0
        {
            // The boundary frame rarely carries the post-compaction count; the
            // next native window reading IS that number. A parked session never
            // gets here (its usage frames are post-turn noise that can predate
            // the boundary) and an approximate/estimated reading is a guess, so
            // both leave the count unknown: the line draws no arrow rather than
            // a wrong one.
            fill_compaction_tokens_after(
                &mut folded,
                &mut pending_tokens_after,
                doc_ref,
                stats.used(),
            );
        }

        if let AgentEvent::Done { status, .. } = &event {
            // A question still pending at turn end can never be legitimately
            // answered (its turn is over): drain the resolvers NOW, or a late
            // `respond_input` finds one, emits InputResolved, and un-parks
            // the session into Working with no turn behind it — stranded
            // Working, timer forever, reaper disarmed. Empty answers unblock
            // the harness-side bridge like an interrupt does.
            let pending = lock(&inner.runs)
                .get(&chat_id)
                .filter(|h| h.run_id == run_id)
                .map(|h| h.pending_inputs.clone());
            if let Some(pending) = pending {
                for (_, pending_input) in lock(&pending).drain() {
                    let _ = pending_input.resolver.send(Vec::new());
                }
            }
            // Mirror of the input drain above, for the same reason: a
            // permission still pending at turn end can never be legitimately
            // answered. Draining the resolver here (not just the doc-part
            // force-resolve below) closes the underlying oneshot too, so a
            // `respond_permission` racing this Done can't find a live
            // resolver for a run that's already gone.
            let pending_perms = lock(&inner.runs)
                .get(&chat_id)
                .filter(|h| h.run_id == run_id)
                .map(|h| h.pending_permissions.clone());
            if let Some(pending_perms) = pending_perms {
                for (request_id, tx) in lock(&pending_perms).drain() {
                    let _ = tx.send(komet_proto::PermissionChoice::Deny);
                    inner.publish(
                        &chat_id,
                        &AgentEvent::PermissionResolved {
                            request_id,
                            choice: komet_proto::PermissionChoice::Deny,
                        },
                    );
                }
            }
            let message_status = match status {
                DoneStatus::Interrupted => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input AND
            // permission parts — neither must outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            // Permission force-resolves to Deny (mirrors the synthetic
            // `PermissionResolved` just published above, and the deny-wins-
            // on-conflict rule in `fold_event_into_parts`): an agent that's
            // gone can't act on a belated Allow, so Deny is the only decision
            // that can't misrepresent what happened.
            for part in folded.iter_mut() {
                match part {
                    MessagePart::Input { resolved, .. } => *resolved = true,
                    MessagePart::Permission {
                        resolved, decision, ..
                    } => {
                        *resolved = true;
                        if decision.is_none() {
                            *decision = Some(komet_proto::PermissionChoice::Deny);
                        }
                    }
                    _ => {}
                }
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if let Some(mut mark) = doc.provider_session(harness_id.as_str())
                && let Ok(entries) = doc.read_entries()
                && let Some(last) = entries.last()
            {
                mark.seen = Some(last.id.clone());
                if let Err(error) = doc.set_provider_session(harness_id.as_str(), &mark) {
                    tracing::warn!(
                        chat = %chat_id,
                        provider = %harness_id.as_str(),
                        error = %error,
                        "failed to update provider transcript watermark"
                    );
                }
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            // Exchange completed on an untitled chat → name it (fire-and-forget;
            // interrupted/errored turns never trigger naming).
            if *status == DoneStatus::Completed
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(&chat_id, harness_id, &user_prompt, &run_cwd);
            }
            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness PARKS instead of ending — child + mailbox stay warm for
            // the next routed dispatch; per-turn state resets for it.
            if *status == DoneStatus::Completed && steerable && !interrupted {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                self_continued_turn = false;
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                continue;
            }
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    // Claim any accepted-but-unconfirmed steers BEFORE the handle goes away:
    // a routed send that raced this exit either finds its entry gone (we own
    // it — re-dispatched below) or reclaims it and starts a fresh run itself.
    let orphans: Vec<RoutedSteer> = lock(&inner.runs)
        .get(&chat_id)
        .filter(|h| h.run_id == run_id)
        .map(|h| std::mem::take(&mut *lock(&h.routed_steers)).into())
        .unwrap_or_default();
    inner.remove_run(&chat_id, &run_id);
    inner.set_status(&chat_id, final_status, false);
    if !interrupted && !orphans.is_empty() {
        // The dying run accepted these into its mailbox but never confirmed a
        // Steered boundary (idle-reaper race, a mid-turn error discarding
        // queued boundary steers, a parked child death). Their user entries
        // are already in the transcript — a message that shows as sent must
        // never silently not run. Re-dispatch each as a fresh turn
        // (write_user_message dedupes by id; resume is engine-injected).
        let engine = SessionsEngine {
            inner: inner.clone(),
        };
        let chat = chat_id.clone();
        tokio::spawn(async move {
            for steer in orphans {
                let Some(mut request) = engine.last_request(&chat) else {
                    tracing::warn!(chat = %chat, "orphaned steer lost: no run config to re-dispatch");
                    break;
                };
                request.prompt = steer.prompt.clone();
                request.resume = None;
                request.attachments = Vec::new();
                tracing::info!(chat = %chat, "re-dispatching steer orphaned by a dying run");
                if let Err(err) = engine
                    .dispatch(&chat, harness_id, request, Some(steer.message_id.clone()))
                    .await
                {
                    tracing::warn!(chat = %chat, error = %err, "orphaned steer re-dispatch failed");
                    engine
                        .inner
                        .set_status(&chat, SessionStatus::Errored, false);
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::{ContextUsageSource, ContextUsageStats, ToolCall};

    #[test]
    fn secret_answers_are_redacted_only_in_the_persisted_copy() {
        let answers = vec![
            UserInputAnswer {
                question_id: "token".into(),
                labels: vec!["ghp_x".into()],
                free_text: Some("ghp_x".into()),
            },
            UserInputAnswer {
                question_id: "env".into(),
                labels: vec!["prod".into()],
                free_text: None,
            },
            UserInputAnswer {
                question_id: "token2".into(),
                labels: vec![],
                free_text: None,
            },
        ];
        let redacted = redact_secret_answers(answers.clone(), &["token".into(), "token2".into()]);
        // The harness already received `answers`; only the journal/broadcast
        // copy is redacted.
        assert_eq!(answers[0].free_text.as_deref(), Some("ghp_x"));
        assert!(redacted[0].labels.is_empty());
        assert_eq!(
            redacted[0].free_text.as_deref(),
            Some(komet_proto::WITHHELD_SECRET_ANSWER)
        );
        assert_eq!(redacted[1], answers[1], "non-secret answers travel intact");
        assert_eq!(
            redacted[2], answers[2],
            "a secret question the user skipped stays empty, not marked"
        );

        assert_eq!(redact_secret_answers(answers.clone(), &[]), answers);
    }

    #[test]
    fn tool_progress_buffer_coalesces_per_tool_and_caps_the_tail() {
        let mut buf = ToolProgressBuffer::default();
        buf.push("a", "one ", true);
        buf.push("a", "two", true);
        buf.push("b", "second tool", false);
        assert_eq!(
            buf.take(),
            vec![
                ("a".to_string(), "one two".to_string()),
                ("b".to_string(), "second tool".to_string()),
            ],
            "one frame per tool, in arrival order"
        );
        // A snapshot replaces what the same window accumulated for that tool.
        buf.push("a", "stale", true);
        buf.push("a", "fresh", false);
        assert_eq!(buf.take(), vec![("a".to_string(), "fresh".to_string())]);
        // The TAIL survives the cap, cut on a char boundary.
        buf.push("c", &"é".repeat(TOOL_PROGRESS_CAP), true);
        buf.push("c", "tail", true);
        let (id, text) = buf.take().into_iter().next().unwrap();
        assert_eq!(id, "c");
        assert!(text.len() <= TOOL_PROGRESS_CAP);
        assert!(text.ends_with("tail"));
    }

    #[tokio::test]
    async fn tool_progress_is_broadcast_but_never_journaled() {
        let (_dir, sessions) = test_engine();
        let chat = "chat-ephemeral";
        let (_replay, mut rx) = sessions.subscribe(chat, 0).unwrap();
        sessions.inner.publish(
            chat,
            &AgentEvent::ToolProgress {
                id: "call-1".into(),
                text: "compiling".into(),
                append: true,
            },
        );
        sessions.inner.publish(
            chat,
            &AgentEvent::TextDelta {
                text: "durable".into(),
            },
        );
        // Live frames reach subscribers with no durable position…
        let frame = rx.recv().await.unwrap();
        assert!(matches!(frame.event, AgentEvent::ToolProgress { .. }));
        assert_eq!(frame.seq, 0);
        // …and the journal holds only the durable event.
        assert!(matches!(
            rx.recv().await.unwrap().event,
            AgentEvent::TextDelta { .. }
        ));
        assert_eq!(
            sessions.latest_seq(chat),
            1,
            "a progress frame must never add a journal line"
        );
    }

    #[test]
    fn phase_change_names_only_what_changed() {
        // The four waits a user can be in, in the order a run actually visits
        // them, and the rule that a streamed turn yields ONE frame per change.
        assert_eq!(
            phase_change(
                &AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: "m".into(),
                    tools: vec![],
                    cwd: "/w".into(),
                    session_id: "s".into(),
                    assistant_message_id: "a".into(),
                },
                Some(RunPhase::Starting),
            ),
            Some(RunPhase::WaitingModel),
            "a harness that started is no longer a harness that is starting"
        );
        let started = Some(RunPhase::WaitingModel);
        assert_eq!(
            phase_change(&AgentEvent::TextDelta { text: "hi".into() }, started),
            Some(RunPhase::Streaming)
        );
        assert_eq!(
            phase_change(
                &AgentEvent::TextDelta {
                    text: " there".into()
                },
                Some(RunPhase::Streaming)
            ),
            None,
            "one frame per phase change, not one per token"
        );
        assert_eq!(
            phase_change(
                &AgentEvent::ReasoningDelta {
                    text: String::new()
                },
                Some(RunPhase::Streaming),
            ),
            None,
            "an empty delta changes nothing"
        );
        assert_eq!(
            phase_change(
                &AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::ReadFile {
                        path: "/w/x".into()
                    },
                },
                Some(RunPhase::Streaming),
            ),
            Some(RunPhase::Tool)
        );
        // Nothing invents a phase for a signal the harness never sent.
        assert_eq!(
            phase_change(
                &AgentEvent::Usage {
                    input_tokens: 1,
                    cached_input_tokens: 0,
                    output_tokens: 1,
                    reasoning_tokens: 0,
                    context_limit: None,
                },
                Some(RunPhase::Starting),
            ),
            None,
            "a silent harness stays on the phase it was dispatched with"
        );
    }

    #[tokio::test]
    async fn run_phase_frames_are_broadcast_but_never_journaled() {
        let (_dir, sessions) = test_engine();
        let chat = "chat-phase";
        let (_replay, mut rx) = sessions.subscribe(chat, 0).unwrap();
        let mut phase = Some(RunPhase::Starting);
        // Exactly what `drive_run` does per harness event, in order.
        let events = [
            AgentEvent::SessionStarted {
                harness: HarnessId::ClaudeCode,
                model: "m".into(),
                tools: vec![],
                cwd: "/w".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
            AgentEvent::TextDelta { text: "hi".into() },
            AgentEvent::TextDelta {
                text: " there".into(),
            },
            AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::ReadFile {
                    path: "/w/x".into(),
                },
            },
        ];
        for event in events {
            sessions.inner.publish(&chat, &event);
            if let Some(next) = phase_change(&event, phase) {
                phase = Some(next);
                sessions
                    .inner
                    .broadcast_ephemeral(&chat, AgentEvent::RunPhase { phase: next });
            }
        }
        let mut seen = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let AgentEvent::RunPhase { phase } = frame.event {
                assert_eq!(frame.seq, 0, "a phase frame has no durable position");
                seen.push(phase);
            }
        }
        assert_eq!(
            seen,
            vec![RunPhase::WaitingModel, RunPhase::Streaming, RunPhase::Tool],
            "the trailer's phases, one frame per change"
        );
        assert_eq!(
            sessions.latest_seq(chat),
            4,
            "three durable events plus… exactly those four: no phase row in the journal"
        );
    }

    fn test_engine() -> (tempfile::TempDir, SessionsEngine) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(RunJournal::open(dir.path()).unwrap());
        let registry = Arc::new(HarnessRegistry::new());
        (
            dir,
            SessionsEngine::new_for_tests("dev".into(), journal, registry),
        )
    }

    #[test]
    fn resume_session_is_never_returned_to_a_different_harness_or_cwd() {
        let (_dir, sessions) = test_engine();
        lock(&sessions.inner.harness_sessions).insert(
            "chat-1".into(),
            HarnessSessionRef {
                harness: HarnessId::ClaudeCode,
                session_id: "claude-session".into(),
                cwd: "/workspace".into(),
            },
        );
        assert_eq!(
            sessions
                .inner
                .resume_for("chat-1", "/workspace", HarnessId::ClaudeCode)
                .as_deref(),
            Some("claude-session")
        );
        assert!(
            sessions
                .inner
                .resume_for("chat-1", "/workspace", HarnessId::Devin)
                .is_none()
        );
        assert!(
            sessions
                .inner
                .resume_for("chat-1", "/other", HarnessId::ClaudeCode)
                .is_none()
        );
    }

    #[test]
    fn provider_replay_includes_only_entries_after_the_seen_watermark() {
        let doc = SessionDoc::init("chat-1").unwrap();
        let message = |id: &str, role, text: &str| komet_doc::SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        doc.push_message(&message("user-1", MessageRole::User, "first prompt"))
            .unwrap();
        doc.push_message(&message(
            "assistant-1",
            MessageRole::Assistant,
            "first answer",
        ))
        .unwrap();
        doc.push_message(&message("user-2", MessageRole::User, "second prompt"))
            .unwrap();
        doc.push_message(&message(
            "assistant-2",
            MessageRole::Assistant,
            "second answer",
        ))
        .unwrap();
        doc.push_message(&message("user-3", MessageRole::User, "third prompt"))
            .unwrap();
        let replay = replay_prompt(&doc, Some("assistant-1"), "user-3", "third prompt").unwrap();
        assert!(replay.contains("second prompt"));
        assert!(replay.contains("second answer"));
        assert!(replay.contains("third prompt"));
        assert!(!replay.contains("first prompt"));
        assert!(!replay.contains("first answer"));
    }

    #[test]
    fn dropping_mcp_permission_cleans_up_pending_request_without_interrupting() {
        let (_dir, sessions) = test_engine();
        let pending: PendingPermissionChoices = Arc::default();
        let (resolver, mut receiver) = oneshot::channel();
        let request_id = "permission-1".to_string();
        lock(&pending).insert(request_id.clone(), resolver);
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();

        drop(McpPermissionCancellation {
            pending: pending.clone(),
            engine_tx,
            sessions,
            chat_id: "chat-1".into(),
            request_id: request_id.clone(),
        });

        assert!(!lock(&pending).contains_key(&request_id));
        assert!(receiver.try_recv().is_err());
        assert!(matches!(
            engine_rx.try_recv(),
            Ok(AgentEvent::PermissionResolved {
                request_id: resolved_id,
                choice: komet_proto::PermissionChoice::Deny,
            }) if resolved_id == request_id
        ));
    }

    fn native_window(used: u64, input: u64, output: u64) -> AgentEvent {
        AgentEvent::ContextWindow {
            stats: ContextUsageStats::window(
                used,
                128_000,
                ContextUsageSource::Native,
                input,
                0,
                output,
                0,
            ),
        }
    }

    fn turn_usage(input: u64, output: u64) -> AgentEvent {
        AgentEvent::Usage {
            input_tokens: input,
            cached_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: 0,
            context_limit: Some(128_000),
        }
    }

    #[test]
    fn context_window_replaces_gauge_and_usage_does_not_accumulate() {
        let (_dir, engine) = test_engine();
        let chat = "chat-1";
        engine.set_status(chat, SessionStatus::Working, true);

        engine
            .inner
            .publish(chat, &native_window(10_000, 10_000, 500));
        let first = engine.usage_for(chat).expect("native snapshot");
        assert_eq!(first.used(), 10_000);
        assert_eq!(first.source, ContextUsageSource::Native);
        assert_eq!(
            engine
                .session_status(chat)
                .and_then(|s| s.context_usage)
                .map(|s| s.used()),
            Some(10_000),
            "WatchSessions row carries the snapshot"
        );

        engine.inner.publish(chat, &turn_usage(3_000, 100));
        let after_turn = engine.usage_for(chat).unwrap();
        assert_eq!(after_turn.used(), 10_000);
        assert_eq!(after_turn.input_tokens, 3_000);
        assert_eq!(after_turn.output_tokens, 100);
        assert_eq!(after_turn.source, ContextUsageSource::Native);

        engine
            .inner
            .publish(chat, &native_window(20_000, 19_000, 1_000));
        let second = engine.usage_for(chat).unwrap();
        assert_eq!(second.used(), 20_000);
        assert_eq!(second.input_tokens, 19_000);
        assert_eq!(
            engine
                .watch_sessions()
                .borrow()
                .iter()
                .find(|s| s.chat_id == chat)
                .and_then(|s| s.context_usage.as_ref())
                .map(|s| s.used()),
            Some(20_000),
        );
    }

    #[test]
    fn approximate_usage_does_not_overwrite_native_window() {
        let (_dir, engine) = test_engine();
        let chat = "chat-2";
        engine.set_status(chat, SessionStatus::Working, true);
        engine.inner.publish(chat, &native_window(9_999, 9_999, 0));
        engine.inner.publish(
            chat,
            &AgentEvent::ContextWindow {
                stats: ContextUsageStats::window(
                    3_000,
                    0,
                    ContextUsageSource::Approximate,
                    3_000,
                    0,
                    40,
                    0,
                ),
            },
        );
        let stats = engine.usage_for(chat).unwrap();
        assert_eq!(stats.used(), 9_999);
        assert_eq!(stats.source, ContextUsageSource::Native);
        assert_eq!(stats.input_tokens, 9_999);
    }

    #[test]
    fn billed_prompt_overshoot_is_capped_for_every_source() {
        let (_dir, engine) = test_engine();
        let chat = "chat-overshoot";
        engine.set_status(chat, SessionStatus::Working, true);

        engine.inner.publish(
            chat,
            &AgentEvent::SessionStarted {
                harness: HarnessId::Grok,
                model: "grok-4.5".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "s".into(),
                assistant_message_id: "a".into(),
            },
        );
        engine.inner.publish(
            chat,
            &AgentEvent::Usage {
                input_tokens: 3_000_000,
                cached_input_tokens: 0,
                output_tokens: 18_000,
                reasoning_tokens: 0,
                context_limit: None,
            },
        );
        let stats = engine.usage_for(chat).unwrap();
        assert_eq!(stats.context_limit, 131_072);
        assert_eq!(stats.used(), 0);
        assert_eq!(stats.input_tokens, 3_000_000);
        assert_eq!(stats.source, ContextUsageSource::Approximate);

        let chat_native = "chat-native-overshoot";
        engine.set_status(chat_native, SessionStatus::Working, true);
        engine.inner.publish(
            chat_native,
            &AgentEvent::ContextWindow {
                stats: ContextUsageStats::window(
                    3_000_000,
                    500_000,
                    ContextUsageSource::Native,
                    3_000_000,
                    0,
                    0,
                    0,
                ),
            },
        );
        let native = engine.usage_for(chat_native).unwrap();
        assert_eq!(native.used(), 500_000);
        assert_eq!(native.input_tokens, 3_000_000);
        assert_eq!(native.source, ContextUsageSource::Native);
    }

    /// Send-path sync (models-refresh T5): a catalog with a single offered
    /// model; dispatches name whatever they want.
    struct StaticCatalogHarness;

    #[async_trait::async_trait]
    impl Harness for StaticCatalogHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Codex
        }
        fn display_name(&self) -> &str {
            "Static"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> komet_proto::SteeringMode {
            komet_proto::SteeringMode::StepBoundary
        }
        fn reasoning_levels(&self) -> &[komet_proto::ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<komet_proto::Model>, komet_harness::HarnessError> {
            Ok(vec![komet_proto::Model {
                id: "offered-1".into(),
                label: "Offered 1".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            }])
        }
        async fn run(
            &self,
            _request: RunRequest,
            _controls: RunControls,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<AgentEvent, komet_harness::HarnessError>>,
            komet_harness::HarnessError,
        > {
            Ok(futures::stream::empty().boxed())
        }
    }

    fn catalog_engine() -> (tempfile::TempDir, SessionsEngine) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(RunJournal::open(dir.path()).unwrap());
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(Arc::new(StaticCatalogHarness));
        (
            dir,
            SessionsEngine::new_for_tests("dev".into(), journal, registry),
        )
    }

    fn catalog_probe_request(model: Option<&str>) -> RunRequest {
        RunRequest {
            prompt: "hi".into(),
            harness: None,
            model: model.map(str::to_string),
            reasoning: None,
            model_options: Default::default(),
            cwd: "/tmp".into(),
            sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
            sandbox_options: None,
            auto_approve: false,
            resume: None,
            attachments: Vec::new(),
            worktree: None,
            permission_timeout_ms: None,
            mcp: None,
            mcp_external: Vec::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_rejects_model_id_missing_from_live_catalog() {
        let (_dir, engine) = catalog_engine();
        // A model the live CLI no longer offers never dispatches: the
        // rejection names the id and points at Rescan.
        let err = engine
            .dispatch(
                "chat-stale-model",
                HarnessId::Codex,
                catalog_probe_request(Some("gone-1")),
                None,
            )
            .await
            .expect_err("model outside the live catalog must not dispatch");
        let message = err.to_string();
        assert!(
            message.contains("gone-1")
                && message.contains("not offered")
                && message.contains("Rescan"),
            "readable rescan rejection, got: {message}"
        );
        // The offered id passes the gate (it then fails on the unwired doc
        // host — proof the gate, not the plumbing, rejected the stale id).
        let err = engine
            .dispatch(
                "chat-offered-model",
                HarnessId::Codex,
                catalog_probe_request(Some("offered-1")),
                None,
            )
            .await
            .expect_err("no doc host wired in unit tests");
        assert!(
            !err.to_string().contains("not offered"),
            "offered id must pass the model gate, got: {err}"
        );
        // No model (or an empty one) always passes the gate.
        for model in [None, Some("")] {
            let err = engine
                .dispatch(
                    "chat-no-model",
                    HarnessId::Codex,
                    catalog_probe_request(model),
                    None,
                )
                .await
                .expect_err("no doc host wired in unit tests");
            assert!(
                !err.to_string().contains("not offered"),
                "model-less run must pass the model gate, got: {err}"
            );
        }
    }
}
