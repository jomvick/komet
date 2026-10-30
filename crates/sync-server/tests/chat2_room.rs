//! chat2 hub end-to-end against the real router: HELLO/STATE, row backfill,
//! push/ack fan-out (and no self-echo), dedupe, presence/probe, checkpoint
//! Range serving + pruning, and device nudges (live + queued replay).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use komet_sync::chat_frames::{self as wire, WireFrame, frame_type};
use komet_sync_server::{AppState, ChatHub, DeviceRelay, ServerIdentity, router};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

const TOKEN: &str = "chat2-token";
const CHAT: &str = "chat-1";
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestServer {
    base: String,
    state: AppState,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_server(data_dir: &std::path::Path) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let state = AppState {
        data_dir: data_dir.to_path_buf(),
        token: TOKEN.to_string(),
        rooms: Arc::new(RwLock::new(HashMap::new())),
        device: DeviceRelay::new(),
        chat: ChatHub::new(data_dir.to_path_buf()),
        identity: ServerIdentity {
            org_id: "org-1".into(),
            user_id: "user-1".into(),
            label: "test chat2".into(),
        },
    };
    let app_state = state.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(app_state)).await.ok();
    });
    TestServer {
        base: format!("http://127.0.0.1:{port}"),
        state,
        task,
    }
}

impl TestServer {
    fn ws_url(&self, path: &str, extra_query: &str) -> String {
        format!(
            "{}{}?token={TOKEN}{extra_query}",
            self.base.replace("http://", "ws://"),
            path
        )
    }
}

async fn dial(server: &TestServer, chat: &str, device: &str) -> Ws {
    let url = server.ws_url(&format!("/chat2/{chat}/ws"), &format!("&device={device}"));
    tokio_tungstenite::connect_async(url)
        .await
        .expect("chat2 dial")
        .0
}

async fn dial_device(server: &TestServer, device: &str, role: &str) -> Ws {
    let url = server.ws_url(&format!("/device/{device}/ws"), &format!("&role={role}"));
    tokio_tungstenite::connect_async(url)
        .await
        .expect("device dial")
        .0
}

async fn send_kind(ws: &mut Ws, kind: u8, header: serde_json::Value, payload: &[u8]) {
    ws.send(Message::Binary(wire::encode(kind, &header, payload)))
        .await
        .expect("send frame");
}

async fn send_hello(ws: &mut Ws, device: &str, cursor: u64) {
    send_kind(
        ws,
        frame_type::HELLO,
        serde_json::json!({"cursor": cursor, "device": device}),
        &[],
    )
    .await;
}

async fn recv_frame(ws: &mut Ws) -> WireFrame {
    loop {
        let message = tokio::time::timeout(FRAME_TIMEOUT, ws.next())
            .await
            .expect("timed out waiting for a chat2 frame")
            .expect("socket closed")
            .expect("websocket error");
        match message {
            Message::Binary(bytes) => return wire::decode(&bytes).expect("server frame decodes"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected websocket message: {other:?}"),
        }
    }
}

async fn recv_device_frame(ws: &mut Ws) -> (komet_rpc::DeviceFrameHeader, Vec<u8>) {
    loop {
        let message = tokio::time::timeout(FRAME_TIMEOUT, ws.next())
            .await
            .expect("timed out waiting for a device frame")
            .expect("socket closed")
            .expect("websocket error");
        match message {
            Message::Binary(bytes) => {
                return komet_rpc::decode_device_frame(&bytes).expect("device frame decodes");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected websocket message: {other:?}"),
        }
    }
}

async fn http(
    app: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"));
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
}

fn json_body(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("json body")
}

fn frontier_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn assert_kind(frame: &WireFrame, kind: u8) {
    assert_eq!(frame.kind, kind, "unexpected frame: {:?}", frame.header);
}

#[tokio::test]
async fn fresh_join_gets_state_then_rows_done() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let mut ws = dial(&server, CHAT, "dev-a").await;

    send_hello(&mut ws, "dev-a", 0).await;
    let state = recv_frame(&mut ws).await;
    assert_kind(&state, frame_type::STATE);
    assert_eq!(state.header["headSeq"], 0);
    assert_eq!(state.header["seqFloor"], 0);
    assert_eq!(state.header["checkpointSeq"], 0);
    assert_eq!(state.header["checkpointSize"], 0);
    assert_eq!(state.header["rowCount"], 0);
    assert!(state.payload.is_empty(), "no checkpoint → empty frontier");

    send_kind(
        &mut ws,
        frame_type::ROWS_REQ,
        serde_json::json!({"after": 0, "excludeOwn": false}),
        &[],
    )
    .await;
    let done = recv_frame(&mut ws).await;
    assert_kind(&done, frame_type::ROWS_DONE);
    assert_eq!(done.header["headSeq"], 0);
}

#[tokio::test]
async fn rows_req_before_hello_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let mut ws = dial(&server, CHAT, "dev-a").await;

    send_kind(
        &mut ws,
        frame_type::ROWS_REQ,
        serde_json::json!({"after": 0, "excludeOwn": false}),
        &[],
    )
    .await;
    let error = recv_frame(&mut ws).await;
    assert_kind(&error, frame_type::ERROR);
    assert_eq!(error.header["code"], "hello_first");
}

