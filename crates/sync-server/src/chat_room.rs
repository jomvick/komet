//! chat2 hub (`/chat2/{chatId}/ws` + checkpoint/tail sidecars) — the Rust port
//! of the removed Cloudflare Durable Object (`edge/src/chat-room.ts`).
//!
//! The hub is a dumb authenticated log relay: opaque Loro updates are appended
//! to a seq-ordered log ([`crate::chat_log`]), live-relayed to the room's other
//! ready sockets, and served back on join. All CRDT semantics live in the
//! clients; this module owns no doc interpretation.
//!
//! Wire frames are the client codec ([`komet_sync::chat_frames`]), so the two
//! ends cannot drift apart. Own replies (STATE/ACK/ROWS_DONE/PROBE_OK/ERROR)
//! are written straight to the requesting socket; ROW/PRESENCE fan-out travels
//! over a per-room broadcast and each socket drops events it originated (the
//! sender must never see its own row echoed).
//!
//! Quota windows and presence beats are memory-only and reset on restart,
//! exactly like the DO's in-memory maps: they contain runaway-client loops and
//! light the stats surface, not honest traffic metering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use komet_sync::chat_frames::{self as wire, frame_type};
use tokio::sync::broadcast;

use crate::AppState;
use crate::chat_log::{
    AppendOutcome, CHECKPOINT_BLOB, ChatLog, CheckpointOutcome, FRONTIER_BLOB, LogStats,
};

/// Inbound frame budget: one pushed row plus header slack. The client caps its
/// own pushes below this, so anything bigger is a peer bug.
const MAX_FRAME_BYTES: usize = 1024 * 1024 + 8192;
/// Host-published sidecar budget (tail JSON / diff payload).
const MAX_SIDECAR_BYTES: usize = 16 * 1024 * 1024;
/// Checkpoint upload budget (a rebuilt thin doc is KB-to-100s-of-KB scale).
const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
/// Presence beats older than this are swept before relay/stats.
const PRESENCE_TTL: Duration = Duration::from_secs(30);
/// Per-device rolling push quota (memory-only; contains runaway loops).
const QUOTA_WINDOW: Duration = Duration::from_secs(60);
const QUOTA_MAX_PUSHES: u32 = 300;
const QUOTA_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Room broadcast depth. A lagging socket is closed rather than replayed:
/// rows cannot be selectively re-sent, and the client backfills on redial.
const BROADCAST_DEPTH: usize = 1024;

static NEXT_ORIGIN: AtomicU64 = AtomicU64::new(1);

/// A live chat2 hub. Rooms are opened lazily and kept for the process lifetime
/// (reopening a sqlite file per frame would be pure overhead).
#[derive(Clone)]
pub struct ChatHub {
    data_dir: PathBuf,
    rooms: Arc<Mutex<HashMap<String, Arc<ChatRoom>>>>,
}

impl ChatHub {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            rooms: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The room for `chat_id`, opening its database on first use. `None` for
    /// an id that could escape the rooms directory (clients mint uuids; the
    /// rule is the edge's `^[A-Za-z0-9_-]{1,128}$`).
    fn room(&self, chat_id: &str) -> Option<Arc<ChatRoom>> {
        if !chat_id_is_valid(chat_id) {
            return None;
        }
        let mut rooms = lock(&self.rooms);
        if let Some(room) = rooms.get(chat_id) {
            return Some(room.clone());
        }
        let path = chat_db_path(&self.data_dir, chat_id);
        let log = match ChatLog::open(&path) {
            Ok(log) => log,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "chat2 room open failed");
                return None;
            }
        };
        let (tx, _) = broadcast::channel(BROADCAST_DEPTH);
        let room = Arc::new(ChatRoom {
            log: Arc::new(log),
            tx,
            presence: Mutex::new(HashMap::new()),
            quotas: Mutex::new(HashMap::new()),
        });
        rooms.insert(chat_id.to_string(), room.clone());
        Some(room)
    }
}

