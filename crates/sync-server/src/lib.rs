use anyhow::Context;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::{RwLock, broadcast};
use tracing::{error, info, warn};

/// Frames kept for a slow subscriber before it is told to resynchronize.
const ROOM_BROADCAST_CAPACITY: usize = 1024;
/// Largest blob `PUT /blob/:chat/:part` accepts. Tool outputs are uploaded
/// here whole, and axum's 2 MiB default rejected anything larger with a 413.
const BLOB_MAX_BYTES: usize = 32 * 1024 * 1024;

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
    let safe: String = room
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir.join("rooms").join(format!("{safe}.db"))
}

/// Open (and initialize) a room database. Returns the error instead of
/// panicking: a busy, full or read-only file system must close one socket, not
/// take the process down.
fn open_room_db(path: &std::path::Path) -> anyhow::Result<rusqlite::Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn =
        rusqlite::Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS frames (id INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB NOT NULL)",
        [],
    )
    .with_context(|| format!("initializing {}", path.display()))?;
    Ok(conn)
}

/// Read every persisted frame of a room. Blocking: always call it from
/// `spawn_blocking`, never on the reactor.
fn read_room_history(db_path: &std::path::Path) -> anyhow::Result<Vec<Vec<u8>>> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    let mut stmt = conn
        .prepare("SELECT data FROM frames ORDER BY id")
        .with_context(|| format!("reading {}", db_path.display()))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .with_context(|| format!("reading {}", db_path.display()))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Append one frame to a room database. Blocking, same rule as
/// [`read_room_history`].
fn append_room_frame(db_path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;
    conn.execute("INSERT INTO frames (data) VALUES (?1)", [data])
        .with_context(|| format!("appending to {}", db_path.display()))?;
    Ok(())
}

/// Close a socket with a reason instead of dropping it silently.
async fn close_socket(socket: WebSocket, reason: &'static str) {
    let (mut sender, _receiver) = socket.split();
    let _ = sender
        .send(Message::Close(Some(CloseFrame {
            code: 1011,
            reason: reason.into(),
        })))
        .await;
    let _ = sender.close().await;
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

/// True when `url` would send the sync bearer token over plain HTTP to a
/// non-local host. `https://` and local dev (`localhost`, `127.0.0.1`,
/// `::1`) are fine; anything else over `http://` leaks the token on the
/// network path (issue #24).
pub fn sync_url_is_insecure(url: &str) -> bool {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        _ => return false,
    };
    let host = rest
        .split_once(['/', '?', '#'])
        .map(|(h, _)| h)
        .unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.split_once(']'))
        .map(|(h, _)| h)
        .unwrap_or_else(|| host.split_once(':').map(|(h, _)| h).unwrap_or(host));
    let host = host.trim().to_lowercase();
    !(host.is_empty() || host == "localhost" || host == "127.0.0.1" || host == "::1")
}

