//! Device-room relay (`/device/{deviceId}/ws`): the point-to-point transport a
//! controller uses to drive another machine's engine.
//!
//! The semantics are the edge `DeviceRoom`'s, on the client's frame codec
//! ([`komet_rpc`] — one implementation, so the two ends cannot drift apart):
//! - a client frame is stamped `from = connId` and handed to the room's one host socket;
//! - a host frame must carry `to = connId` and is delivered with the routing keys stripped;
//! - a text `ping` is answered with a text `pong` here and never enters the room — both
//!   ends treat that pair as proof of life and drop a silent socket after a lease;
//! - nothing is persisted or replayed. A device room is a live transport, not a chat log:
//!   replaying an earlier session's frames would hand a fresh client stale RPC replies.
//!
//! Relay control frames ([`RELAY_KIND`]) carry the room's side of a disconnect:
//! `host_offline` (a client sent with no host), `client_gone` (the host routed to a client
//! that left), `host_closed` (the host socket went away), `client_closed` (a client left).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use komet_rpc::device_room::{
    CLIENT_CLOSED, CLIENT_GONE, HOST_CLOSED, HOST_OFFLINE, NUDGE_KIND, RELAY_KIND,
};
use komet_rpc::{DeviceFrameHeader, decode_device_frame, encode_device_frame};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::AppState;

/// Keepalive pair every device socket speaks. It is answered by this server alone —
/// forwarding a keepalive is what makes a peer read it as a malformed frame.
const PING: &str = "ping";
const PONG: &str = "pong";

/// Pending nudges kept per device while its host is offline; overflow drops the
/// oldest (recency wins, like the DO's `pending_nudges` bound).
const NUDGE_MAX_PENDING: usize = 256;

enum Out {
    Frame(Vec<u8>),
    /// Text `pong` — the answer to the protocol's text `ping`.
    Keepalive,
    /// WebSocket-level pong, for a peer that pings at the frame layer instead.
    WsPong(Vec<u8>),
    Close,
}

#[derive(Default)]
struct DeviceRoom {
    host: Option<UnboundedSender<Out>>,
    clients: HashMap<String, UnboundedSender<Out>>,
    /// Chat ids nudged while no host was connected, replayed in order on the
    /// next host join. Memory-only (the DO persisted them; a sync-server
    /// restart loses a nudge, and the command itself stays durable in the doc).
    pending_nudges: VecDeque<String>,
}

/// Live device rooms, keyed by the URL's room (the host's device id).
#[derive(Clone, Default)]
pub struct DeviceRelay {
    rooms: Arc<Mutex<HashMap<String, DeviceRoom>>>,
}

fn lock(rooms: &Mutex<HashMap<String, DeviceRoom>>) -> MutexGuard<'_, HashMap<String, DeviceRoom>> {
    rooms.lock().unwrap_or_else(PoisonError::into_inner)
}

fn relay_error(code: &str) -> Vec<u8> {
    serde_json::json!({ "error": code })
        .to_string()
        .into_bytes()
}

/// A text `ping` earns a text `pong`; any other text frame is not part of the device
/// protocol and is dropped rather than relayed as bytes.
fn keepalive_reply(text: &str) -> Option<&'static str> {
    (text == PING).then_some(PONG)
}

