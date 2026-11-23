//! RegistryClient — WebSocket transport for the workspace registry
//! (docs/sync.md): hello/cursor handshake, push/ack for pending op
//! batches, merged-row broadcasts, presence beats, probe/redial liveness, and
//! reconnect with exponential backoff.
//!
//! The client owns no row semantics: everything applies through the shared
//! [`komet_doc::RegistryDoc`] under a lock. Wire frames are JSON text —
//! byte-compatible with `shared protocol behavior`.
//!
//! Liveness discipline is inherited from `room.rs` and its incidents: the
//! transport-level text ping elicits a runtime auto-pong that proves NOTHING
//! about the DO (2026-07-30), so room-level health is judged only by protocol
//! frames — a probe that goes unanswered past its deadline tears the session
//! down for a fresh dial.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use komet_doc::{PendingBatch, RegistryDoc, RegistryRow, StateOutcome};

use crate::transport::{
    self, Connector, HELLO_DEADLINE, Liveness, Pipe, Session, SessionEnd, Signals,
};
use crate::types::{RoomStatsSnapshot, StaticUrl, SyncError, UrlProvider};

/// Presence entries older than this are treated as expired (mirrors the
/// EphemeralStore's 30s TTL the old workspace room used).
const PRESENCE_TTL: Duration = Duration::from_secs(30);

/// Per-client tuning.
#[derive(Clone, Copy, Debug)]
pub struct RegistryTuning {
    /// Send a liveness probe after this much protocol-frame silence.
    pub probe_quiet: Duration,
}

impl Default for RegistryTuning {
    fn default() -> Self {
        Self {
            probe_quiet: transport::PROBE_QUIET_DEFAULT,
        }
    }
}

/// Connection/sync lifecycle notifications (best-effort broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryEvent {
    /// Joined (or re-joined); the hello state has been applied.
    Connected,
    /// The connection dropped; the client is backing off before redialing.
    Disconnected,
    /// Rows/acks were applied to the doc — republish and persist.
    Applied,
    /// A remote device's presence beat arrived.
    Presence,
}

