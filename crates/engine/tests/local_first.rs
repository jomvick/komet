//! Local-first startup boundaries and captured synced-session behavior.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use komet_engine::{AuthState, Engine, EngineConfig, EngineInfo, HarnessId, WorkspaceScope};
use komet_rpc::{connect_ws, memory_client, methods};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(
    data_dir: &std::path::Path,
    edge_url: String,
    workos_client_id: Option<&str>,
    edge_token: Option<&str>,
) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        edge_url,
        edge_token: edge_token.map(str::to_string),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        workos_client_id: workos_client_id.map(str::to_string),
    }
}

async fn rejecting_edge() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                seen.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":"revoked"}"#;
                let response = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), requests, task)
}

#[tokio::test]
async fn signed_out_workos_boot_serves_local_data_without_dev_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready without auth");

    assert_eq!(scope, WorkspaceScope::Local);
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();
    let client = memory_client(runtime.core().rpc_service());
    let info: EngineInfo = client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(info.workspace_scope, WorkspaceScope::Local);
    assert!(
        client
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap()
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(dir.path().join("profiles/local").is_dir());
    assert!(!dir.path().join("orgs/dev-org/dev-user").exists());
    assert!(runtime.core().links().is_none());
    runtime.shutdown().await;
}

#[tokio::test]
async fn clean_local_auth_construction_does_not_probe_edge_health() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);

    let auth = Engine::build_auth(&config).await;

    assert_eq!(
        Engine::initial_workspace_scope(&auth),
        WorkspaceScope::Local
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    edge_task.abort();
}

#[tokio::test]
async fn local_runtime_does_not_start_the_edge_updater() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("local profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Local);
    assert!(runtime.core().links().is_none());
    assert!(
        runtime.core().updater().is_none(),
        "local runtime must not start an Edge updater"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn revoked_captured_session_stays_on_its_synced_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"dead","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, Some("client_test"), None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("persisted org resolves before refresh");

    assert!(auth.loaded_workos_session());
    assert_eq!(scope, WorkspaceScope::Synced);
    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(auth.loaded_workos_session());
    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Synced);
    assert!(dir.path().join("orgs/org_1/user_1").is_dir());
    assert!(!dir.path().join("profiles/local").exists());
    assert!(requests.load(Ordering::SeqCst) >= 1);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn transient_refresh_failure_keeps_synced_recovery_supervisors_alive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"still-valid","user":{"id":"user_1","email":"u@example.com"},"orgId":"org_1"}"#,
    )
    .unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("persisted org resolves while Edge is unavailable");

    let runtime = Engine::assemble_runtime(&config, auth.clone(), profile)
        .await
        .unwrap();

    assert_eq!(scope, WorkspaceScope::Synced);
    assert!(
        auth.state().is_signed_in(),
        "network errors are not revocation"
    );
    assert!(
        runtime.core().links().is_some(),
        "peer routing must recover without restarting the app"
    );
    assert!(
        runtime.core().updater().is_some(),
        "the Edge updater supervisor must survive an offline boot"
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn development_without_an_explicit_bearer_stays_offline() {
    let dir = tempfile::tempdir().unwrap();
    let (edge_url, requests, edge_task) = rejecting_edge().await;
    let config = config(dir.path(), edge_url, None, None);
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("development profile is ready");

    assert_eq!(scope, WorkspaceScope::Development);
    assert_eq!(auth.access_token().await.as_deref(), Some("dev-user"));
    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(
        runtime.core().links().is_none(),
        "the synthetic dev identity must not enable Edge"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    runtime.shutdown().await;
    edge_task.abort();
}

#[tokio::test]
async fn explicit_dev_bearer_keeps_online_routing_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        None,
        Some("dev-user@dev-org"),
    );
    let auth = Engine::build_auth(&config).await;
    let scope = Engine::initial_workspace_scope(&auth);
    let profile = Engine::resolve_profile(&config, &auth, scope)
        .unwrap()
        .expect("development profile is ready");

    let runtime = Engine::assemble_runtime(&config, auth, profile)
        .await
        .unwrap();

    assert_eq!(runtime.workspace_scope(), WorkspaceScope::Development);
    assert!(runtime.core().links().is_some());
    assert!(dir.path().join("orgs/dev-org/dev-user").is_dir());
    runtime.shutdown().await;
}

#[tokio::test]
async fn headless_stop_rpc_drains_the_daemon_and_releases_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut engine_config = config(
        dir.path(),
        "http://127.0.0.1:1".into(),
        Some("client_test"),
        None,
    );
    engine_config.ipc_port = port;
    let daemon = tokio::spawn(Engine::new(engine_config).run());

    let client = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(client) = connect_ws(&format!("ws://127.0.0.1:{port}")).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("headless IPC did not start");

    assert_eq!(
        client
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await
            .unwrap(),
        serde_json::json!({ "ok": true })
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("headless engine did not stop")
        .expect("headless task panicked")
        .expect("headless shutdown failed");

    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("headless IPC port remained occupied after shutdown");
}
