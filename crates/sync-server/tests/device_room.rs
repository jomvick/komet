//! The device room end-to-end against this server: a hosted engine registers as the
//! room's host, a controller dials the same room as a client, and ordinary RPC crosses
//! the relay. This is the transport that lets one machine drive another's agents.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use komet_rpc::device_room::HOST_OFFLINE;
use komet_rpc::{
    HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig, NudgeHandler, RpcError, RpcReply,
    RpcService, StaticToken,
};
use komet_sync_server::{AppState, DeviceRelay, ServerIdentity, router};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

const TOKEN: &str = "device-room-token";
const DEVICE: &str = "dev-a";

/// Stands in for the hosted engine: answers with a value only it could produce.
struct HostService;

#[async_trait::async_trait]
impl RpcService for HostService {
    async fn handle(
        &self,
        _method: &str,
        _params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        // The dial's readiness probe asks for harnesses before the link is cached, so
        // every method answers — with something only the hosted engine could say.
        RpcReply::value(&serde_json::json!({ "device": DEVICE }))
    }
}

struct TestServer {
    edge_url: String,
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
        identity: ServerIdentity {
            org_id: "org-1".into(),
            user_id: "user-1".into(),
            label: "test device room".into(),
        },
    };
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.ok();
    });
    TestServer {
        edge_url: format!("http://127.0.0.1:{port}"),
        task,
    }
}

fn host_token() -> Arc<StaticToken> {
    Arc::new(StaticToken(TOKEN.to_string()))
}

fn noop_nudge() -> NudgeHandler {
    Arc::new(|_| {})
}

#[tokio::test]
async fn rpc_round_trips_from_controller_to_hosted_engine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = start_server(dir.path()).await;

    let _host = HostRelay::spawn(
        HostRelayConfig::new(&server.edge_url, DEVICE, host_token()),
        Arc::new(HostService),
        noop_nudge(),
    );

    let mut config = LinkCacheConfig::new(server.edge_url.clone(), host_token());
    config.probe_timeout = Duration::from_secs(5);
    // The relay accepts the client socket before the host has registered, so a
    // dial can bounce `host_offline`; keep the retry cheap and bounded.
    config.cooldown_base = Duration::from_millis(50);
    config.cooldown_max = Duration::from_millis(100);
    let cache = LinkCache::new(config);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let client = loop {
        match cache.client(DEVICE).await {
            Ok(client) => break client,
            Err(err) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "controller never reached the hosted engine: {err}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let reply = client
        .call("whereAmI", serde_json::json!({}))
        .await
        .expect("the hosted engine answers through the relay");
    assert_eq!(reply["device"], serde_json::json!(DEVICE));
}

#[tokio::test]
async fn a_call_with_no_hosted_engine_fails_instead_of_hanging() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = start_server(dir.path()).await;

    let mut config = LinkCacheConfig::new(server.edge_url.clone(), host_token());
    config.probe_timeout = Duration::from_secs(2);
    config.cooldown_base = Duration::from_millis(100);
    config.cooldown_max = Duration::from_millis(200);
    let cache = LinkCache::new(config);

    // Nothing is hosting this device: the relay bounces `host_offline`, so the dial
    // fails fast rather than waiting out a socket that will never answer.
    let Err(err) = cache.client("nobody-home").await else {
        panic!("an offline device must not produce a live link");
    };
    let message = err.to_string();
    assert!(
        message.contains(HOST_OFFLINE) || message.contains("readiness"),
        "unexpected failure surfaced for an offline device: {message}"
    );
    assert!(
        !message.contains(TOKEN),
        "the dial error must not carry the bearer: {message}"
    );
}
