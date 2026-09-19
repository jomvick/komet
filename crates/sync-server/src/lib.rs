use std::{collections::HashMap, path::PathBuf, sync::Arc};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, RwLock};
use tracing::info;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: PathBuf,
    /// Shared bearer token; never empty (see [`require_token`]).
    pub token: String,
    pub rooms: Arc<RwLock<HashMap<String, Room>>>,
}

pub struct Room {
    pub tx: broadcast::Sender<Vec<u8>>,
    pub db_path: PathBuf,
}

fn room_db_path(data_dir: &std::path::Path, room: &str) -> PathBuf {
    let safe: String = room.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    data_dir.join("rooms").join(format!("{safe}.db"))
}

fn open_room_db(path: &std::path::Path) -> rusqlite::Connection {
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS frames (id INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB NOT NULL)", []).ok();
    conn
}

/// Validate the configured server token. Unset or blank tokens are refused, so
/// a server can never run without authentication by accident.
pub fn require_token(token: Option<String>) -> anyhow::Result<String> {
    let token = token.map(|t| t.trim().to_string()).unwrap_or_default();
    if token.is_empty() {
        anyhow::bail!(
            "KOMET_SYNC_TOKEN is not set. Generate one with `komet sync-init` and \
             set it on the server and on every device."
        );
    }
    Ok(token)
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> bool {
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-komet-token").and_then(|v| v.to_str().ok()));
    got.is_some_and(|got| constant_time_eq(got.as_bytes(), state.token.as_bytes()))
}

/// Compare two byte strings without stopping at the first difference, so the
/// response time does not reveal how much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"ok":true}))
}