#[tokio::test]
async fn push_acks_the_sender_and_relays_to_other_sockets_without_echo() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;

    let mut sender = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut sender, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut sender).await, frame_type::STATE);

    let mut peer = dial(&server, CHAT, "dev-b").await;
    send_hello(&mut peer, "dev-b", 0).await;
    assert_kind(&recv_frame(&mut peer).await, frame_type::STATE);

    send_kind(
        &mut sender,
        frame_type::PUSH,
        serde_json::json!({"batchId": "batch-1"}),
        b"update-1",
    )
    .await;
    let ack = recv_frame(&mut sender).await;
    assert_kind(&ack, frame_type::ACK);
    assert_eq!(ack.header["batchId"], "batch-1");
    assert_eq!(ack.header["seq"], 1);
    assert_eq!(ack.header["dup"], false);

    let row = recv_frame(&mut peer).await;
    assert_kind(&row, frame_type::ROW);
    assert_eq!(row.header["seq"], 1);
    assert_eq!(row.header["device"], "dev-a");
    assert_eq!(row.header["batchId"], "batch-1");
    assert_eq!(row.payload, b"update-1");

    // No self-echo: after its ack, the sender's next frame is the probe reply,
    // not its own row coming back through the room broadcast.
    send_kind(&mut sender, frame_type::PROBE, serde_json::json!({}), &[]).await;
    let probe = recv_frame(&mut sender).await;
    assert_kind(&probe, frame_type::PROBE_OK);
    assert_eq!(probe.header["headSeq"], 1);
}

#[tokio::test]
async fn duplicate_push_acks_the_original_seq_and_broadcasts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;

    let mut sender = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut sender, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut sender).await, frame_type::STATE);

    let mut peer = dial(&server, CHAT, "dev-b").await;
    send_hello(&mut peer, "dev-b", 0).await;
    assert_kind(&recv_frame(&mut peer).await, frame_type::STATE);

    for _ in 0..2 {
        send_kind(
            &mut sender,
            frame_type::PUSH,
            serde_json::json!({"batchId": "batch-1"}),
            b"update-1",
        )
        .await;
    }
    let first = recv_frame(&mut sender).await;
    assert_kind(&first, frame_type::ACK);
    assert_eq!(first.header["seq"], 1);
    assert_eq!(first.header["dup"], false);
    let replay = recv_frame(&mut sender).await;
    assert_kind(&replay, frame_type::ACK);
    assert_eq!(replay.header["seq"], 1);
    assert_eq!(replay.header["dup"], true);

    let row = recv_frame(&mut peer).await;
    assert_kind(&row, frame_type::ROW);
    assert_eq!(row.header["seq"], 1);
    // Exactly one relay: the duplicate must not have produced a second ROW.
    send_kind(&mut peer, frame_type::PROBE, serde_json::json!({}), &[]).await;
    let probe = recv_frame(&mut peer).await;
    assert_kind(&probe, frame_type::PROBE_OK);
    assert_eq!(probe.header["headSeq"], 1);
}