fn query_pairs(query: Option<&str>) -> HashMap<String, String> {
    query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

impl DeviceRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deliver a nudge frame to the device's live host, or queue it for the
    /// next host join. True = delivered now.
    ///
    /// Unlike the edge DO there is no persisted `pending_nudges` table: this
    /// queue lives in memory, so a queue for a device that was never seen is
    /// harmless (it replays to the first host that claims the room and dies
    /// with the process). The edge answered 404 only for an owner mismatch,
    /// which a single-token server has no way to express.
    pub fn nudge(&self, room: &str, chat_id: &str) -> bool {
        let mut rooms = lock(&self.rooms);
        let entry = rooms.entry(room.to_string()).or_default();
        match &entry.host {
            Some(host) if !host.is_closed() => {
                send_nudge(host, chat_id);
                true
            }
            _ => {
                // Dedupe by chat (one open covers any number of pending
                // commands), bounded so a runaway sender cannot grow it.
                entry.pending_nudges.retain(|pending| pending != chat_id);
                if entry.pending_nudges.len() >= NUDGE_MAX_PENDING {
                    entry.pending_nudges.pop_front();
                }
                entry.pending_nudges.push_back(chat_id.to_string());
                false
            }
        }
    }

    /// Serve one device-room socket until it closes. `role=host` claims the room; any
    /// other role is a client identified by the `connId` it dialed with — that id is
    /// what `from`/`to` route on.
    pub async fn serve(self, room: String, socket: WebSocket, query: Option<String>) {
        let query = query_pairs(query.as_deref());
        let is_host = query.get("role").map(String::as_str) == Some("host");
        let conn_id = query
            .get("connId")
            .cloned()
            .unwrap_or_else(|| "anon".into());
        let (sender, receiver) = socket.split();
        let (tx, rx) = unbounded_channel::<Out>();

        self.join(&room, is_host, &conn_id, &tx);
        let writer = tokio::spawn(pump(sender, rx));

        let mut inbound = receiver;
        while let Some(message) = inbound.next().await {
            match message {
                Ok(Message::Binary(bytes)) => {
                    if !self.route(&room, is_host, &conn_id, &tx, bytes.to_vec()) {
                        break;
                    }
                }
                Ok(Message::Text(text)) => match keepalive_reply(&text) {
                    Some(_) => {
                        if tx.send(Out::Keepalive).is_err() {
                            break;
                        }
                    }
                    None => continue,
                },
                // A WebSocket-level ping still needs its pong; the socket is otherwise
                // silent to the peer and would be reaped as dead.
                Ok(Message::Ping(payload)) => {
                    if tx.send(Out::WsPong(payload)).is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(Message::Pong(_)) => {}
            }
        }

        self.leave(&room, is_host, &conn_id, &tx);
        writer.abort();
    }

    fn join(&self, room: &str, is_host: bool, conn_id: &str, tx: &UnboundedSender<Out>) {
        let mut rooms = lock(&self.rooms);
        let entry = rooms.entry(room.to_string()).or_default();
        if is_host {
            // One host per device: a restart or a second claim supersedes the predecessor
            // rather than splitting its RPC surface across two sockets.
            if let Some(previous) = entry.host.replace(tx.clone()) {
                let _ = previous.send(Out::Close);
            }
            // Cold-chat delivery: nudges that arrived while no host was live
            // are replayed now, in order, and then cleared.
            let pending: Vec<String> = entry.pending_nudges.drain(..).collect();
            for chat_id in pending {
                send_nudge(tx, &chat_id);
            }
        } else {
            entry.clients.insert(conn_id.to_string(), tx.clone());
        }
    }

    /// Returns false when the socket must close: a frame this server cannot decode means
    /// the peer speaks a different codec, so it reconnects instead of streaming garbage.
    fn route(
        &self,
        room: &str,
        is_host: bool,
        conn_id: &str,
        from: &UnboundedSender<Out>,
        bytes: Vec<u8>,
    ) -> bool {
        let Ok((header, payload)) = decode_device_frame(&bytes) else {
            return false;
        };
        let rooms = lock(&self.rooms);
        let Some(entry) = rooms.get(room) else {
            return true;
        };
        // Only the current registration may speak for its role: a socket that a
        // reconnect superseded must not route through the live one.
        let owns_registration = if is_host {
            entry
                .host
                .as_ref()
                .is_some_and(|host| host.same_channel(from))
        } else {
            entry
                .clients
                .get(conn_id)
                .is_some_and(|client| client.same_channel(from))
        };
        if !owns_registration {
            return false;
        }
        if !is_host {
            match &entry.host {
                Some(host) => {
                    let mut routed = DeviceFrameHeader::new(header.s, header.k);
                    routed.from = Some(conn_id.to_string());
                    send_frame(host, &routed, &payload);
                }
                None => {
                    // Fail the call now rather than letting it time out behind an engine
                    // that is not connected — the controller cools down and retries.
                    let bounce = DeviceFrameHeader::new(header.s, RELAY_KIND);
                    send_frame(from, &bounce, &relay_error(HOST_OFFLINE));
                }
            }
            return true;
        }
        let Some(to) = header.to else {
            return true; // an unaddressed host frame has no destination
        };
        match entry.clients.get(&to) {
            Some(client) => {
                let stripped = DeviceFrameHeader::new(header.s, header.k);
                send_frame(client, &stripped, &payload);
            }
            None => {
                let bounce = DeviceFrameHeader::new(header.s, RELAY_KIND).with_to(to);
                send_frame(from, &bounce, &relay_error(CLIENT_GONE));
            }
        }
        true
    }

    /// Disconnect bookkeeping: whoever left is announced to the peers that still think
    /// they have a path to them.
    fn leave(&self, room: &str, is_host: bool, conn_id: &str, tx: &UnboundedSender<Out>) {
        let mut rooms = lock(&self.rooms);
        let Some(entry) = rooms.get_mut(room) else {
            return;
        };
        if is_host {
            if !entry
                .host
                .as_ref()
                .is_some_and(|host| host.same_channel(tx))
            {
                return; // already superseded by a newer host socket
            }
            entry.host = None;
            let header = DeviceFrameHeader::new("", RELAY_KIND);
            let payload = relay_error(HOST_CLOSED);
            for client in entry.clients.values() {
                send_frame(client, &header, &payload);
            }
            return;
        }
        if !entry
            .clients
            .get(conn_id)
            .is_some_and(|client| client.same_channel(tx))
        {
            return;
        }
        entry.clients.remove(conn_id);
        if let Some(host) = &entry.host {
            let mut header = DeviceFrameHeader::new("", RELAY_KIND);
            header.from = Some(conn_id.to_string());
            send_frame(host, &header, &relay_error(CLIENT_CLOSED));
        }
    }
}

/// Encode and hand a frame to one socket's writer; a dead socket is not an error here —
/// its own loop is tearing the connection down.
fn send_frame(socket: &UnboundedSender<Out>, header: &DeviceFrameHeader, payload: &[u8]) {
    if let Ok(frame) = encode_device_frame(header, payload) {
        let _ = socket.send(Out::Frame(frame));
    }
}

/// A nudge frame carries the chat id as its stream id and `{chatId}` as the
/// payload — the shape `host_session`'s `on_nudge` path expects.
fn send_nudge(socket: &UnboundedSender<Out>, chat_id: &str) {
    let payload = serde_json::json!({ "chatId": chat_id }).to_string();
    send_frame(
        socket,
        &DeviceFrameHeader::new(chat_id, NUDGE_KIND),
        payload.as_bytes(),
    );
}

/// `POST /device/{deviceId}/nudge` — "chat X has pending commands: open its
/// doc and drain". Delivered live when the host is connected, queued and
/// replayed otherwise.
///
/// Auth is the Bearer header only: this is what the engine's
/// `nudge_remote_host` sends, and unlike the sync routes the edge never
/// accepted a query token here.
pub(crate) async fn nudge_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device): Path<String>,
    body: Bytes,
) -> Response {
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|got| crate::constant_time_eq(got.as_bytes(), state.token.as_bytes()));
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let bad_chat_id = || {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_chat_id"})),
        )
            .into_response()
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return bad_chat_id();
    };
    let Some(chat_id) = body.get("chatId").and_then(|value| value.as_str()) else {
        return bad_chat_id();
    };
    // Same rule as the DO's CHAT_ID_RE (64, not the 128 a room id allows).
    if !(1..=64).contains(&chat_id.len())
        || !chat_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return bad_chat_id();
    }
    if state.device.nudge(&device, chat_id) {
        Json(serde_json::json!({"delivered": true})).into_response()
    } else {
        Json(serde_json::json!({"delivered": false, "queued": true})).into_response()
    }
}