fn chat_id_is_valid(chat_id: &str) -> bool {
    (1..=128).contains(&chat_id.len())
        && chat_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A separate database per chat: a chat id is also a `/session/{id}` room
/// name, and the two protocols must never share a table.
fn chat_db_path(data_dir: &std::path::Path, chat_id: &str) -> PathBuf {
    let safe: String = chat_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir
        .join("rooms")
        .join("chat2")
        .join(format!("{safe}.db"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One live chat room: its log, the fan-out channel, and the memory-only
/// side tables.
struct ChatRoom {
    log: Arc<ChatLog>,
    tx: broadcast::Sender<Arc<RoomEvent>>,
    presence: Mutex<HashMap<String, i64>>,
    quotas: Mutex<HashMap<String, QuotaWindow>>,
}

/// What fan-out carries. `origin` identifies the emitting socket so its own
/// events are dropped by that socket's receive loop (no self-echo).
enum RoomEvent {
    Row {
        origin: u64,
        seq: u64,
        device: String,
        batch_id: String,
        payload: Vec<u8>,
    },
    Presence {
        origin: u64,
        device: String,
        at: i64,
        payload: Vec<u8>,
    },
}

impl RoomEvent {
    /// The socket that emitted this event (its own receive loop drops it).
    fn origin(&self) -> u64 {
        match self {
            RoomEvent::Row { origin, .. } | RoomEvent::Presence { origin, .. } => *origin,
        }
    }
}

struct QuotaWindow {
    since: Instant,
    pushes: u32,
    bytes: usize,
}

impl ChatRoom {
    fn admit_quota(&self, device: &str, bytes: usize) -> bool {
        let now = Instant::now();
        let key = if device.is_empty() {
            "(unknown)"
        } else {
            device
        };
        let mut quotas = lock(&self.quotas);
        let window = quotas.entry(key.to_string()).or_insert(QuotaWindow {
            since: now,
            pushes: 0,
            bytes: 0,
        });
        if now.duration_since(window.since) > QUOTA_WINDOW {
            *window = QuotaWindow {
                since: now,
                pushes: 0,
                bytes: 0,
            };
        }
        window.pushes += 1;
        window.bytes += bytes;
        window.pushes <= QUOTA_MAX_PUSHES && window.bytes <= QUOTA_MAX_BYTES
    }

    fn touch_presence(&self, device: &str, at: i64) {
        let horizon = now_ms() - PRESENCE_TTL.as_millis() as i64;
        let mut presence = lock(&self.presence);
        presence.retain(|_, beat| *beat >= horizon);
        presence.insert(device.to_string(), at);
    }
}

fn encode_event(event: &RoomEvent) -> Vec<u8> {
    match event {
        RoomEvent::Row {
            seq,
            device,
            batch_id,
            payload,
            ..
        } => wire::encode(
            frame_type::ROW,
            &serde_json::json!({"seq": seq, "device": device, "batchId": batch_id}),
            payload,
        ),
        RoomEvent::Presence {
            device,
            at,
            payload,
            ..
        } => wire::encode(
            frame_type::PRESENCE,
            &serde_json::json!({"device": device, "at": at}),
            payload,
        ),
    }
}

/// A chat2 socket. Auth is the server's usual bearer (`?token=` included);
/// the `device` query parameter seeds the identity until HELLO confirms it.
pub(crate) async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    query: RawQuery,
) -> Result<Response, (StatusCode, &'static str)> {
    if !crate::check_auth(&state, &headers) && !crate::check_query_token(&state, query.0.as_deref())
    {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let Some(room) = state.chat.room(&chat_id) else {
        return Err((StatusCode::BAD_REQUEST, "invalid chat id"));
    };
    let device = query_param(query.0.as_deref(), "device")
        .unwrap_or_default()
        .to_string();
    Ok(ws.on_upgrade(move |socket| serve(room, device, socket)))
}

async fn serve(room: Arc<ChatRoom>, initial_device: String, socket: WebSocket) {
    let origin = NEXT_ORIGIN.fetch_add(1, Ordering::Relaxed);
    let mut events = room.tx.subscribe();
    let (mut sender, mut receiver) = socket.split();
    let mut device = initial_device;
    let mut ready = false;

    loop {
        tokio::select! {
            biased;
            event = events.recv() => match event {
                Ok(event) => {
                    // Own events never echo; a socket that has not said hello
                    // yet has no business receiving relayed traffic.
                    if event.origin() == origin || !ready {
                        continue;
                    }
                    if sender
                        .send(Message::Binary(encode_event(&event)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Missed frames cannot be replayed selectively; closing
                    // makes the client redial and backfill from its cursor.
                    tracing::warn!(skipped, "chat2 socket lagged; closing for a redial");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            message = receiver.next() => {
                let Some(message) = message else { break };
                match message {
                    Ok(Message::Binary(bytes)) => {
                        if bytes.len() > MAX_FRAME_BYTES {
                            let _ = sender
                                .send(Message::Close(Some(CloseFrame {
                                    code: 1009,
                                    reason: "frame too large".into(),
                                })))
                                .await;
                            break;
                        }
                        if !handle_frame(&room, origin, &mut device, &mut ready, &mut sender, &bytes)
                            .await
                        {
                            break;
                        }
                    }
                    Ok(Message::Text(text)) if text == "ping" => {
                        if sender.send(Message::Text("pong".to_string())).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Text(_)) => {
                        // The chat2 protocol is binary; a text frame is a
                        // different codec (the DO closed 1003 as well).
                        let _ = sender
                            .send(Message::Close(Some(CloseFrame {
                                code: 1003,
                                reason: "binary frames only".into(),
                            })))
                            .await;
                        break;
                    }
                    Ok(Message::Ping(payload)) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Pong(_)) => {}
                }
            }
        }
    }
}

/// Handle one inbound frame. Returns false when the socket must close.
async fn handle_frame(
    room: &Arc<ChatRoom>,
    origin: u64,
    device: &mut String,
    ready: &mut bool,
    sender: &mut SplitSink<WebSocket, Message>,
    bytes: &[u8],
) -> bool {
    let Some(frame) = wire::decode(bytes) else {
        // Recoverable: a corrupt frame from a flaky link must not cost a
        // reconnect cycle (the DO answered `error`, not a close).
        return send_error(sender, "bad_frame", "malformed frame", None).await;
    };
    match frame.kind {
        frame_type::HELLO => {
            if let Some(got) = frame.header.get("device").and_then(|v| v.as_str())
                && !got.is_empty()
            {
                *device = got.to_string();
            }
            *ready = true;
            let stats = match room.log.stats() {
                Ok(stats) => stats,
                Err(err) => return storage_error(sender, err).await,
            };
            // The client compares the frontier against its local doc: included
            // → rows-only catch-up; not included → GET /checkpoint first.
            let frontier = match room.log.get_blob(FRONTIER_BLOB) {
                Ok(frontier) => frontier.unwrap_or_default(),
                Err(err) => return storage_error(sender, err).await,
            };
            send_wire(sender, frame_type::STATE, state_header(&stats), &frontier).await
        }
        frame_type::ROWS_REQ => {
            if !*ready {
                return send_error(sender, "hello_first", "rows before hello", None).await;
            }
            let after = frame
                .header
                .get("after")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let exclude_own = frame
                .header
                .get("excludeOwn")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let exclude = exclude_own.then(|| device.clone());
            let rows = match room.log.rows_after(after, exclude.as_deref()) {
                Ok(rows) => rows,
                Err(err) => return storage_error(sender, err).await,
            };
            for row in rows {
                let header = serde_json::json!({
                    "seq": row.seq,
                    "device": row.device,
                    "batchId": row.batch_id,
                });
                if !send_wire(sender, frame_type::ROW, header, &row.bytes).await {
                    return false;
                }
            }
            // Read the head AFTER the backfill: rows pushed concurrently must
            // still be reachable from the cursor this frame hands back.
            let head = match room.log.stats() {
                Ok(stats) => stats.head_seq,
                Err(err) => return storage_error(sender, err).await,
            };
            send_wire(
                sender,
                frame_type::ROWS_DONE,
                serde_json::json!({"headSeq": head}),
                &[],
            )
            .await
        }
        frame_type::PUSH => {
            let batch_id = frame
                .header
                .get("batchId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Push errors carry the batchId so clients can retire permanently
            // rejected batches instead of replaying them forever.
            if !*ready || batch_id.is_empty() || batch_id.len() > 128 {
                return send_error(
                    sender,
                    "bad_push",
                    "hello first / malformed push",
                    Some(batch_id),
                )
                .await;
            }
            let payload = &frame.payload;
            if payload.is_empty() {
                return send_error(sender, "empty", "push payload is empty", Some(batch_id)).await;
            }
            if payload.len() > crate::chat_log::MAX_ROW_BYTES {
                return send_error(sender, "too_large", "row exceeds the cap", Some(batch_id))
                    .await;
            }
            if !room.admit_quota(device.as_str(), payload.len()) {
                return send_error(
                    sender,
                    "quota",
                    "per-device push quota exceeded",
                    Some(batch_id),
                )
                .await;
            }
            match room.log.append(device, batch_id, payload, now_ms()) {
                Ok(AppendOutcome::Appended { seq, dup }) => {
                    if !dup {
                        // Live relay to every other ready socket; the sender
                        // holds its own bytes and only gets the ack.
                        let _ = room.tx.send(Arc::new(RoomEvent::Row {
                            origin,
                            seq,
                            device: device.clone(),
                            batch_id: batch_id.to_string(),
                            payload: payload.clone(),
                        }));
                    }
                    send_wire(
                        sender,
                        frame_type::ACK,
                        serde_json::json!({"batchId": batch_id, "seq": seq, "dup": dup}),
                        &[],
                    )
                    .await
                }
                Ok(AppendOutcome::Empty) => {
                    send_error(sender, "empty", "push payload is empty", Some(batch_id)).await
                }
                Ok(AppendOutcome::TooLarge) => {
                    send_error(sender, "too_large", "row exceeds the cap", Some(batch_id)).await
                }
                Err(err) => storage_error(sender, err).await,
            }
        }
        frame_type::PRESENCE => {
            // Opaque ephemera: relayed verbatim, never stored. A device that
            // joins later simply waits for the next beat.
            if !*ready || device.is_empty() {
                return true;
            }
            let at = frame
                .header
                .get("at")
                .and_then(|v| v.as_i64())
                .unwrap_or_else(now_ms);
            room.touch_presence(device, at);
            let _ = room.tx.send(Arc::new(RoomEvent::Presence {
                origin,
                device: device.clone(),
                at,
                payload: frame.payload.clone(),
            }));
            true
        }
        frame_type::PROBE => {
            let head = match room.log.stats() {
                Ok(stats) => stats.head_seq,
                Err(err) => return storage_error(sender, err).await,
            };
            send_wire(
                sender,
                frame_type::PROBE_OK,
                serde_json::json!({"headSeq": head}),
                &[],
            )
            .await
        }
        other => {
            send_error(
                sender,
                "bad_frame",
                &format!("unexpected type {other}"),
                None,
            )
            .await
        }
    }
}

fn state_header(stats: &LogStats) -> serde_json::Value {
    serde_json::json!({
        "headSeq": stats.head_seq,
        "seqFloor": stats.seq_floor,
        "checkpointSeq": stats.checkpoint_seq,
        "checkpointSize": stats.checkpoint_size,
        "rowCount": stats.row_count,
        "rowBytes": stats.row_bytes,
    })
}

async fn send_wire(
    sender: &mut SplitSink<WebSocket, Message>,
    kind: u8,
    header: serde_json::Value,
    payload: &[u8],
) -> bool {
    sender
        .send(Message::Binary(wire::encode(kind, &header, payload)))
        .await
        .is_ok()
}

async fn send_error(
    sender: &mut SplitSink<WebSocket, Message>,
    code: &str,
    message: &str,
    batch_id: Option<&str>,
) -> bool {
    let mut header = serde_json::json!({"code": code, "message": message});
    if let Some(batch_id) = batch_id {
        header["batchId"] = serde_json::Value::String(batch_id.to_string());
    }
    send_wire(sender, frame_type::ERROR, header, &[]).await
}

async fn storage_error(sender: &mut SplitSink<WebSocket, Message>, err: rusqlite::Error) -> bool {
    tracing::error!(error = %err, "chat2 storage error");
    send_error(sender, "internal", "storage error", None).await
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

// ── HTTP surface (checkpoint + host-published sidecars) ─────────────────────

fn json_error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({"error": code}))).into_response()
}

fn internal_error(err: rusqlite::Error) -> Response {
    tracing::error!(error = %err, "chat2 storage error");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}

/// `GET /chat2/{id}/checkpoint` — Range-resumable (`bytes=N-` only): a
/// download that dies at byte 800k resumes instead of restarting.
pub(crate) async fn checkpoint_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    if !crate::check_auth(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(room) = state.chat.room(&chat_id) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_chat_id");
    };
    let bytes = match room.log.get_blob(CHECKPOINT_BLOB) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "not_found"),
        Err(err) => return internal_error(err),
    };
    let seq = room
        .log
        .stats()
        .map(|stats| stats.checkpoint_seq)
        .unwrap_or(0);
    let len = bytes.len();

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("accept-ranges", "bytes")
        .header("x-chat2-checkpoint-seq", seq.to_string());
    if let Some(start) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_range_start)
    {
        if start >= len as u64 {
            builder = builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"));
            return builder
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        let start = start as usize;
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{len}", len - 1),
        );
        return builder
            .body(Body::from(bytes[start..].to_vec()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    builder
        .status(StatusCode::OK)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `bytes=N-` (open-ended resume) only; anything fancier is ignored → 200.
fn parse_range_start(value: &str) -> Option<u64> {
    let start = value.trim().strip_prefix("bytes=")?.strip_suffix('-')?;
    if start.is_empty() || start.contains(',') {
        return None;
    }
    let start: u64 = start.parse().ok()?;
    (start > 0).then_some(start)
}

/// `POST /chat2/{id}/checkpoint?seqCovered=N` — commit a client-built
/// checkpoint covering rows `seq <= N`, pruning them in the same transaction.
pub(crate) async fn checkpoint_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    query: RawQuery,
    body: Bytes,
) -> Response {
    if !crate::check_auth(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(room) = state.chat.room(&chat_id) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_chat_id");
    };
    let Some(raw) = query_param(query.0.as_deref(), "seqCovered") else {
        return json_error(StatusCode::BAD_REQUEST, "bad_seq_covered");
    };
    // `u64` rejects both non-integers and negatives.
    let Ok(seq_covered) = raw.parse::<u64>() else {
        return json_error(StatusCode::BAD_REQUEST, "bad_seq_covered");
    };
    let Some(frontier) = headers
        .get("x-chat2-frontier")
        .and_then(|value| value.to_str().ok())
    else {
        return json_error(StatusCode::BAD_REQUEST, "bad_frontier");
    };
    // Empty is a valid (empty) frontier; only undecodable base64 is rejected.
    let Ok(frontier) = base64::engine::general_purpose::STANDARD.decode(frontier) else {
        return json_error(StatusCode::BAD_REQUEST, "bad_frontier");
    };
    if body.len() > MAX_CHECKPOINT_BYTES {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    }
    match room
        .log
        .commit_checkpoint(seq_covered, &frontier, &body, now_ms())
    {
        Ok(CheckpointOutcome::Committed { seq_floor, pruned }) => Json(serde_json::json!({
            "ok": true,
            "seqFloor": seq_floor,
            "pruned": pruned,
        }))
        .into_response(),
        Ok(CheckpointOutcome::Empty) => json_error(StatusCode::CONFLICT, "empty"),
        Ok(CheckpointOutcome::FloorRegression) => {
            json_error(StatusCode::CONFLICT, "floor_regression")
        }
        Ok(CheckpointOutcome::AheadOfHead) => json_error(StatusCode::CONFLICT, "ahead_of_head"),
        Err(err) => internal_error(err),
    }
}

pub(crate) async fn tail_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    sidecar_get(&state, &headers, &chat_id, "tail").await
}

pub(crate) async fn tail_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    body: Bytes,
) -> Response {
    sidecar_put(&state, &headers, &chat_id, "tail", &body)
}

pub(crate) async fn diff_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
) -> Response {
    sidecar_get(&state, &headers, &chat_id, "diff").await
}

pub(crate) async fn diff_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chat_id): Path<String>,
    body: Bytes,
) -> Response {
    sidecar_put(&state, &headers, &chat_id, "diff", &body)
}

