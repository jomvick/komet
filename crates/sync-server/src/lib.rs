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
    pub token: Option<String>,
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

fn check_auth(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.token else {
        return true;
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-komet-token").and_then(|v| v.to_str().ok()));
    got == Some(expected.as_str())
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

async fn get_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((chat, part)): Path<(String, String)>,
) -> impl IntoResponse {
    if !check_auth(&state, &headers) {
        return Err((StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let path = state.data_dir.join("blobs").join(&chat).join(&part);
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
    let dir = state.data_dir.join("blobs").join(&chat);
    let _ = tokio::fs::create_dir_all(&dir).await;
    let path = dir.join(&part);
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

pub async fn serve(data_dir: PathBuf, token: Option<String>, port: u16) -> anyhow::Result<()> {
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