// ── wire frames (JSON text; mirror shared protocol behavior) ───────────────

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientFrame<'a> {
    Hello {
        cursor: Option<u64>,
        device: &'a str,
    },
    Push {
        batch: &'a str,
        ops: &'a [komet_doc::RowOp],
    },
    Presence {
        at: i64,
    },
    Probe,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ServerFrame {
    State {
        seq: u64,
        full: bool,
        #[serde(rename = "gcFloor", default)]
        gc_floor: u64,
        rows: Vec<RegistryRow>,
        #[serde(default)]
        presence: HashMap<String, i64>,
    },
    Rows {
        seq: u64,
        rows: Vec<RegistryRow>,
    },
    Ack {
        batch: String,
        seq: u64,
        #[allow(dead_code)]
        applied: u64,
    },
    Presence {
        device: String,
        at: i64,
    },
    #[serde(rename = "probe-ok")]
    ProbeOk {
        #[allow(dead_code)]
        seq: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

// ── transport plumbing (text frames; codec owned by transport.rs) ───────────

pub(crate) type TextPipe = Pipe<String>;

// ── stats (RoomStatsSnapshot-compatible so SyncStatus/`komet sync` render it) ─

#[derive(Default)]
struct Stats {
    connected: std::sync::atomic::AtomicBool,
    last_pushed_ms: std::sync::atomic::AtomicI64,
    last_ack_ms: std::sync::atomic::AtomicI64,
    rejoins: std::sync::atomic::AtomicU64,
    probes: std::sync::atomic::AtomicU64,
    full_resyncs: std::sync::atomic::AtomicU64,
    disconnects: std::sync::atomic::AtomicU64,
    rejected: std::sync::atomic::AtomicU64,
}

impl Stats {
    fn snapshot(&self) -> RoomStatsSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        RoomStatsSnapshot {
            connected: self.connected.load(Relaxed),
            last_pushed_ms: self.last_pushed_ms.load(Relaxed),
            last_ack_ms: self.last_ack_ms.load(Relaxed),
            rejoins: self.rejoins.load(Relaxed),
            probes: self.probes.load(Relaxed),
            full_resyncs: self.full_resyncs.load(Relaxed),
            disconnects: self.disconnects.load(Relaxed),
            rejected: self.rejected.load(Relaxed),
        }
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── the client ──────────────────────────────────────────────────────────────

/// A live registry-room membership for one [`RegistryDoc`].
///
/// Owns a background actor that keeps the doc converged with the room. The
/// engine host mutates the doc under its own lock and calls [`Self::nudge`]
/// to push; server frames apply through the same lock.
pub struct RegistryClient {
    doc: Arc<Mutex<RegistryDoc>>,
    events: broadcast::Sender<RegistryEvent>,
    shutdown: watch::Sender<bool>,
    nudge: mpsc::Sender<()>,
    probe: mpsc::Sender<()>,
    redial: mpsc::Sender<()>,
    presence_out: mpsc::Sender<i64>,
    presence: Arc<Mutex<HashMap<String, (i64, tokio::time::Instant)>>>,
    stats: Arc<Stats>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RegistryClient {
    /// Connect (fixed URL — dev/tests).
    pub async fn connect(
        url: &str,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
    ) -> Result<Self, SyncError> {
        Self::connect_via(Arc::new(StaticUrl(url.to_string())), doc, device_id).await
    }

    /// Connect with a per-dial URL provider (fresh `?token=` every attempt).
    /// Resolves once the initial hello/state handshake lands; a first-attempt
    /// failure is returned as `Err` (callers own the initial-join retry, same
    /// contract as `RoomClient`). After that the client reconnects itself.
    pub async fn connect_via(
        provider: Arc<dyn UrlProvider>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
    ) -> Result<Self, SyncError> {
        Self::connect_via_tuned(provider, doc, device_id, RegistryTuning::default()).await
    }

    pub async fn connect_via_tuned(
        provider: Arc<dyn UrlProvider>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
    ) -> Result<Self, SyncError> {
        let connector: Arc<dyn Connector<String>> = Arc::new(transport::WsConnector::new(provider));
        Self::connect_with_tuned(connector, doc, device_id, tuning).await
    }

    pub(crate) async fn connect_with_tuned(
        connector: Arc<dyn Connector<String>>,
        doc: Arc<Mutex<RegistryDoc>>,
        device_id: &str,
        tuning: RegistryTuning,
    ) -> Result<Self, SyncError> {
        let (events, _) = broadcast::channel(256);
        let (ready_tx, ready_rx) = oneshot::channel();
        let Signals {
            nudge_tx,
            nudge_rx,
            probe_tx,
            probe_rx,
            redial_tx,
            redial_rx,
            presence_tx,
            presence_rx,
            shutdown_tx,
            shutdown_rx,
        } = Signals::new(4);
        let presence = Arc::new(Mutex::new(HashMap::new()));
        let stats = Arc::new(Stats::default());

        let actor = Actor {
            doc: doc.clone(),
            device_id: device_id.to_string(),
            connector,
            tuning,
            events: events.clone(),
            shutdown: shutdown_rx,
            nudge_rx,
            probe_rx,
            redial_rx,
            presence_rx,
            presence: presence.clone(),
            stats: stats.clone(),
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                doc,
                events,
                shutdown: shutdown_tx,
                nudge: nudge_tx,
                probe: probe_tx,
                redial: redial_tx,
                presence_out: presence_tx,
                presence,
                stats,
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

    pub fn doc(&self) -> &Arc<Mutex<RegistryDoc>> {
        &self.doc
    }

    pub fn events(&self) -> broadcast::Receiver<RegistryEvent> {
        self.events.subscribe()
    }

    /// Local writes were enqueued — push pending batches now.
    pub fn nudge(&self) {
        let _ = self.nudge.try_send(());
    }

    /// Publish this device's presence beat (epoch ms).
    pub fn set_presence(&self, at: i64) {
        let _ = self.presence_out.try_send(at);
    }

    /// Remote devices' live presence beats (entries within the 30s TTL),
    /// device → beat epoch ms.
    pub fn presence(&self) -> HashMap<String, i64> {
        let now = tokio::time::Instant::now();
        lock(&self.presence)
            .iter()
            .filter(|(_, (_, seen))| now.duration_since(*seen) < PRESENCE_TTL)
            .map(|(device, (at, _))| (device.clone(), *at))
            .collect()
    }

    /// Liveness hint: probe the room now (deadline-checked).
    pub fn probe(&self) {
        let _ = self.probe.try_send(());
    }

    /// Escalation: tear the session down and dial a fresh socket.
    pub fn redial(&self) {
        let _ = self.redial.try_send(());
    }

    pub fn stats(&self) -> RoomStatsSnapshot {
        self.stats.snapshot()
    }

    /// Leave cleanly and stop the actor.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RegistryClient {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// ── the actor ───────────────────────────────────────────────────────────────

struct Actor {
    doc: Arc<Mutex<RegistryDoc>>,
    device_id: String,
    connector: Arc<dyn Connector<String>>,
    tuning: RegistryTuning,
    events: broadcast::Sender<RegistryEvent>,
    shutdown: watch::Receiver<bool>,
    nudge_rx: mpsc::Receiver<()>,
    probe_rx: mpsc::Receiver<()>,
    redial_rx: mpsc::Receiver<()>,
    presence_rx: mpsc::Receiver<i64>,
    presence: Arc<Mutex<HashMap<String, (i64, tokio::time::Instant)>>>,
    stats: Arc<Stats>,
}

impl Actor {
    async fn run(self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let connector = self.connector.clone();
        let shutdown = self.shutdown.clone();
        transport::run(connector, self, shutdown, ready).await
    }
}

impl Session<String> for Actor {
    async fn run_session<'a>(
        &'a mut self,
        mut pipe: TextPipe,
        ready: &'a mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> SessionEnd {
        use std::sync::atomic::Ordering::Relaxed;

        // System wake: the socket sat through a suspend and is almost certainly
        // half-dead — redial now instead of discovering it by probe deadline.
        let mut wake = crate::wake::subscribe();

        // ── hello / state handshake ─────────────────────────────────────────
        let cursor = {
            let doc = lock(&self.doc);
            let cursor = doc.cursor();
            if cursor == 0 && doc.pending_len() == 0 && doc.generation() == 0 {
                None
            } else {
                Some(cursor)
            }
        };
        let hello = serde_json::to_string(&ClientFrame::Hello {
            cursor,
            device: &self.device_id,
        })
        .expect("hello serializes");
        if pipe.tx.send(hello).await.is_err() {
            return SessionEnd::Reconnect;
        }
        let state = tokio::time::timeout(HELLO_DEADLINE, async {
            loop {
                match pipe.rx.recv().await {
                    Some(text) => match serde_json::from_str::<ServerFrame>(&text) {
                        Ok(frame @ ServerFrame::State { .. }) => return Some(frame),
                        Ok(_) => continue, // stale broadcast before our state
                        Err(err) => {
                            tracing::warn!(error = %err, "registry: bad frame during handshake");
                            return None;
                        }
                    },
                    None => None?,
                }
            }
        })
        .await;
        let Ok(Some(ServerFrame::State {
            seq,
            full,
            gc_floor,
            rows,
            presence,
        })) = state
        else {
            tracing::warn!("registry: no state frame within deadline");
            return SessionEnd::Reconnect;
        };
        {
            let mut doc = lock(&self.doc);
            let outcome = doc.apply_state(seq, full, gc_floor, rows);
            if full {
                self.stats.full_resyncs.fetch_add(1, Relaxed);
            }
            if outcome == StateOutcome::Reseeded {
                tracing::info!("registry: server behind local state; re-seeding");
            }
        }
        {
            let now = tokio::time::Instant::now();
            let mut map = lock(&self.presence);
            for (device, at) in presence {
                map.insert(device, (at, now));
            }
        }
        self.stats.connected.store(true, Relaxed);
        self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
        if ready.is_none() {
            self.stats.rejoins.fetch_add(1, Relaxed);
        }
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let _ = self.events.send(RegistryEvent::Connected);
        let _ = self.events.send(RegistryEvent::Applied);

        // Anything pending (offline writes, reseeds, migration) pushes now.
        if !self.push_pending(&mut pipe).await {
            return SessionEnd::Reconnect;
        }

        // ── steady state ────────────────────────────────────────────────────
        let mut liveness = Liveness::new();
        loop {
            let quiet_probe_at = liveness.quiet_at(self.tuning.probe_quiet);
            let deadline_at = liveness.deadline_at();
            tokio::select! {
                frame = pipe.rx.recv() => {
                    let Some(text) = frame else {
                        return SessionEnd::Reconnect;
                    };
                    liveness.note_frame();
                    if !self.handle_frame(&text) {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.nudge_rx.recv() => {
                    if !self.push_pending(&mut pipe).await {
                        return SessionEnd::Reconnect;
                    }
                }
                at = self.presence_rx.recv() => {
                    if let Some(at) = at {
                        let frame = serde_json::to_string(&ClientFrame::Presence { at })
                            .expect("presence serializes");
                        if pipe.tx.send(frame).await.is_err() {
                            return SessionEnd::Reconnect;
                        }
                    }
                }
                _ = self.probe_rx.recv() => {
                    if !self.send_probe(&mut pipe, &mut liveness).await {
                        return SessionEnd::Reconnect;
                    }
                }
                _ = self.redial_rx.recv() => {
                    tracing::info!("registry: redial requested");
                    return SessionEnd::Reconnect;
                }
                _ = wake.recv() => {
                    tracing::info!("registry: system wake; redialing");
                    return SessionEnd::Wake;
                }
                _ = tokio::time::sleep_until(quiet_probe_at) => {
                    if !self.send_probe(&mut pipe, &mut liveness).await {
                        return SessionEnd::Reconnect;
                    }
                    // Don't re-arm the quiet timer against the same silence.
                    liveness.touch();
                }
                _ = tokio::time::sleep_until(deadline_at) => {
                    tracing::warn!("registry: probe unanswered past deadline; redialing");
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

    /// Unwind a session that is no longer usable: everything unacked becomes
    /// pushable again and the public stats stop claiming a live connection.
    /// Called for `Reconnect` (transport died) and `Wake` (suspend killed the
    /// socket silently) so both recover the same way.
    fn on_session_end(&mut self, _end: SessionEnd) {
        use std::sync::atomic::Ordering::Relaxed;
        lock(&self.doc).mark_disconnected();
        self.stats.connected.store(false, Relaxed);
        self.stats.disconnects.fetch_add(1, Relaxed);
        let _ = self.events.send(RegistryEvent::Disconnected);
    }

    fn protocol_name(&self) -> &'static str {
        "registry"
    }
}

impl Actor {
    async fn send_probe(&self, pipe: &mut TextPipe, liveness: &mut Liveness) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.stats.probes.fetch_add(1, Relaxed);
        let frame = serde_json::to_string(&ClientFrame::Probe).expect("probe serializes");
        if pipe.tx.send(frame).await.is_err() {
            return false;
        }
        liveness.note_probe_sent();
        true
    }

    async fn push_pending(&self, pipe: &mut TextPipe) -> bool {
        let batches: Vec<PendingBatch> = lock(&self.doc).take_pushable();
        for batch in batches {
            let frame = serde_json::to_string(&ClientFrame::Push {
                batch: &batch.batch,
                ops: &batch.ops,
            })
            .expect("push serializes");
            if pipe.tx.send(frame).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Apply one inbound protocol frame. False = protocol breakdown, redial.
    fn handle_frame(&self, text: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let frame = match serde_json::from_str::<ServerFrame>(text) {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!(error = %err, "registry: unparseable frame");
                return false;
            }
        };
        match frame {
            ServerFrame::Rows { seq, rows } => {
                lock(&self.doc).apply_rows(seq, rows);
                self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
                let _ = self.events.send(RegistryEvent::Applied);
            }
            ServerFrame::Ack { batch, seq, .. } => {
                lock(&self.doc).ack_batch(&batch, seq);
                self.stats.last_ack_ms.store(epoch_ms(), Relaxed);
                let _ = self.events.send(RegistryEvent::Applied);
            }
            ServerFrame::Presence { device, at } => {
                lock(&self.presence).insert(device, (at, tokio::time::Instant::now()));
                let _ = self.events.send(RegistryEvent::Presence);
            }
            ServerFrame::ProbeOk { .. } => {
                self.stats.last_pushed_ms.store(epoch_ms(), Relaxed);
            }
            ServerFrame::State {
                seq,
                full,
                gc_floor,
                rows,
                presence,
            } => {
                // Servers only send state as a hello answer, but applying a
                // late duplicate is harmless and simpler than special-casing.
                let mut doc = lock(&self.doc);
                doc.apply_state(seq, full, gc_floor, rows);
                drop(doc);
                let now = tokio::time::Instant::now();
                let mut map = lock(&self.presence);
                for (device, at) in presence {
                    map.insert(device, (at, now));
                }
                let _ = self.events.send(RegistryEvent::Applied);
            }
            ServerFrame::Error { code, message } => {
                self.stats.rejected.fetch_add(1, Relaxed);
                tracing::warn!(code, message, "registry: server rejected a frame");
            }
        }
        true
    }
}

#[cfg(any(test, feature = "mock-server"))]
pub mod mock_server;

#[cfg(test)]
mod session_end_tests {
    use super::*;
    use futures::future::BoxFuture;
    use komet_proto::Device;

    /// A dialer that is never used: these tests exercise the post-session
    /// bookkeeping, not the transport.
    struct NoDial;

    impl Connector<String> for NoDial {
        fn connect(&self) -> BoxFuture<'static, Result<TextPipe, SyncError>> {
            Box::pin(async { Err(SyncError::WebSocket("not dialed in this test".into())) })
        }
    }

    fn actor(doc: Arc<Mutex<RegistryDoc>>) -> (Actor, broadcast::Receiver<RegistryEvent>) {
        let (events, events_rx) = broadcast::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        std::mem::forget(shutdown_tx); // keep the channel open for the actor
        let (_nudge_tx, nudge_rx) = mpsc::channel(1);
        let (_probe_tx, probe_rx) = mpsc::channel(1);
        let (_redial_tx, redial_rx) = mpsc::channel(1);
        let (_presence_tx, presence_rx) = mpsc::channel(1);
        let actor = Actor {
            doc,
            device_id: "dev-a".into(),
            connector: Arc::new(NoDial),
            tuning: RegistryTuning::default(),
            events,
            shutdown: shutdown_rx,
            nudge_rx,
            probe_rx,
            redial_rx,
            presence_rx,
            presence: Arc::new(Mutex::new(HashMap::new())),
            stats: Arc::new(Stats::default()),
        };
        (actor, events_rx)
    }

    /// A wake ends the session without any transport error, so a push that was
    /// in flight must be released for replay — otherwise `take_pushable()`
    /// (which skips `in_flight` batches) would never resend that write on the
    /// replacement connection.
    #[test]
    fn wake_releases_in_flight_batches_for_replay() {
        let doc = Arc::new(Mutex::new(RegistryDoc::new("dev-a")));
        {
            let mut d = lock(&doc);
            d.upsert_device(&Device {
                id: "dev-a".into(),
                name: "A".into(),
                platform: "linux".into(),
                last_seen_at: None,
                created_at: None,
                version: None,
            })
            .expect("local write");
            let pushed = d.take_pushable();
            assert_eq!(pushed.len(), 1, "the local write is queued for push");
            assert!(
                d.take_pushable().is_empty(),
                "in flight until acked or disconnected"
            );
        }

        let (mut actor, mut events_rx) = actor(doc.clone());
        actor
            .stats
            .connected
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // The `SessionEnd::Wake` path: the supervisor calls this for both
        // Reconnect and Wake.
        actor.on_session_end(SessionEnd::Wake);

        assert!(
            !actor
                .stats
                .connected
                .load(std::sync::atomic::Ordering::Relaxed),
            "stats must not claim a live connection during the redial"
        );
        assert_eq!(
            actor
                .stats
                .disconnects
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(matches!(
            events_rx.try_recv(),
            Ok(RegistryEvent::Disconnected)
        ));
        assert_eq!(
            lock(&doc).take_pushable().len(),
            1,
            "the interrupted batch must be pushable on the new connection"
        );
    }
}