/// Warn when a token would be sent over insecure HTTP. LAN setups can opt
/// in explicitly with `KOMET_SYNC_ALLOW_INSECURE_HTTP=1`.
pub fn warn_if_insecure_sync_url(url: &str) {
    if !sync_url_is_insecure(url) {
        return;
    }
    if std::env::var("KOMET_SYNC_ALLOW_INSECURE_HTTP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        tracing::warn!(
            url = url,
            "sync token sent over plain HTTP (KOMET_SYNC_ALLOW_INSECURE_HTTP=1); prefer HTTPS via a reverse proxy"
        );
        return;
    }
    tracing::warn!(
        url = url,
        "KOMET_SYNC_TOKEN will be sent over plain HTTP which anyone on the network can read. \
         Put the sync server behind TLS (see docs/self-hosted-sync.md) or set \
         KOMET_SYNC_ALLOW_INSECURE_HTTP=1 for trusted LAN use."
    );
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
    // The room table lock is held only to look the entry up (and to create it),
    // never across the history read: that read is disk-bound and would
    // otherwise freeze every other room's connections for its duration.
    let (tx, db_path) = {
        let mut rooms = state.rooms.write().await;
        match rooms.get(&room).map(|r| (r.tx.clone(), r.db_path.clone())) {
            Some(existing) => existing,
            None => {
                let path = room_db_path(&state.data_dir, &room);
                if let Err(err) = open_room_db(&path) {
                    drop(rooms);
                    warn!(room = %room, error = %err, "closing socket: the room database cannot be opened");
                    close_socket(socket, "room database unavailable").await;
                    return;
                }
                let (tx, _) = broadcast::channel(ROOM_BROADCAST_CAPACITY);
                rooms.insert(
                    room.clone(),
                    Room {
                        tx: tx.clone(),
                        db_path: path.clone(),
                    },
                );
                (tx, path)
            }
        }
    };
    let history_path = db_path.clone();
    let history = match tokio::task::spawn_blocking(move || read_room_history(&history_path)).await
    {
        Ok(Ok(history)) => history,
        Ok(Err(err)) => {
            warn!(room = %room, error = %err, "closing socket: room history is unreadable");
            close_socket(socket, "room database unavailable").await;
            return;
        }
        Err(join_error) => {
            warn!(room = %room, error = %join_error, "closing socket: the history task failed");
            close_socket(socket, "room database unavailable").await;
            return;
        }
    };
    let mut rx = tx.subscribe();
    let (mut sender, mut receiver) = socket.split();

    // replay history
    for frame in history {
        let _ = sender.send(Message::Binary(frame)).await;
    }

    let tx2 = tx.clone();
    let room2 = room.clone();
    let write_path = db_path.clone();
    // recv task: persist + broadcast
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.into_bytes(),
                Message::Close(_) => break,
                _ => continue,
            };
            // Persist off the reactor, through the room's own database path.
            let path = write_path.clone();
            let payload = data.clone();
            if let Ok(Err(err)) =
                tokio::task::spawn_blocking(move || append_room_frame(&path, &payload)).await
            {
                warn!(room = %room2, error = %err, "frame not persisted");
            }
            let _ = tx2.send(data);
        }
    });

    // send task: forward broadcasts
    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    if sender.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                // A subscriber that fell ROOM_BROADCAST_CAPACITY frames behind
                // used to be disconnected silently. Tell it to reconnect (and
                // replay the persisted history) instead of dropping frames.
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    warn!(
                        dropped,
                        "sync subscriber lagged; asking it to resynchronize"
                    );
                    let _ = sender
                        .send(Message::Close(Some(CloseFrame {
                            code: 1011,
                            reason: "lagged".into(),
                        })))
                        .await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
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
    // Write to a sibling temp file and rename: a truncated download must never
    // replace a complete blob. The temp name is unique per request so two
    // uploads of the same part cannot clobber each other's staging file.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err((StatusCode::BAD_REQUEST, "invalid blob path"));
    };
    let staged = path.with_file_name(format!("{name}.tmp-{}", uuid::Uuid::new_v4()));
    if let Err(err) = tokio::fs::write(&staged, &body).await {
        error!(path = %staged.display(), error = %err, "staged blob write failed");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "blob write failed"));
    }
    if let Err(err) = tokio::fs::rename(&staged, &path).await {
        let _ = tokio::fs::remove_file(&staged).await;
        error!(path = %path.display(), error = %err, "blob rename failed");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "blob write failed"));
    }
    Ok(Json(serde_json::json!({"ok":true, "bytes": body.len()})).into_response())
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/session/:room/ws", get(ws_handler))
        .route("/chat2/:room/ws", get(ws_handler))
        .route("/registry/:room/ws", get(ws_handler))
        .route("/device/:room/ws", get(ws_handler))
        .route(
            "/blob/:chat/:part",
            get(get_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::max(BLOB_MAX_BYTES)),
        )
        .with_state(state)
}