#[tokio::test]
async fn rows_req_exclude_own_filters_the_hello_device() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;

    let mut a = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut a, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut a).await, frame_type::STATE);
    send_kind(
        &mut a,
        frame_type::PUSH,
        serde_json::json!({"batchId": "a-1"}),
        b"from-a",
    )
    .await;
    assert_kind(&recv_frame(&mut a).await, frame_type::ACK);

    let mut b = dial(&server, CHAT, "dev-b").await;
    send_hello(&mut b, "dev-b", 0).await;
    let state = recv_frame(&mut b).await;
    assert_eq!(state.header["headSeq"], 1);
    send_kind(
        &mut b,
        frame_type::PUSH,
        serde_json::json!({"batchId": "b-1"}),
        b"from-b",
    )
    .await;
    assert_kind(&recv_frame(&mut b).await, frame_type::ACK);
    // Drain the live relay of b's push that a received.
    assert_kind(&recv_frame(&mut a).await, frame_type::ROW);

    // a reconnecting would skip its own writes (the cursor already holds them).
    send_kind(
        &mut a,
        frame_type::ROWS_REQ,
        serde_json::json!({"after": 0, "excludeOwn": true}),
        &[],
    )
    .await;
    let row = recv_frame(&mut a).await;
    assert_kind(&row, frame_type::ROW);
    assert_eq!(row.header["seq"], 2);
    assert_eq!(row.header["device"], "dev-b");
    let done = recv_frame(&mut a).await;
    assert_kind(&done, frame_type::ROWS_DONE);
    assert_eq!(done.header["headSeq"], 2);

    // A cold reader takes everything, in order.
    send_kind(
        &mut b,
        frame_type::ROWS_REQ,
        serde_json::json!({"after": 0, "excludeOwn": false}),
        &[],
    )
    .await;
    let first = recv_frame(&mut b).await;
    assert_eq!(first.header["seq"], 1);
    assert_eq!(first.header["device"], "dev-a");
    let second = recv_frame(&mut b).await;
    assert_eq!(second.header["seq"], 2);
    assert_eq!(second.header["device"], "dev-b");
    assert_kind(&recv_frame(&mut b).await, frame_type::ROWS_DONE);
}

#[tokio::test]
async fn presence_is_relayed_to_peers_and_never_stored() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;

    let mut a = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut a, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut a).await, frame_type::STATE);
    let mut b = dial(&server, CHAT, "dev-b").await;
    send_hello(&mut b, "dev-b", 0).await;
    assert_kind(&recv_frame(&mut b).await, frame_type::STATE);

    send_kind(
        &mut a,
        frame_type::PRESENCE,
        serde_json::json!({"at": 42}),
        b"cursor-position",
    )
    .await;
    let beat = recv_frame(&mut b).await;
    assert_kind(&beat, frame_type::PRESENCE);
    assert_eq!(beat.header["device"], "dev-a");
    assert_eq!(beat.header["at"], 42);
    assert_eq!(beat.payload, b"cursor-position");

    // Never stored: a device joining later only sees the next beat.
    let mut late = dial(&server, CHAT, "dev-c").await;
    send_hello(&mut late, "dev-c", 0).await;
    let state = recv_frame(&mut late).await;
    assert_kind(&state, frame_type::STATE);
    assert_eq!(state.header["headSeq"], 0);
    send_kind(&mut late, frame_type::PROBE, serde_json::json!({}), &[]).await;
    let probe = recv_frame(&mut late).await;
    assert_kind(&probe, frame_type::PROBE_OK);
}

