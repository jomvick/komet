//! Server side: dispatch loop over string frames + the WebSocket acceptor.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HandshakeRequest, Response as HandshakeResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;

use crate::{ClientFrame, RpcError, RpcReply, RpcService, ServerFrame, ServiceRouter};

/// Serve one connection: read client frames from `inbound`, write server frames to `out`.
/// Returns when `inbound` closes; all in-flight request tasks are aborted on exit.
pub async fn serve_connection(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    mut inbound: mpsc::Receiver<String>,
) {
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    while let Some(payload) = inbound.recv().await {
        // ndjson: a transport may batch several frames per message.
        for line in payload.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let frame: ClientFrame = match serde_json::from_str(line) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "rpc: dropping malformed client frame");
                    continue;
                }
            };
            running.retain(|_, task| !task.is_finished());
            if frame.cancel {
                if let Some(task) = running.remove(&frame.id) {
                    task.abort();
                }
                continue;
            }
            let Some(method) = frame.method else {
                tracing::warn!(id = frame.id, "rpc: frame has neither method nor cancel");
                continue;
            };
            let task = tokio::spawn(handle_request(
                service.clone(),
                out.clone(),
                frame.id,
                method,
                frame.params,
            ));
            running.insert(frame.id, task.abort_handle());
        }
    }
    for (_, task) in running {
        task.abort();
    }
}

async fn handle_request(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<String>,
    id: u64,
    method: String,
    params: serde_json::Value,
) {
    let send = |frame: ServerFrame| {
        let out = out.clone();
        async move {
            match serde_json::to_string(&frame) {
                Ok(json) => out.send(json).await.map_err(|_| RpcError::Closed),
                Err(err) => {
                    tracing::error!(error = %err, "rpc: failed to serialize server frame");
                    Err(RpcError::Closed)
                }
            }
        }
    };
    match service.handle(&method, params).await {
        Ok(RpcReply::Value(value)) => {
            let _ = send(ServerFrame {
                id,
                ok: Some(value),
                ..Default::default()
            })
            .await;
        }
        Ok(RpcReply::Stream(mut stream)) => {
            while let Some(item) = stream.next().await {
                if send(ServerFrame {
                    id,
                    item: Some(item),
                    ..Default::default()
                })
                .await
                .is_err()
                {
                    return; // connection gone
                }
            }
            let _ = send(ServerFrame {
                id,
                done: true,
                ..Default::default()
            })
            .await;
        }
        Err(err) => {
            let _ = send(ServerFrame {
                id,
                err: Some(err.to_string()),
                ..Default::default()
            })
            .await;
        }
    }
}

/// Accept WebSocket connections forever, serving each with `service`. Every
/// handshake must present `token` (see [`crate::ipc_token`]).
pub async fn serve_ws_listener(
    listener: TcpListener,
    service: Arc<dyn RpcService>,
    token: Arc<str>,
) {
    serve_ws_listener_with_router(listener, Arc::new(SingleService(service)), token).await
}

/// One service per connection, chosen from the `?env=` query parameter.
///
/// The UI keeps a single IPC port and opens one connection per environment; the
/// router maps the id to that environment's service. Resolving after the handshake
/// keeps a connection for an unknown env from ever reaching a default service: it is
/// closed instead.
pub async fn serve_ws_listener_with_router(
    listener: TcpListener,
    router: Arc<dyn ServiceRouter>,
    token: Arc<str>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!(%peer, "rpc: connection accepted");
                tokio::spawn(serve_ws_socket(stream, router.clone(), token.clone()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "rpc: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// A router that serves every connection from one service (the pre-environment
/// behaviour, kept so existing call sites and tests stay unchanged).
pub struct SingleService(pub Arc<dyn RpcService>);

#[async_trait]
impl ServiceRouter for SingleService {
    async fn service_for(&self, _env: Option<String>) -> Result<Arc<dyn RpcService>, String> {
        Ok(self.0.clone())
    }
}

fn env_query(uri: &tokio_tungstenite::tungstenite::http::Uri) -> Option<String> {
    uri.query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("env="))
            .map(|value| value.to_string())
    })
}

// tungstenite's `accept_hdr_async` callback fixes the Err type as a full `Response` —
// its size is not ours to shrink.
#[allow(clippy::result_large_err)]
async fn serve_ws_socket(stream: TcpStream, router: Arc<dyn ServiceRouter>, token: Arc<str>) {
    // Native viewports dial this socket with a bare `connect_async` and send
    // no `Origin` header. A browser always attaches `Origin` to a WebSocket
    // handshake and cannot forge or suppress it from script, and WebSockets
    // are exempt from the Same-Origin Policy — so only rejecting any handshake
    // that carries `Origin` keeps a page the user happens to visit from
    // reaching this local socket. Keep this check.
    //
    // The large `Err` (ErrorResponse) is the shape tungstenite's Callback
    // trait requires; it can't be boxed away here.
    #[allow(clippy::result_large_err)]
    let mut env: Option<String> = None;
    let reject_cross_origin = |req: &HandshakeRequest, resp: HandshakeResponse| {
        env = env_query(req.uri());
        if let Some(origin) = req.headers().get("origin") {
            tracing::warn!(
                origin = %String::from_utf8_lossy(origin.as_bytes()),
                "rpc: rejecting handshake carrying an Origin header (cross-origin browser dial)"
            );
            let mut err = ErrorResponse::new(Some("origin not allowed on local IPC".to_string()));
            *err.status_mut() = StatusCode::FORBIDDEN;
            return Err(err);
        }
        // Loopback is reachable by every local user, so the caller must also
        // prove it can read this user's token file.
        let presented = req
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if !presented.is_some_and(|p| crate::ipc_token::matches(p.as_bytes(), token.as_bytes())) {
            tracing::warn!("rpc: rejecting handshake without a valid engine token");
            let mut err = ErrorResponse::new(Some("missing or invalid engine token".to_string()));
            *err.status_mut() = StatusCode::UNAUTHORIZED;
            return Err(err);
        }
        Ok(resp)
    };
    let mut ws = match tokio_tungstenite::accept_hdr_async(stream, reject_cross_origin).await {
        Ok(ws) => ws,
        Err(err) => {
            tracing::warn!(error = %err, "rpc: websocket handshake failed");
            return;
        }
    };
    let service = match router.service_for(env).await {
        Ok(service) => service,
        Err(reason) => {
            tracing::warn!(%reason, "rpc: closing connection for an unresolvable environment");
            use tokio_tungstenite::tungstenite::protocol::CloseFrame;
            use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
            let _ = ws
                .close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: reason.into(),
                }))
                .await;
            return;
        }
    };
    let (mut sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);

    // Pump: socket <-> string channels. Ends when either side closes.
    let pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(text) => {
                        if sink.send(WsMessage::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = ws_stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // ping/pong/binary — ignored
                },
            }
        }
    });

    serve_connection(service, out_tx, in_rx).await;
    pump.abort();
}
