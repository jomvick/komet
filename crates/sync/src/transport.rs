//! Shared WebSocket session supervisor for the sync clients.
//!
//! Every room client (registry, chat2, device relay) runs the same skeleton:
//! dial with a timeout, run one protocol session, unwind it, back off
//! exponentially — cut short by wake/online events — and redial. What differs
//! is only the frame codec, the hello handshake, and frame handling; those
//! stay in each protocol's `Session` impl.
//!
//! Liveness discipline is inherited from the 2026-07-30 incidents: the
//! transport-level text ping elicits a runtime auto-pong that proves NOTHING
//! about the DO, so room health is judged only by protocol frames — a probe
//! that goes unanswered past [`PROBE_DEADLINE`] tears the session down.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::{SyncError, UrlProvider};

/// Text `"ping"` keepalive interval (answered by the runtime's auto-pong —
/// transport liveness only, never proof of room health).
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Transport silence lease: pongs count, so a healthy socket never trips this.
pub(crate) const SILENCE_LEASE: Duration = Duration::from_secs(45);
/// Bound on one dial attempt (URL fetch + WS handshake).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// The server must answer a hello with its state frame within this deadline.
pub(crate) const HELLO_DEADLINE: Duration = Duration::from_secs(15);
/// A probe must be answered by some protocol frame within this deadline, or
/// the session is torn down for a fresh socket.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(10);
/// Reconnect backoff: 250 ms doubling to a 30 s cap.
pub(crate) const BACKOFF_BASE: Duration = Duration::from_millis(250);
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Quiet-room probe cadence default (one room per engine — a fixed cadence
/// costs ~100 DO wakes/day total).
pub(crate) const PROBE_QUIET_DEFAULT: Duration = Duration::from_secs(900);
/// The "no deadline armed" select arm sleeps out a day and is always beaten
/// by the real timers.
const DAY: Duration = Duration::from_secs(86_400);

// ── pipe + connector ────────────────────────────────────────────────────────

/// One live session's frame channels: `tx` feeds the socket, `rx` yields what
/// the peer sent. `F` is `String` for JSON-text protocols, `Vec<u8>` for the
/// binary chat2 codec.
pub struct Pipe<F> {
    pub tx: mpsc::Sender<F>,
    pub rx: mpsc::Receiver<F>,
}

/// Dials one session. Production uses [`WsConnector`]; tests inject fakes.
pub(crate) trait Connector<F>: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe<F>, SyncError>>;
}

/// Production connector: fresh URL from the provider (new `?token=` per
/// attempt) then `dial::connect_ws` (happy-eyeballs across address families).
pub(crate) struct WsConnector {
    provider: Arc<dyn UrlProvider>,
}

impl WsConnector {
    pub(crate) fn new(provider: Arc<dyn UrlProvider>) -> Self {
        Self { provider }
    }
}

impl Connector<String> for WsConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe<String>, SyncError>> {
        let provider = self.provider.clone();
        Box::pin(async move { connect_text(provider).await })
    }
}

impl Connector<Vec<u8>> for WsConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe<Vec<u8>>, SyncError>> {
        let provider = self.provider.clone();
        Box::pin(async move { connect_bin(provider).await })
    }
}

async fn connect_text(provider: Arc<dyn UrlProvider>) -> Result<Pipe<String>, SyncError> {
    let url = provider.url().await?;
    let ws = crate::dial::connect_ws(&url)
        .await
        .map_err(|e| SyncError::WebSocket(e.to_string()))?;
    Ok(text_pipe(ws, SILENCE_LEASE))
}

async fn connect_bin(provider: Arc<dyn UrlProvider>) -> Result<Pipe<Vec<u8>>, SyncError> {
    let url = provider.url().await?;
    let ws = crate::dial::connect_ws(&url)
        .await
        .map_err(|e| SyncError::WebSocket(e.to_string()))?;
    Ok(binary_pipe(ws, SILENCE_LEASE))
}