async fn ws_handler(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Path(room): Path<String>,
) -> impl IntoResponse {
    if !check_auth(&state, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    Ok(ws.on_upgrade(move |socket| handle_socket(state, room, socket)))
}

async fn handle_socket(state: AppState, room: String, socket: WebSocket) {
    let (tx, history) = {
        let mut rooms = state.rooms.write().await;
        let entry = rooms.entry(room.clone()).or_insert_with(|| {
            let path = room_db_path(&state.data_dir, &room);
            open_room_db(&path);
            let (tx, _) = broadcast::channel(1024);
            Room { tx, db_path: path }
        });
        let conn = rusqlite::Connection::open(&entry.db_path).unwrap();
        let mut stmt = conn.prepare("SELECT data FROM frames ORDER BY id").unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap();
        let hist: Vec<Vec<u8>> = rows.filter_map(|r| r.ok()).collect();
        (entry.tx.clone(), hist)
    };
    let mut rx = tx.subscribe();
    let (mut sender, mut receiver) = socket.split();


    // replay history
    for frame in history {
        let _ = sender.send(Message::Binary(frame)).await;
    }

    let tx2 = tx.clone();
    let room2 = room.clone();
    let data_dir2 = state.data_dir.clone();
    // recv task: persist + broadcast
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.into_bytes(),
                Message::Close(_) => break,
                _ => continue,
            };
            // persist
            let path = room_db_path(&data_dir2, &room2);
            if let Ok(conn) = rusqlite::Connection::open(&path) {
                let _ = conn.execute("INSERT INTO frames (data) VALUES (?1)", [&data]);
            }
            let _ = tx2.send(data);
        }
    });

    // send task: forward broadcasts
    let mut send_task = tokio::spawn(async move {
        while let Ok(data) = rx.recv().await {
            if sender.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut recv_task => send_task.abort(),
        _ = &mut send_task => recv_task.abort(),
    }
    info!(room=%room, "client disconnected");
}

/// The file for a blob, or `None` when either segment could leave
/// `data_dir/blobs`. Accepts exactly what clients send (see `fetch_tool_blob`
/// in `crates/engine/src/doc_host.rs`): chat ids of letters, digits, `_` and
/// `-`; part ids that may also contain `.`, `:`, `#` and `~` but never start
/// with a dot, which rules out `.` and `..`. The final check also catches a
/// Windows drive prefix such as `C:name`, which `join` would treat as a new root.
fn blob_path(data_dir: &std::path::Path, chat: &str, part: &str) -> Option<PathBuf> {
    let chat_valid = (1..=128).contains(&chat.len())
        && chat
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let part_valid = (1..=200).contains(&part.len())
        && !part.starts_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:#~-".contains(&b));
    if !(chat_valid && part_valid) {
        return None;
    }
    let chat_dir = data_dir.join("blobs").join(chat);
    let path = chat_dir.join(part);
    path.starts_with(&chat_dir).then_some(path)
}

async fn get_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((chat, part)): Path<(String, String)>,
) -> impl IntoResponse {
    if !check_auth(&state, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let Some(path) = blob_path(&state.data_dir, &chat, &part) else {
        return Err((StatusCode::BAD_REQUEST, "invalid blob path"));
    };
    match tokio::fs::read(&path).await {
        Ok(b) => Ok(b.into_response()),
        Err(_) => Err((StatusCode::NOT_FOUND, "not_found")),
    }
}

async fn put_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((chat, part)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if !check_auth(&state, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let Some(path) = blob_path(&state.data_dir, &chat, &part) else {
        return Err((StatusCode::BAD_REQUEST, "invalid blob path"));
    };
    if let Some(dir) = path.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    match tokio::fs::write(&path, &body).await {
        Ok(_) => Ok(Json(serde_json::json!({"ok":true, "bytes": body.len()})).into_response()),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Box::leak(e.to_string().into_boxed_str()) as &str,
        )),
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/session/:room/ws", get(ws_handler))
        .route("/chat2/:room/ws", get(ws_handler))
        .route("/registry/:room/ws", get(ws_handler))
        .route("/device/:room/ws", get(ws_handler))
        .route("/blob/:chat/:part", get(get_blob).put(put_blob))
        .with_state(state)
}

/// Run the sync server. A blank `token` is refused here as well as in
/// [`require_token`], so no caller can start a server without authentication.
pub async fn serve(data_dir: PathBuf, token: String, port: u16) -> anyhow::Result<()> {
    let token = require_token(Some(token))?;
    tokio::fs::create_dir_all(data_dir.join("rooms")).await.ok();
    tokio::fs::create_dir_all(data_dir.join("blobs")).await.ok();
    let state = AppState {
        data_dir,
        token,
        rooms: Arc::new(RwLock::new(HashMap::new())),
    };
    let app = router(state);
    let addr = format!("0.0.0.0:{port}");
    info!("komet-sync listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";

    fn test_state(data_dir: &std::path::Path) -> AppState {
        AppState {
            data_dir: data_dir.to_path_buf(),
            token: TOKEN.to_string(),
            rooms: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn request(method: Method, uri: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body))
            .unwrap()
    }

    // Finding 1: an encoded `../` in either segment must not reach the file
    // system outside `data_dir/blobs`.
    #[tokio::test]
    async fn blob_put_rejects_paths_outside_the_blob_directory() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let app = router(test_state(&data_dir));
        let escaped = root.path().join("escaped.txt");

        for uri in [
            "/blob/..%2F..%2F/escaped.txt",
            "/blob/chat-1/..%2F..%2F..%2Fescaped.txt",
            "/blob/chat-1/.hidden",
            "/blob/chat-1/..",
        ] {
            let response = app
                .clone()
                .oneshot(request(Method::PUT, uri, "owned"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
        assert!(!escaped.exists());
    }

    #[tokio::test]
    async fn blob_get_rejects_paths_outside_the_blob_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("secret.txt"), "secret").unwrap();
        let app = router(test_state(&root.path().join("data")));

        let response = app
            .oneshot(request(Method::GET, "/blob/..%2F..%2F/secret.txt", ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // The ids real clients send still work: a uuid chat id with a tool call
    // part and its `.diff` companion (see `doc_host.rs` `upload_tool_sidecar`).
    #[tokio::test]
    async fn blob_round_trip_accepts_real_client_ids() {
        let root = tempfile::tempdir().unwrap();
        let app = router(test_state(root.path()));
        let chat = "0b7f1c4e-9d2a-4e55-8a61-3c9f0e2d7b11";

        for part in ["toolu_01ABC:call#1", "toolu_01ABC:call#1.diff"] {
            let uri = format!("/blob/{chat}/{}", part.replace('#', "%23"));
            let put = app
                .clone()
                .oneshot(request(Method::PUT, &uri, "output"))
                .await
                .unwrap();
            assert_eq!(put.status(), StatusCode::OK, "{part}");
            let get = app
                .clone()
                .oneshot(request(Method::GET, &uri, ""))
                .await
                .unwrap();
            assert_eq!(get.status(), StatusCode::OK, "{part}");
            let body = get.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(&body[..], b"output");
        }
    }

    // Finding 2: a server must never start without a real token.
    #[test]
    fn server_token_must_be_set_and_non_blank() {
        assert!(require_token(None).is_err());
        assert!(require_token(Some(String::new())).is_err());
        assert!(require_token(Some("   ".into())).is_err());
        assert_eq!(require_token(Some(" abc ".into())).unwrap(), "abc");
    }

    // Library callers that skip require_token still cannot start an open server.
    #[tokio::test]
    async fn serve_refuses_a_blank_token() {
        let root = tempfile::tempdir().unwrap();
        for token in ["", "   "] {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                serve(root.path().to_path_buf(), token.to_string(), 0),
            )
            .await
            .expect("serve must return immediately for a blank token");
            assert!(result.is_err(), "{token:?}");
        }
    }

    fn blob_get(auth: Option<(&'static str, String)>) -> Request<Body> {
        let mut builder = Request::builder().uri("/blob/chat-1/missing");
        if let Some((name, value)) = auth {
            builder = builder.header(name, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn requests_without_the_exact_token_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let app = router(test_state(root.path()));

        for auth in [
            None,
            Some(("authorization", "Bearer ".to_string())),
            Some(("x-komet-token", String::new())),
            Some(("authorization", "Bearer wrong".to_string())),
            Some(("authorization", format!("Bearer {TOKEN}x"))),
        ] {
            let response = app.clone().oneshot(blob_get(auth.clone())).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{auth:?}");
        }
        // The right token passes authentication (the blob itself is missing).
        for auth in [
            ("authorization", format!("Bearer {TOKEN}")),
            ("x-komet-token", TOKEN.to_string()),
        ] {
            let response = app.clone().oneshot(blob_get(Some(auth))).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }
}
