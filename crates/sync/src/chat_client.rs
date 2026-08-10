//! ChatClient — WebSocket transport for chat2 rooms (docs/chat2-sync.md C1):
//! hello/state handshake with client-side checkpoint precision, cursor-based
//! row backfill, push/ack with a pending-unacked queue, opaque presence
//! relay, probe/redial liveness, and reconnect with exponential backoff.
//!
//! The client owns no CRDT semantics: update bytes flow through a
//! [`ChatDocSink`] the engine implements over its `ChatDocHandle` (import +
//! persist doc AND cursor in one transaction — the C2 rule). Wire frames are
//! the binary chat2 codec ([`crate::chat_frames`]), byte-compatible with
//! `edge/src/chat-frames.ts`.
//!
//! Liveness discipline is inherited from `registry.rs` and its incidents:
//! transport pings prove nothing about the DO; room health is judged only by
//! protocol frames with probe deadlines.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::chat_frames::{self as wire, frame_type};
use crate::room::{StaticUrl, SyncError, UrlProvider};

const PING_INTERVAL: Duration = Duration::from_secs(15);
const SILENCE_LEASE: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HELLO_DEADLINE: Duration = Duration::from_secs(15);
/// Backfill after hello must complete (rowsDone) within this deadline —
/// post-strip rooms are KB-scale, so this is generous even at 1.2 Mbps.
const BACKFILL_DEADLINE: Duration = Duration::from_secs(120);
const PROBE_DEADLINE: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Quiet-room probe cadence default (matches the registry's fleet math).
const PROBE_QUIET_DEFAULT: Duration = Duration::from_secs(900);

/// Per-client tuning.
#[derive(Clone, Copy, Debug)]
pub struct ChatTuning {
    pub probe_quiet: Duration,
}

impl Default for ChatTuning {
    fn default() -> Self {
        Self {
            probe_quiet: PROBE_QUIET_DEFAULT,
        }
    }
}

/// Connection/sync lifecycle notifications (best-effort broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatEvent {
    /// Joined (or re-joined); the hello state has been received.
    Connected,
    /// Backfill finished — the doc is converged with the room at this head.
    CaughtUp { head_seq: u64 },
    /// Remote rows/acks were applied through the sink — republish.
    Applied,
    /// The connection dropped; the client is backing off before redialing.
    Disconnected,
    /// A remote device's presence beat arrived.
    Presence,
}

// ── engine-facing traits ────────────────────────────────────────────────────

/// Where remote bytes land. The engine implements this over its doc handle;
/// every method persists doc content AND the room cursor in one transaction
/// (`DocsStore::save_snapshot_with_cursor`) so they can never diverge.
pub trait ChatDocSink: Send + Sync + 'static {
    /// Import one remote update row; `cursor` is the row's seq.
    fn apply_row(&self, bytes: &[u8], cursor: u64);
    /// Replace/merge from a checkpoint blob; `cursor` is its checkpointSeq.
    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String>;
    /// Client-side precision (replaces the server VV diff): is the server
    /// checkpoint's frontier already contained in the local doc?
    fn contains_frontier(&self, frontier: &[u8]) -> bool;
    /// An own-write ack advanced the cursor with no content change.
    fn advance_cursor(&self, cursor: u64);
}

/// `GET /chat2/{chatId}/checkpoint` over HTTP. Implementations should resume
/// partial downloads with `Range: bytes=N-` (the DO serves 206) — that
/// resumability is the point of checkpoint-over-HTTP vs export-per-join.
pub trait CheckpointFetcher: Send + Sync + 'static {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>>;
}

// ── catch-up planning (pure — the client-side precision rule) ───────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUpPlan {
    /// Local doc already contains the checkpoint frontier (or there is no
    /// checkpoint): stream rows only.
    RowsOnly { after: u64 },
    /// Fetch + import the checkpoint first, then rows after it.
    CheckpointThenRows { after: u64 },
}