#[tokio::test]
async fn checkpoint_round_trips_with_range_resume_and_prunes_rows() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let mut ws = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut ws, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut ws).await, frame_type::STATE);
    for batch in ["b1", "b2"] {
        send_kind(
            &mut ws,
            frame_type::PUSH,
            serde_json::json!({"batchId": batch}),
            batch.as_bytes(),
        )
        .await;
        assert_kind(&recv_frame(&mut ws).await, frame_type::ACK);
    }

    let app = router(server.state.clone());
    let post = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=2",
        &[("x-chat2-frontier", frontier_b64(b"frontier"))],
        b"snapshot".to_vec(),
    )
    .await;
    assert_eq!(post.0, StatusCode::OK);
    let body = json_body(&post.2);
    assert_eq!(body["ok"], true);
    assert_eq!(body["seqFloor"], 2);
    assert_eq!(body["pruned"], 2);

    let full = http(&app, Method::GET, "/chat2/chat-1/checkpoint", &[], vec![]).await;
    assert_eq!(full.0, StatusCode::OK);
    assert_eq!(full.2, b"snapshot");
    assert_eq!(full.1["content-type"], "application/octet-stream");
    assert_eq!(full.1["accept-ranges"], "bytes");
    assert_eq!(full.1["x-chat2-checkpoint-seq"], "2");

    let resumed = http(
        &app,
        Method::GET,
        "/chat2/chat-1/checkpoint",
        &[("range", "bytes=2-".to_string())],
        vec![],
    )
    .await;
    assert_eq!(resumed.0, StatusCode::PARTIAL_CONTENT);
    assert_eq!(resumed.2, b"apshot");
    assert_eq!(resumed.1["content-range"], "bytes 2-7/8");

    let beyond = http(
        &app,
        Method::GET,
        "/chat2/chat-1/checkpoint",
        &[("range", "bytes=8-".to_string())],
        vec![],
    )
    .await;
    assert_eq!(beyond.0, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(beyond.1["content-range"], "bytes */8");

    // A new join sees the checkpoint metadata and the pruned log.
    let mut reader = dial(&server, CHAT, "dev-b").await;
    send_hello(&mut reader, "dev-b", 0).await;
    let state = recv_frame(&mut reader).await;
    assert_kind(&state, frame_type::STATE);
    assert_eq!(state.header["headSeq"], 2);
    assert_eq!(state.header["seqFloor"], 2);
    assert_eq!(state.header["checkpointSeq"], 2);
    assert_eq!(state.header["checkpointSize"], 8);
    assert_eq!(state.header["rowCount"], 0);
    assert_eq!(state.payload, b"frontier");
    send_kind(
        &mut reader,
        frame_type::ROWS_REQ,
        serde_json::json!({"after": 0, "excludeOwn": false}),
        &[],
    )
    .await;
    let done = recv_frame(&mut reader).await;
    assert_kind(&done, frame_type::ROWS_DONE);
    assert_eq!(done.header["headSeq"], 2);
}