/// Wrap an already-established WebSocket in text-frame channels plus the
/// keepalive/silence-lease pump. `silence_lease` is per-protocol: the device
/// relay runs a shorter lease than the room clients.
pub fn text_pipe(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    silence_lease: Duration,
) -> Pipe<String> {
    ws_pipe(ws, silence_lease, WsMessage::Text, |msg| match msg {
        WsMessage::Text(text) => {
            let text = text.to_string();
            // The runtime answers our "ping" keepalive with "pong";
            // transport liveness only — never a protocol frame.
            (text != "pong").then_some(text)
        }
        _ => None,
    })
}

/// Binary-frame sibling of [`text_pipe`].
pub fn binary_pipe(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    silence_lease: Duration,
) -> Pipe<Vec<u8>> {
    ws_pipe(ws, silence_lease, WsMessage::Binary, |msg| match msg {
        WsMessage::Binary(bytes) => Some(bytes.to_vec()),
        _ => None,
    })
}

fn ws_pipe<F: Send + 'static>(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    silence_lease: Duration,
    to_msg: fn(F) -> WsMessage,
    from_msg: fn(WsMessage) -> Option<F>,
) -> Pipe<F> {
    let (out_tx, out_rx) = mpsc::channel(64);
    let (in_tx, in_rx) = mpsc::channel(64);
    tokio::spawn(pump(ws, out_rx, in_tx, silence_lease, to_msg, from_msg));
    Pipe {
        tx: out_tx,
        rx: in_rx,
    }
}

/// Shuttle frames between the WebSocket and the session's channels, plus the
/// ping keepalive and the transport silence lease.
async fn pump<F: Send + 'static>(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<F>,
    in_tx: mpsc::Sender<F>,
    silence_lease: Duration,
    to_msg: fn(F) -> WsMessage,
    from_msg: fn(WsMessage) -> Option<F>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(frame) => {
                    if sink.send(to_msg(frame)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Close(close))) => {
                    // Peer-initiated close: log the code/reason (relay close
                    // codes like 4409 "superseded host" are diagnostic gold)
                    // and end — the stream is finished either way.
                    if let Some(close) = close {
                        tracing::info!(
                            code = %close.code,
                            reason = %close.reason,
                            "ws closed by peer"
                        );
                    }
                    break;
                }
                Some(Ok(msg)) => {
                    last_rx = tokio::time::Instant::now();
                    if let Some(frame) = from_msg(msg)
                        && in_tx.send(frame).await.is_err()
                    {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + silence_lease) => {
                tracing::warn!("socket silent past lease; treating as dead");
                break;
            }
        }
    }
}

// ── session channels ────────────────────────────────────────────────────────

/// The nudge/probe/redial/presence/shutdown channel bundle every client
/// exposes. Capacity-1 signal channels keep `try_send` coalescing semantics:
/// a second nudge while one is pending is dropped, which is correct — the
/// pending one already covers it.
pub(crate) struct Signals<P> {
    pub(crate) nudge_tx: mpsc::Sender<()>,
    pub(crate) nudge_rx: mpsc::Receiver<()>,
    pub(crate) probe_tx: mpsc::Sender<()>,
    pub(crate) probe_rx: mpsc::Receiver<()>,
    pub(crate) redial_tx: mpsc::Sender<()>,
    pub(crate) redial_rx: mpsc::Receiver<()>,
    pub(crate) presence_tx: mpsc::Sender<P>,
    pub(crate) presence_rx: mpsc::Receiver<P>,
    pub(crate) shutdown_tx: watch::Sender<bool>,
    pub(crate) shutdown_rx: watch::Receiver<bool>,
}