/// Decide the catch-up path from the hello state. `frontier_contained` is the
/// sink's verdict on the checkpoint frontier payload.
pub fn plan_catch_up(
    cursor: u64,
    state: &wire::StateHeader,
    frontier_contained: bool,
) -> CatchUpPlan {
    // A cursor ahead of the server means the server lost state (reset/wipe);
    // our cursor is meaningless there — treat as fresh.
    let cursor = if cursor > state.head_seq { 0 } else { cursor };
    if state.checkpoint_seq == 0 {
        return CatchUpPlan::RowsOnly { after: cursor };
    }
    if frontier_contained {
        // Rows ≤ checkpointSeq are covered by a checkpoint we already
        // contain — skip straight past them even if our cursor is older.
        CatchUpPlan::RowsOnly {
            after: cursor.max(state.checkpoint_seq),
        }
    } else {
        CatchUpPlan::CheckpointThenRows {
            after: state.checkpoint_seq,
        }
    }
}

// ── transport plumbing (binary sibling of registry.rs's TextPipe) ───────────

pub(crate) struct BinPipe {
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
    pub(crate) rx: mpsc::Receiver<Vec<u8>>,
}

pub(crate) trait BinConnector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>>;
}

struct WsBinConnector {
    url: Arc<dyn UrlProvider>,
}

impl BinConnector for WsBinConnector {
    fn connect(&self) -> BoxFuture<'static, Result<BinPipe, SyncError>> {
        let provider = self.url.clone();
        Box::pin(async move {
            let url = provider.url().await?;
            let (ws, _) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            tokio::spawn(pump(ws, out_rx, in_tx));
            Ok(BinPipe {
                tx: out_tx,
                rx: in_rx,
            })
        })
    }
}

/// Shuttle binary frames between the WebSocket and the actor's channels; the
/// text `"ping"` keepalive rides the same socket (runtime-answered pair).
async fn pump(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    last_rx = tokio::time::Instant::now();
                    if in_tx.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {
                    // Text pong / control frames: transport liveness only.
                    last_rx = tokio::time::Instant::now();
                }
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                tracing::warn!("chat2 socket silent past lease; treating as dead");
                break;
            }
        }
    }
}

// ── shared client state ─────────────────────────────────────────────────────

struct PendingPush {
    batch_id: String,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    cursor: u64,
    pending: VecDeque<PendingPush>,
    /// Last hello/probe view of the server log (checkpoint-policy inputs).
    server: Option<wire::StateHeader>,
}

/// `komet sync` surface (plan: cursor / headSeq / floorLag / pendingPushes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChatStatsSnapshot {
    pub connected: bool,
    pub cursor: u64,
    pub head_seq: u64,
    pub seq_floor: u64,
    pub checkpoint_seq: u64,
    pub row_count: u64,
    pub row_bytes: u64,
    pub pending_pushes: u64,
    pub rejoins: u64,
    pub disconnects: u64,
    pub rejected: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the client ──────────────────────────────────────────────────────────────

/// A live chat2-room membership for one chat doc.
pub struct ChatClient {
    shared: Arc<Mutex<Shared>>,
    events: broadcast::Sender<ChatEvent>,
    shutdown: watch::Sender<bool>,
    nudge: mpsc::Sender<()>,
    probe: mpsc::Sender<()>,
    redial: mpsc::Sender<()>,
    presence_out: mpsc::Sender<(i64, Vec<u8>)>,
    flags: Arc<Flags>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct Flags {
    connected: std::sync::atomic::AtomicBool,
    rejoins: std::sync::atomic::AtomicU64,
    disconnects: std::sync::atomic::AtomicU64,
    rejected: std::sync::atomic::AtomicU64,
}

impl ChatClient {
    /// Connect (fixed URL — dev/tests).
    pub async fn connect(
        url: &str,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
    ) -> Result<Self, SyncError> {
        Self::connect_via(
            Arc::new(StaticUrl(url.to_string())),
            sink,
            fetcher,
            device_id,
            initial_cursor,
        )
        .await
    }