fn sidecar_name(name: &str) -> String {
    format!("sidecar-{name}")
}

async fn sidecar_get(state: &AppState, headers: &HeaderMap, chat_id: &str, name: &str) -> Response {
    if !crate::check_auth(state, headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(room) = state.chat.room(chat_id) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_chat_id");
    };
    let blob = sidecar_name(name);
    let bytes = match room.log.get_blob(&blob) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "not_found"),
        Err(err) => return internal_error(err),
    };
    let content_type = room
        .log
        .meta(&format!("{blob}-type"))
        .ok()
        .flatten()
        .unwrap_or_else(|| "application/json".to_string());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn sidecar_put(
    state: &AppState,
    headers: &HeaderMap,
    chat_id: &str,
    name: &str,
    body: &[u8],
) -> Response {
    if !crate::check_auth(state, headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(room) = state.chat.room(chat_id) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_chat_id");
    };
    if body.len() > MAX_SIDECAR_BYTES {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large");
    }
    let blob = sidecar_name(name);
    if let Err(err) = room.log.put_blob(&blob, body) {
        return internal_error(err);
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json");
    if let Err(err) = room.log.set_meta(&format!("{blob}-type"), content_type) {
        return internal_error(err);
    }
    Json(serde_json::json!({"ok": true, "bytes": body.len()})).into_response()
}