impl<P> Signals<P> {
    /// `presence_capacity` bounds queued presence beats (4 everywhere today —
    /// beats are lossy hints, not state).
    pub(crate) fn new(presence_capacity: usize) -> Self {
        let (nudge_tx, nudge_rx) = mpsc::channel(1);
        let (probe_tx, probe_rx) = mpsc::channel(1);
        let (redial_tx, redial_rx) = mpsc::channel(1);
        let (presence_tx, presence_rx) = mpsc::channel(presence_capacity);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
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
        }
    }
}

// ── session end + liveness ──────────────────────────────────────────────────

/// How a session ended; the supervisor turns it into the next action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// Transport died / probe deadline / requested redial: unwind, back off,
    /// redial.
    Reconnect,
    /// System wake: the socket sat through a suspend and is dead even though
    /// no transport error was reported — unwind like a drop, then redial at
    /// once with a fresh backoff (sleeping first would delay the recovery the
    /// wake announced).
    Wake,
    /// Shutdown requested: stop the actor.
    Stop,
}

/// Protocol-frame liveness for one session: quiet-room probe cadence and the
/// probe-answer deadline. Any inbound protocol frame proves the room alive;
/// transport pongs do NOT reach this tracker (they're filtered in `pump`).
#[derive(Debug)]
pub(crate) struct Liveness {
    last_frame: tokio::time::Instant,
    probe_deadline: Option<tokio::time::Instant>,
}

impl Liveness {
    pub(crate) fn new() -> Self {
        Self {
            last_frame: tokio::time::Instant::now(),
            probe_deadline: None,
        }
    }

    /// An inbound protocol frame: room is alive, and it answers any pending
    /// probe so the deadline disarms.
    pub(crate) fn note_frame(&mut self) {
        self.last_frame = tokio::time::Instant::now();
        self.probe_deadline = None;
    }

    /// Re-arm the quiet-probe timer WITHOUT clearing the probe deadline —
    /// used after WE send a probe: silence continues, but the answer clock
    /// stays armed.
    pub(crate) fn touch(&mut self) {
        self.last_frame = tokio::time::Instant::now();
    }

    /// When the quiet-room probe should fire.
    pub(crate) fn quiet_at(&self, probe_quiet: Duration) -> tokio::time::Instant {
        self.last_frame + probe_quiet
    }

    /// When an unanswered probe kills the session (far future when none is
    /// outstanding).
    pub(crate) fn deadline_at(&self) -> tokio::time::Instant {
        self.probe_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + DAY)
    }

    /// Record that a probe was just sent; arm the answer deadline once —
    /// repeated probes before the deadline do not push it out.
    pub(crate) fn note_probe_sent(&mut self) {
        if self.probe_deadline.is_none() {
            self.probe_deadline = Some(tokio::time::Instant::now() + PROBE_DEADLINE);
        }
    }
}

// ── the supervisor ──────────────────────────────────────────────────────────

/// What a protocol implements: one session's worth of work on a [`Pipe`].
/// `run_session` owns the hello handshake AND the steady-state loop; the
/// supervisor owns everything between sessions.
pub(crate) trait Session<F>: Send {
    /// Run one session to its end. `ready` resolves the caller's initial-join
    /// wait exactly once (first successful handshake); a session that ends
    /// while `ready` is still `Some` failed the FIRST join.
    fn run_session<'a>(
        &'a mut self,
        pipe: Pipe<F>,
        ready: &'a mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> impl std::future::Future<Output = SessionEnd> + Send + 'a;

    /// Unwind after a session ended badly — release in-flight state, mark the
    /// public stats disconnected, emit the Disconnected event. Called for
    /// BOTH `Reconnect` and `Wake`: a suspend kills the socket silently, so
    /// the wake path must recover exactly like a transport drop.
    fn on_session_end(&mut self, end: SessionEnd);

    /// Protocol label for logs and the first-join error
    /// ("registry handshake failed", "chat2 handshake failed", …).
    fn protocol_name(&self) -> &'static str;
}