    /// Connect with a per-dial URL provider (fresh `?token=` every attempt).
    /// Resolves once hello/state lands AND the initial catch-up (checkpoint
    /// if needed + row backfill) completes; first-attempt failures are `Err`
    /// (callers own the initial-join retry). After that it reconnects itself.
    pub async fn connect_via(
        provider: Arc<dyn UrlProvider>,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
    ) -> Result<Self, SyncError> {
        let connector = Arc::new(WsBinConnector { url: provider });
        Self::connect_with_tuned(
            connector,
            sink,
            fetcher,
            device_id,
            initial_cursor,
            ChatTuning::default(),
        )
        .await
    }

    pub(crate) async fn connect_with_tuned(
        connector: Arc<dyn BinConnector>,
        sink: Arc<dyn ChatDocSink>,
        fetcher: Arc<dyn CheckpointFetcher>,
        device_id: &str,
        initial_cursor: u64,
        tuning: ChatTuning,
    ) -> Result<Self, SyncError> {
        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (nudge_tx, nudge_rx) = mpsc::channel(1);
        let (probe_tx, probe_rx) = mpsc::channel(1);
        let (redial_tx, redial_rx) = mpsc::channel(1);
        let (presence_tx, presence_rx) = mpsc::channel(4);
        let shared = Arc::new(Mutex::new(Shared {
            cursor: initial_cursor,
            ..Shared::default()
        }));
        let flags = Arc::new(Flags::default());

        let actor = Actor {
            shared: shared.clone(),
            sink,
            fetcher,
            device_id: device_id.to_string(),
            connector,
            tuning,
            events: events.clone(),
            shutdown: shutdown_rx,
            nudge_rx,
            probe_rx,
            redial_rx,
            presence_rx,
            flags: flags.clone(),
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                shared,
                events,
                shutdown: shutdown_tx,
                nudge: nudge_tx,
                probe: probe_tx,
                redial: redial_tx,
                presence_out: presence_tx,
                flags,
                task: Some(task),
            }),
            Ok(Err(err)) => {
                task.abort();
                Err(err)
            }
            Err(_) => {
                task.abort();
                Err(SyncError::Closed)
            }
        }
    }

    pub fn events(&self) -> broadcast::Receiver<ChatEvent> {
        self.events.subscribe()
    }

    /// Queue one local update batch for push (a fresh batch id is minted; the
    /// batch survives reconnects until acked — the server dedupes replays).
    pub fn enqueue_update(&self, bytes: Vec<u8>) {
        {
            let mut shared = lock(&self.shared);
            shared.pending.push_back(PendingPush {
                batch_id: uuid::Uuid::new_v4().to_string(),
                bytes,
            });
        }
        let _ = self.nudge.try_send(());
    }

    /// Publish this device's presence beat with an opaque payload (cursor
    /// positions etc. — relayed verbatim, never stored).
    pub fn send_presence(&self, at: i64, payload: Vec<u8>) {
        let _ = self.presence_out.try_send((at, payload));
    }

    /// Liveness hint: probe the room now (deadline-checked).
    pub fn probe(&self) {
        let _ = self.probe.try_send(());
    }

    /// Escalation: tear the session down and dial a fresh socket.
    pub fn redial(&self) {
        let _ = self.redial.try_send(());
    }

    pub fn stats(&self) -> ChatStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        let shared = lock(&self.shared);
        let server = shared.server.unwrap_or(wire::StateHeader {
            head_seq: 0,
            seq_floor: 0,
            checkpoint_seq: 0,
            checkpoint_size: 0,
            row_count: 0,
            row_bytes: 0,
        });
        ChatStatsSnapshot {
            connected: self.flags.connected.load(Relaxed),
            cursor: shared.cursor,
            head_seq: server.head_seq.max(shared.cursor),
            seq_floor: server.seq_floor,
            checkpoint_seq: server.checkpoint_seq,
            row_count: server.row_count,
            row_bytes: server.row_bytes,
            pending_pushes: shared.pending.len() as u64,
            rejoins: self.flags.rejoins.load(Relaxed),
            disconnects: self.flags.disconnects.load(Relaxed),
            rejected: self.flags.rejected.load(Relaxed),
        }
    }

    /// Leave cleanly and stop the actor.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ChatClient {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// ── the actor ───────────────────────────────────────────────────────────────