#[tokio::test]
async fn checkpoint_post_validates_seq_frontier_and_floor() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let app = router(server.state.clone());
    let frontier = || vec![("x-chat2-frontier", frontier_b64(b"f"))];

    let missing_seq = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint",
        &frontier(),
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(missing_seq.0, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&missing_seq.2)["error"], "bad_seq_covered");

    for seq in ["abc", "-1"] {
        let bad = http(
            &app,
            Method::POST,
            &format!("/chat2/chat-1/checkpoint?seqCovered={seq}"),
            &frontier(),
            b"snap".to_vec(),
        )
        .await;
        assert_eq!(bad.0, StatusCode::BAD_REQUEST, "{seq}");
        assert_eq!(json_body(&bad.2)["error"], "bad_seq_covered");
    }

    let missing_frontier = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=0",
        &[],
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(missing_frontier.0, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&missing_frontier.2)["error"], "bad_frontier");

    let bad_frontier = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=0",
        &[("x-chat2-frontier", "not base64 !!".to_string())],
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(bad_frontier.0, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&bad_frontier.2)["error"], "bad_frontier");

    // An empty frontier is valid when the checkpoint covers nothing.
    let empty_uncovered = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=0",
        &[("x-chat2-frontier", String::new())],
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(empty_uncovered.0, StatusCode::OK);

    let empty = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=0",
        &frontier(),
        vec![],
    )
    .await;
    assert_eq!(empty.0, StatusCode::CONFLICT);
    assert_eq!(json_body(&empty.2)["error"], "empty");

    let ahead = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=5",
        &frontier(),
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(ahead.0, StatusCode::CONFLICT);
    assert_eq!(json_body(&ahead.2)["error"], "ahead_of_head");

    // Push two rows, cover both, then try to move the floor backwards.
    let mut ws = dial(&server, CHAT, "dev-a").await;
    send_hello(&mut ws, "dev-a", 0).await;
    assert_kind(&recv_frame(&mut ws).await, frame_type::STATE);
    for batch in ["b1", "b2"] {
        send_kind(
            &mut ws,
            frame_type::PUSH,
            serde_json::json!({"batchId": batch}),
            b"x",
        )
        .await;
        assert_kind(&recv_frame(&mut ws).await, frame_type::ACK);
    }
    // Covering rows requires a frontier: an empty one is rejected.
    let empty_covered = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=1",
        &[("x-chat2-frontier", String::new())],
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(empty_covered.0, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&empty_covered.2)["error"], "bad_frontier");
    let committed = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=2",
        &frontier(),
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(committed.0, StatusCode::OK);
    let regression = http(
        &app,
        Method::POST,
        "/chat2/chat-1/checkpoint?seqCovered=1",
        &frontier(),
        b"snap".to_vec(),
    )
    .await;
    assert_eq!(regression.0, StatusCode::CONFLICT);
    assert_eq!(json_body(&regression.2)["error"], "floor_regression");
}

#[tokio::test]
async fn tail_sidecar_round_trips_and_is_missing_before_upload() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let app = router(server.state.clone());

    let missing = http(&app, Method::GET, "/chat2/chat-1/tail", &[], vec![]).await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);

    let put = http(
        &app,
        Method::PUT,
        "/chat2/chat-1/tail",
        &[("content-type", "application/json".to_string())],
        br#"{"messages":[]}"#.to_vec(),
    )
    .await;
    assert_eq!(put.0, StatusCode::OK);

    let get = http(&app, Method::GET, "/chat2/chat-1/tail", &[], vec![]).await;
    assert_eq!(get.0, StatusCode::OK);
    assert_eq!(get.1["content-type"], "application/json");
    assert_eq!(get.2, br#"{"messages":[]}"#);
}

#[tokio::test]
async fn nudge_queues_for_an_offline_host_and_replays_on_join() {
    let dir = tempfile::tempdir().unwrap();
    let server = start_server(dir.path()).await;
    let app = router(server.state.clone());
    let body = || {
        serde_json::json!({"chatId": "chat-1"})
            .to_string()
            .into_bytes()
    };

    let queued = http(&app, Method::POST, "/device/dev-a/nudge", &[], body()).await;
    assert_eq!(queued.0, StatusCode::OK);
    assert_eq!(json_body(&queued.2)["delivered"], false);
    assert_eq!(json_body(&queued.2)["queued"], true);

    // The host joins later: the queued nudge is replayed first, in order.
    let mut host = dial_device(&server, "dev-a", "host").await;
    let (header, payload) = recv_device_frame(&mut host).await;
    assert_eq!(header.k, komet_rpc::device_room::NUDGE_KIND);
    assert_eq!(header.s, "chat-1");
    assert_eq!(json_body(&payload)["chatId"], "chat-1");

    // With the host live the nudge is delivered straight through.
    let live = http(&app, Method::POST, "/device/dev-a/nudge", &[], body()).await;
    assert_eq!(live.0, StatusCode::OK);
    assert_eq!(json_body(&live.2)["delivered"], true);
    let (header, payload) = recv_device_frame(&mut host).await;
    assert_eq!(header.k, komet_rpc::device_room::NUDGE_KIND);
    assert_eq!(json_body(&payload)["chatId"], "chat-1");

    let invalid = http(
        &app,
        Method::POST,
        "/device/dev-a/nudge",
        &[],
        serde_json::json!({"chatId": "../escape"})
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(invalid.0, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&invalid.2)["error"], "bad_chat_id");
}