/// Wait out one reconnect-backoff step, cut short by a wake or online event.
/// Returns true when shutdown was observed.
async fn sleep_or_shutdown(shutdown: &mut watch::Receiver<bool>, wait: Duration) -> bool {
    tokio::select! {
        // Wake/online events cut the backoff short; an offline-reported
        // path parks the wait instead of burning futile dials.
        _ = crate::wake::reconnect_wait(wait) => false,
        _ = shutdown.changed() => *shutdown.borrow(),
    }
}

/// The dial → session → backoff → redial loop shared by every client.
/// Consumes `session`; returns when shutdown is observed or the FIRST join
/// fails (the error goes back through `ready` — callers own the initial-join
/// retry, same contract as before the extraction).
pub(crate) async fn run<F, S>(
    connector: Arc<dyn Connector<F>>,
    mut session: S,
    mut shutdown: watch::Receiver<bool>,
    ready: oneshot::Sender<Result<(), SyncError>>,
) where
    F: Send + 'static,
    S: Session<F>,
{
    let mut ready = Some(ready);
    let mut backoff = BACKOFF_BASE;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let dial = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect()).await;
        let pipe = match dial {
            Ok(Ok(pipe)) => pipe,
            outcome => {
                let (err, timed_out) = match outcome {
                    Ok(Err(err)) => (err, false),
                    Err(_) => (SyncError::WebSocket("connect timeout".into()), true),
                    Ok(Ok(_)) => unreachable!("handled above"),
                };
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(err));
                    return; // first join failed: caller owns the retry
                }
                if timed_out {
                    tracing::warn!("{} dial timed out; backing off", session.protocol_name());
                } else {
                    tracing::warn!(
                        error = %err,
                        "{} dial failed; backing off",
                        session.protocol_name()
                    );
                }
                if sleep_or_shutdown(&mut shutdown, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_CAP);
                continue;
            }
        };

        match session.run_session(pipe, &mut ready).await {
            SessionEnd::Stop => return,
            SessionEnd::Wake => {
                session.on_session_end(SessionEnd::Wake);
                // Resume is an event, not a timeout: no backoff sleep, and
                // the next failure starts the ladder over — the network path
                // just changed, so the old delay says nothing about the new
                // one.
                backoff = BACKOFF_BASE;
            }
            SessionEnd::Reconnect => {
                session.on_session_end(SessionEnd::Reconnect);
                if ready.is_some() {
                    // Handshake failed on the very first session.
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(SyncError::Protocol(format!(
                            "{} handshake failed",
                            session.protocol_name()
                        ))));
                    }
                    return;
                }
                if sleep_or_shutdown(&mut shutdown, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Freeze the liveness/backoff constant table: drift here silently
    /// changes every client's reconnect and probe behavior at once.
    #[test]
    fn liveness_constants_are_frozen() {
        assert_eq!(PING_INTERVAL, Duration::from_secs(15));
        assert_eq!(SILENCE_LEASE, Duration::from_secs(45));
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(20));
        assert_eq!(HELLO_DEADLINE, Duration::from_secs(15));
        assert_eq!(PROBE_DEADLINE, Duration::from_secs(10));
        assert_eq!(BACKOFF_BASE, Duration::from_millis(250));
        assert_eq!(BACKOFF_CAP, Duration::from_secs(30));
        assert_eq!(PROBE_QUIET_DEFAULT, Duration::from_secs(900));
    }

    #[test]
    fn liveness_tracks_frames_probes_and_deadlines() {
        let mut live = Liveness::new();
        live.note_probe_sent();
        let deadline = live.probe_deadline.expect("probe arms the deadline");
        assert!(live.deadline_at() <= deadline + PROBE_DEADLINE);
        // A second probe before the answer does not push the deadline out.
        live.note_probe_sent();
        assert_eq!(live.probe_deadline, Some(deadline));
        live.note_frame();
        assert!(live.probe_deadline.is_none(), "a frame answers the probe");
    }
}