struct Actor {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn ChatDocSink>,
    fetcher: Arc<dyn CheckpointFetcher>,
    device_id: String,
    connector: Arc<dyn BinConnector>,
    tuning: ChatTuning,
    events: broadcast::Sender<ChatEvent>,
    shutdown: watch::Receiver<bool>,
    nudge_rx: mpsc::Receiver<()>,
    probe_rx: mpsc::Receiver<()>,
    redial_rx: mpsc::Receiver<()>,
    presence_rx: mpsc::Receiver<(i64, Vec<u8>)>,
    flags: Arc<Flags>,
}

enum SessionEnd {
    Reconnect,
    Stop,
}

impl Actor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let dial = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect()).await;
            let pipe = match dial {
                Ok(Ok(pipe)) => pipe,
                Ok(Err(err)) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(err));
                        return; // first join failed: caller owns the retry
                    }
                    tracing::warn!(error = %err, "chat2 dial failed; backing off");
                    if self.sleep_or_shutdown(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                    continue;
                }
                Err(_) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(SyncError::WebSocket("connect timeout".into())));
                        return;
                    }
                    tracing::warn!("chat2 dial timed out; backing off");
                    if self.sleep_or_shutdown(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                    continue;
                }
            };

            match self.run_session(pipe, &mut ready).await {
                SessionEnd::Stop => return,
                SessionEnd::Reconnect => {
                    use std::sync::atomic::Ordering::Relaxed;
                    self.flags.connected.store(false, Relaxed);
                    self.flags.disconnects.fetch_add(1, Relaxed);
                    let _ = self.events.send(ChatEvent::Disconnected);
                    if ready.is_some() {
                        if let Some(ready) = ready.take() {
                            let _ =
                                ready.send(Err(SyncError::Protocol("chat2 handshake failed".into())));
                        }
                        return;
                    }
                    if self.sleep_or_shutdown(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
        }
    }

    /// True = shutdown observed.
    async fn sleep_or_shutdown(&mut self, wait: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(wait) => false,
            _ = self.shutdown.changed() => *self.shutdown.borrow(),
        }
    }

    async fn run_session(
        &mut self,
        mut pipe: BinPipe,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> SessionEnd {
        use std::sync::atomic::Ordering::Relaxed;

        // ── hello / state ───────────────────────────────────────────────────
        let cursor = lock(&self.shared).cursor;
        let hello = wire::encode(
            frame_type::HELLO,
            &wire::HelloHeader {
                cursor,
                device: &self.device_id,
            },
            &[],
        );
        if pipe.tx.send(hello).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let state = tokio::time::timeout(HELLO_DEADLINE, async {
            loop {
                let bytes = pipe.rx.recv().await?;
                let Some(frame) = wire::decode(&bytes) else {
                    tracing::warn!("chat2: bad frame during handshake");
                    return None;
                };
                if frame.kind == frame_type::STATE {
                    return Some(frame);
                }
                // Stale broadcast before our state: skip.
            }
        })
        .await;
        let Ok(Some(state_frame)) = state else {
            tracing::warn!("chat2: no state frame within deadline");
            return SessionEnd::Reconnect;
        };
        let Ok(state) = serde_json::from_value::<wire::StateHeader>(state_frame.header.clone())
        else {
            tracing::warn!("chat2: malformed state header");
            return SessionEnd::Reconnect;
        };
        lock(&self.shared).server = Some(state);
        self.flags.connected.store(true, Relaxed);
        if ready.is_none() {
            self.flags.rejoins.fetch_add(1, Relaxed);
        }
        let _ = self.events.send(ChatEvent::Connected);

        // ── catch-up: checkpoint precision + row backfill ───────────────────
        let contained =
            state.checkpoint_seq == 0 || self.sink.contains_frontier(&state_frame.payload);
        let plan = plan_catch_up(cursor, &state, contained);
        let after = match plan {
            CatchUpPlan::RowsOnly { after } => after,
            CatchUpPlan::CheckpointThenRows { after } => {
                tracing::info!(
                    checkpoint_seq = state.checkpoint_seq,
                    checkpoint_size = state.checkpoint_size,
                    "chat2: fetching checkpoint"
                );
                let bytes = match self.fetcher.fetch().await {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        tracing::warn!(error = %err, "chat2: checkpoint fetch failed");
                        return SessionEnd::Reconnect;
                    }
                };
                if let Err(err) = self.sink.apply_checkpoint(&bytes, state.checkpoint_seq) {
                    tracing::warn!(error = %err, "chat2: checkpoint import failed");
                    return SessionEnd::Reconnect;
                }
                let mut shared = lock(&self.shared);
                shared.cursor = shared.cursor.max(state.checkpoint_seq);
                drop(shared);
                let _ = self.events.send(ChatEvent::Applied);
                after
            }
        };
        // Clamp the persisted cursor into the room's honest range (server
        // reset detection happened in plan_catch_up via after==0).
        if after < cursor {
            lock(&self.shared).cursor = after;
        }
        let rows_req = wire::encode(
            frame_type::ROWS_REQ,
            &wire::RowsReqHeader {
                after,
                exclude_own: true,
            },
            &[],
        );
        if pipe.tx.send(rows_req).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let backfill = tokio::time::timeout(BACKFILL_DEADLINE, async {
            loop {
                let bytes = pipe.rx.recv().await?;
                let Some(frame) = wire::decode(&bytes) else {
                    return None;
                };
                match frame.kind {
                    frame_type::ROWS_DONE => {
                        let done: wire::RowsDoneHeader =
                            serde_json::from_value(frame.header).ok()?;
                        return Some(done.head_seq);
                    }
                    _ => {
                        if !self.handle_frame(frame) {
                            return None;
                        }
                    }
                }
            }
        })
        .await;
        let Ok(Some(head_seq)) = backfill else {
            tracing::warn!("chat2: backfill did not complete");
            return SessionEnd::Reconnect;
        };
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let _ = self.events.send(ChatEvent::CaughtUp { head_seq });

        // Anything pending (offline writes, reconnect re-pushes) goes now —
        // the server's batchId dedupe makes replays exact no-ops.
        if !self.push_pending(&mut pipe).await {
            return SessionEnd::Reconnect;
        }

        // ── steady state ────────────────────────────────────────────────────
        let mut last_frame = tokio::time::Instant::now();
        let mut probe_deadline: Option<tokio::time::Instant> = None;
        loop {
            let quiet_probe_at = last_frame + self.tuning.probe_quiet;
            let deadline_at = probe_deadline
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                frame = pipe.rx.recv() => {
                    let Some(bytes) = frame else {
                        return SessionEnd::Reconnect;
                    };
                    last_frame = tokio::time::Instant::now();
                    probe_deadline = None;
                    let Some(frame) = wire::decode(&bytes) else {
                        tracing::warn!("chat2: unparseable frame");
                        return SessionEnd::Reconnect;
                    };
                    if !self.handle_frame(frame) {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.nudge_rx.recv() => {
                    if !self.push_pending(&mut pipe).await {
                        return SessionEnd::Reconnect;
                    }
                }
                beat = self.presence_rx.recv() => {
                    if let Some((at, payload)) = beat {
                        let frame = wire::encode(
                            frame_type::PRESENCE,
                            &wire::PresenceOutHeader { at },
                            &payload,
                        );
                        if pipe.tx.send(frame).await.is_err() {
                            return SessionEnd::Reconnect;
                        }
                    }
                }
                _ = self.probe_rx.recv() => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.redial_rx.recv() => {
                    tracing::info!("chat2: redial requested");
                    return SessionEnd::Reconnect;
                }
                _ = tokio::time::sleep_until(quiet_probe_at) => {
                    if !self.send_probe(&mut pipe, &mut probe_deadline).await {
                        return SessionEnd::Reconnect;
                    }
                    last_frame = tokio::time::Instant::now();
                }
                _ = tokio::time::sleep_until(deadline_at) => {
                    tracing::warn!("chat2: probe unanswered past deadline; redialing");
                    return SessionEnd::Reconnect;
                }
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        return SessionEnd::Stop;
                    }
                }
            }
        }
    }

    async fn send_probe(
        &self,
        pipe: &mut BinPipe,
        probe_deadline: &mut Option<tokio::time::Instant>,
    ) -> bool {
        let frame = wire::encode(frame_type::PROBE, &serde_json::json!({}), &[]);
        if pipe.tx.send(frame).await.is_err() {
            return false;
        }
        if probe_deadline.is_none() {
            *probe_deadline = Some(tokio::time::Instant::now() + PROBE_DEADLINE);
        }
        true
    }

    async fn push_pending(&self, pipe: &mut BinPipe) -> bool {
        // Clone rather than drain: batches stay queued until their ack.
        let frames: Vec<Vec<u8>> = lock(&self.shared)
            .pending
            .iter()
            .map(|push| {
                wire::encode(
                    frame_type::PUSH,
                    &wire::PushHeader {
                        batch_id: &push.batch_id,
                    },
                    &push.bytes,
                )
            })
            .collect();
        for frame in frames {
            if pipe.tx.send(frame).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Apply one inbound protocol frame. False = protocol breakdown, redial.
    fn handle_frame(&self, frame: wire::WireFrame) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        match frame.kind {
            frame_type::ROW => {
                let Ok(row) = serde_json::from_value::<wire::RowHeader>(frame.header) else {
                    return false;
                };
                // Own-device rows can still arrive (live relay of a racing
                // second socket, or a server that ignored excludeOwn) — Loro
                // re-import is a no-op; the cursor advance is what matters.
                self.sink.apply_row(&frame.payload, row.seq);
                let mut shared = lock(&self.shared);
                shared.cursor = shared.cursor.max(row.seq);
                drop(shared);
                let _ = self.events.send(ChatEvent::Applied);
            }
            frame_type::ACK => {
                let Ok(ack) = serde_json::from_value::<wire::AckHeader>(frame.header) else {
                    return false;
                };
                let mut shared = lock(&self.shared);
                shared.pending.retain(|p| p.batch_id != ack.batch_id);
                shared.cursor = shared.cursor.max(ack.seq);
                let cursor = shared.cursor;
                drop(shared);
                self.sink.advance_cursor(cursor);
                let _ = self.events.send(ChatEvent::Applied);
            }
            frame_type::PRESENCE => {
                let _ = self.events.send(ChatEvent::Presence);
            }
            frame_type::PROBE_OK => {
                if let Ok(probe) = serde_json::from_value::<wire::ProbeOkHeader>(frame.header) {
                    if let Some(server) = &mut lock(&self.shared).server {
                        server.head_seq = server.head_seq.max(probe.head_seq);
                    }
                }
            }
            frame_type::STATE => {
                // Late duplicate of a hello answer — refresh the server view.
                if let Ok(state) = serde_json::from_value::<wire::StateHeader>(frame.header) {
                    lock(&self.shared).server = Some(state);
                }
            }
            frame_type::ERROR => {
                self.flags.rejected.fetch_add(1, Relaxed);
                let code = frame.header["code"].as_str().unwrap_or("?").to_string();
                let message = frame.header["message"].as_str().unwrap_or("").to_string();
                tracing::warn!(code, message, "chat2: server rejected a frame");
            }
            other => {
                // Unknown server frame: tolerate (future protocol additions).
                tracing::debug!(kind = other, "chat2: ignoring unknown frame type");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests;