/// Run the sync server. A blank `token` is refused here as well as in
/// [`require_token`], so no caller can start a server without authentication.
pub async fn serve(data_dir: PathBuf, token: String, port: u16) -> anyhow::Result<()> {
    let token = require_token(Some(token))?;
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating the data directory {}", data_dir.display()))?;
    // Fail at boot, not at the first connection: a data directory the server
    // cannot write (a Docker volume created by root, a read-only mount) used to
    // start anyway and then refuse every room, with no explanation.
    let probe = data_dir.join(".write-probe");
    tokio::fs::write(&probe, b"").await.with_context(|| {
        format!(
            "the data directory {} is not writable — fix its owner/permissions \
             (the container runs as uid 10001)",
            data_dir.display()
        )
    })?;
    let _ = tokio::fs::remove_file(&probe).await;
    for sub in ["rooms", "blobs"] {
        let dir = data_dir.join(sub);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
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

    fn request_bytes(method: Method, uri: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::from(body))
            .unwrap()
    }

    // SYN-04: an unusable database path must be reported, not panic.
    #[test]
    fn opening_a_room_database_reports_errors_instead_of_panicking() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").unwrap();
        let error = open_room_db(&blocker.join("room.db")).unwrap_err();
        assert!(
            error.to_string().contains("creating") || error.to_string().contains("opening"),
            "{error}"
        );
    }

    // SYN-08: a blob is published by rename, so no staging file survives and a
    // failed upload can never truncate the blob already in place.
    #[tokio::test]
    async fn blob_put_is_atomic_and_leaves_no_staging_files() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let app = router(test_state(&data_dir));
        let uri = "/blob/chat-1/toolu_01%231";

        for body in ["first", "second"] {
            let response = app
                .clone()
                .oneshot(request(Method::PUT, uri, body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let mut entries: Vec<String> = std::fs::read_dir(data_dir.join("blobs/chat-1"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["toolu_01#1".to_string()]);
        assert_eq!(
            std::fs::read_to_string(data_dir.join("blobs/chat-1/toolu_01#1")).unwrap(),
            "second"
        );
    }

    // SYN-12: axum's 2 MiB default body limit rejected every larger tool upload.
    #[tokio::test]
    async fn blobs_larger_than_the_default_body_limit_are_accepted() {
        let root = tempfile::tempdir().unwrap();
        let app = router(test_state(root.path()));
        let body = vec![b'x'; 3 * 1024 * 1024];

        let put = app
            .clone()
            .oneshot(request_bytes(Method::PUT, "/blob/chat-1/big", body))
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK);
        let get = app
            .oneshot(request(Method::GET, "/blob/chat-1/big", ""))
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let stored = get.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(stored.len(), 3 * 1024 * 1024);
    }

    // SYN-11: an unwritable data directory (root-owned Docker volume) must stop
    // the server at boot instead of failing on every later connection.
    #[cfg(unix)]
    #[tokio::test]
    async fn serve_fails_fast_when_the_data_directory_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve(data_dir.clone(), TOKEN.to_string(), 0),
        )
        .await
        .expect("serve must fail fast on an unwritable data directory");

        // Restore before asserting so the tempdir can always be cleaned up.
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.expect_err("an unwritable data directory is a boot error");
        assert!(error.to_string().contains("not writable"), "{error}");
    }

    #[test]
    fn insecure_url_detection() {
        assert!(!sync_url_is_insecure("https://example.com:8787"));
        assert!(!sync_url_is_insecure("http://localhost:8787"));
        assert!(!sync_url_is_insecure("http://127.0.0.1:8787"));
        assert!(!sync_url_is_insecure("http://[::1]:8787"));
        assert!(sync_url_is_insecure("http://192.168.1.10:8787"));
        assert!(sync_url_is_insecure("http://vps.example.com:8787"));
    }
}
