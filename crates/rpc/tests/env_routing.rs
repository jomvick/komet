//! `?env=` per-connection routing: one IPC port, one service per environment.

use std::sync::Arc;

use async_trait::async_trait;
use komet_rpc::{
    RpcError, RpcReply, RpcService, ServiceRouter, connect_ws_for, serve_ws_listener_with_router,
};

const TOKEN: &str = "env-routing-token";

struct Named(&'static str);

#[async_trait]
impl RpcService for Named {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            "Who" => Ok(RpcReply::Value(serde_json::json!(self.0))),
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

struct Router;

#[async_trait]
impl ServiceRouter for Router {
    async fn service_for(&self, env: Option<String>) -> Result<Arc<dyn RpcService>, String> {
        match env.as_deref() {
            None | Some("local") => Ok(Arc::new(Named("local"))),
            Some("a") => Ok(Arc::new(Named("a"))),
            Some(_) => Err("unknown environment".into()),
        }
    }
}

async fn serve() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_ws_listener_with_router(
        listener,
        Arc::new(Router),
        TOKEN.into(),
    ));
    port
}

#[tokio::test]
async fn each_connection_reaches_its_environment() {
    let port = serve().await;
    let url = format!("ws://127.0.0.1:{port}");

    let plain = connect_ws_for(&url, TOKEN, None).await.unwrap();
    assert_eq!(
        plain.call("Who", serde_json::Value::Null).await.unwrap(),
        serde_json::json!("local")
    );

    let env_a = connect_ws_for(&url, TOKEN, Some("a")).await.unwrap();
    assert_eq!(
        env_a.call("Who", serde_json::Value::Null).await.unwrap(),
        serde_json::json!("a")
    );
}

#[tokio::test]
async fn unknown_environment_closes_the_connection() {
    let port = serve().await;
    let url = format!("ws://127.0.0.1:{port}");
    let client = connect_ws_for(&url, TOKEN, Some("nope")).await.unwrap();
    let err = client
        .call("Who", serde_json::Value::Null)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RpcError::Transport(_) | RpcError::Closed),
        "unknown env must not fall back to local: {err:?}"
    );
}

#[tokio::test]
async fn the_token_still_gates_every_environment() {
    let port = serve().await;
    let url = format!("ws://127.0.0.1:{port}");
    assert!(connect_ws_for(&url, "wrong", Some("a")).await.is_err());
}