async fn pump(mut socket: SplitSink<WebSocket, Message>, mut out: UnboundedReceiver<Out>) {
    while let Some(out) = out.recv().await {
        let message = match out {
            Out::Frame(bytes) => Message::Binary(bytes),
            Out::Keepalive => Message::Text(PONG.to_string()),
            Out::WsPong(payload) => Message::Pong(payload),
            Out::Close => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        };
        if socket.send(message).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_keepalive_is_answered_locally() {
        assert_eq!(keepalive_reply(PING), Some(PONG));
        // A non-keepalive text frame must not become a frame on the other side either.
        assert_eq!(keepalive_reply("hello"), None);
    }

    #[test]
    fn dial_query_names_the_role_and_the_connection() {
        let query = query_pairs(Some("role=client&connId=c1&token=t"));
        assert_eq!(query.get("role").map(String::as_str), Some("client"));
        assert_eq!(query.get("connId").map(String::as_str), Some("c1"));
        assert!(query_pairs(None).is_empty());
    }

    #[test]
    fn a_superseded_registration_cannot_route() {
        let relay = DeviceRelay::new();
        let (old_host, mut old_host_rx) = unbounded_channel();
        let (host, _host_rx) = unbounded_channel();
        relay.join("dev", true, "host", &old_host);
        relay.join("dev", true, "host", &host);
        assert!(
            matches!(old_host_rx.try_recv(), Ok(Out::Close)),
            "the superseded host is told to close"
        );

        let (client, mut client_rx) = unbounded_channel();
        relay.join("dev", false, "c1", &client);

        let frame = encode_device_frame(&DeviceFrameHeader::new("s", "k").with_to("c1"), b"hi")
            .expect("encode");
        assert!(
            !relay.route("dev", true, "host", &old_host, frame.clone()),
            "a superseded host socket must not speak for the room"
        );
        assert!(relay.route("dev", true, "host", &host, frame));
        assert!(
            matches!(client_rx.try_recv(), Ok(Out::Frame(_))),
            "the current host reaches the client"
        );

        let (old_client, _old_client_rx) = unbounded_channel();
        let client_frame =
            encode_device_frame(&DeviceFrameHeader::new("s", "k"), b"hi").expect("encode");
        assert!(
            !relay.route("dev", false, "c1", &old_client, client_frame),
            "a superseded client socket must not speak for its connId"
        );
    }

    #[test]
    fn a_device_room_holds_one_host_and_many_clients() {
        // The routing keys are the room's contract: `from` says which client a host
        // reply belongs to, `to` says which client a host frame is for.
        let header = DeviceFrameHeader::new("rpc", RELAY_KIND).with_to("c1");
        assert_eq!(header.to.as_deref(), Some("c1"));
        let frame = encode_device_frame(&header, &relay_error(HOST_OFFLINE)).expect("encode");
        let (decoded, payload) = decode_device_frame(&frame).expect("decode");
        assert_eq!(decoded.k, RELAY_KIND);
        assert_eq!(
            String::from_utf8(payload).expect("utf8"),
            r#"{"error":"host_offline"}"#
        );
    }
}
