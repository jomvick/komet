
use super::*;
use async_trait::async_trait;
use chrono::TimeDelta;
use komet_doc::TranscriptFrame;
use komet_engine::{EngineCore, default_registry};
// `SessionStatus` is only needed to build the fixtures below — the module
// itself derives everything through `komet_proto::view`.
use crate::state::engine::{DeferredEngineState, query_engine_info};
use komet_proto::{EngineInfo, HarnessId, SessionStatus, UserProfile};
use komet_rpc::{RpcError, RpcReply, RpcService, connect_ws, memory_client};

/// A localhost port that was just free (bind :0, read, drop).
async fn free_port() -> u16 {
    if std::env::var_os("KOMET_RUNTIME_DIR").is_none() {
        let tmp_run = std::env::temp_dir().join("komet-test-run");
        let _ = std::fs::create_dir_all(&tmp_run);
        unsafe { std::env::set_var("KOMET_RUNTIME_DIR", &tmp_run) };
    }
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    socket.local_addr().unwrap().port()
}

struct LegacyIdentityRpc;

#[async_trait]
impl RpcService for LegacyIdentityRpc {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::LOCAL_DEVICE => {
                RpcReply::value(&serde_json::json!({ "deviceId": "legacy-device" }))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

struct DeferredIdentityRpc {
    engine_info: EngineInfo,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
}

#[async_trait]
impl RpcService for DeferredIdentityRpc {
    async fn handle(&self, method: &str, _params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
            methods::ENGINE_READY => {
                let mut state = self.state.clone();
                wait_for_deferred_engine(&mut state)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&serde_json::json!({ "ready": true }))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

#[tokio::test]
async fn legacy_daemon_identity_remains_usable() {
    let client = memory_client(Arc::new(LegacyIdentityRpc));

    let info = query_engine_info(&client).await.unwrap();

    assert_eq!(info.device_id, "legacy-device");
    assert_eq!(info.workspace_scope, WorkspaceScope::Synced);
    assert_eq!(
        gate_phase(
            &ConnectionStatus::Ready,
            Some(info.workspace_scope),
            Some(&AuthState::SignedOut),
        ),
        GatePhase::Ready
    );
}

/// Publish an engine token for a test server the way `serve_ipc` does, so
/// the viewport's attach path can read it. The file is removed on drop.
fn publish_engine_token(port: u16) -> komet_rpc::ipc_token::PublishedToken {
    komet_rpc::ipc_token::publish(&komet_rpc::ipc_token::default_dir().unwrap(), port).unwrap()
}

#[tokio::test]
async fn remote_viewport_treats_legacy_daemon_as_ready() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let published = publish_engine_token(port);
    let server = tokio::spawn(komet_rpc::serve_ws_listener(
        listener,
        Arc::new(LegacyIdentityRpc),
        published.token().into(),
    ));
    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .expect("legacy daemon remains attachable");

    let mut deferred = handle
        .deferred_state()
        .expect("remote viewport tracks readiness");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_for_deferred_engine(&mut deferred),
    )
    .await
    .expect("legacy readiness fallback completes")
    .expect("unknown EngineReady means the old daemon is assembled");

    handle.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn bootstrap_embeds_engine_when_port_is_free() {
    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: free_port().await,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None, // offline
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .unwrap();
    assert_eq!(handle.mode(), EngineMode::InProcess);
    assert!(matches!(
        handle
            .deferred_state()
            .expect("embedded lifecycle")
            .borrow()
            .clone(),
        DeferredEngineState::Ready
    ));
    // Same protocol over the in-memory transport: a real engine answers.
    let harnesses = handle
        .client()
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .unwrap();
    assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    handle.shutdown().await;
}

/// A device connection is built from gpui's main thread, which has no tokio
/// reactor. The transport has to land on the engine's runtime anyway — the
/// thread that asks for a device is not the thread that runs it.
#[tokio::test]
async fn env_client_builds_a_connection_without_an_ambient_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: free_port().await,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .unwrap();

    let dialer = {
        let handle = handle.clone();
        std::thread::spawn(move || futures::executor::block_on(handle.env_client(LOCAL_ENV_ID)))
    };
    let client = match dialer.join() {
        Ok(Ok(client)) => client,
        Ok(Err(err)) => panic!("device dial from a runtime-free thread failed: {err}"),
        Err(_) => panic!("the device dial panicked on a thread without a reactor"),
    };
    // The answer travels back through the test runtime; the server task the
    // dial spawned lives on the engine's runtime the whole time.
    let harnesses = client
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await;
    let harnesses = match harnesses {
        Ok(harnesses) => harnesses,
        Err(err) => panic!("environment connection built without a reactor: {err}"),
    };
    assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    handle.shutdown().await;
}

#[tokio::test]
async fn bootstrap_reports_local_assembly_failure_before_returning_a_handle() {
    let dir = tempfile::tempdir().unwrap();
    // Komet is local-first: the boot resolves the DEVELOPMENT profile, so
    // the store corruption must sit on that profile's device root.
    std::fs::create_dir_all(dir.path().join("orgs")).unwrap();
    std::fs::write(dir.path().join("orgs/dev-org"), b"not a directory").unwrap();
    let port = free_port().await;

    let error = match EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: Some("client_test".into()),
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    {
        Ok(handle) => {
            handle.shutdown().await;
            panic!("a corrupt local store must fail bootstrap")
        }
        Err(error) => error,
    };

    assert!(!format!("{error:#}").is_empty());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "failed bootstrap must release the IPC listener"
    );
}

#[tokio::test]
async fn deferred_engine_failure_remains_observable_after_early_attach() {
    let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
    state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));

    assert_eq!(
        wait_for_deferred_engine(&mut state_rx).await,
        Err("store failed".into())
    );
}

#[tokio::test]
async fn remote_viewport_observes_deferred_engine_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let published = publish_engine_token(port);
    let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
    let server = tokio::spawn(komet_rpc::serve_ws_listener(
        listener,
        Arc::new(DeferredIdentityRpc {
            engine_info: EngineInfo {
                device_id: "owner-device".into(),
                workspace_scope: WorkspaceScope::Local,
            },
            state: state_rx,
        }),
        published.token().into(),
    ));

    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .expect("second viewport attaches over IPC");
    assert!(matches!(handle.mode(), EngineMode::Remote { .. }));

    let mut deferred = handle
        .deferred_state()
        .expect("remote viewport tracks engine readiness");
    state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deferred_engine(&mut deferred),
        )
        .await
        .expect("remote readiness probe completes"),
        Err("store failed".into())
    );

    handle.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
    // The whole point of embedding-and-serving: a second viewport (the
    // terminal app) can attach to this window's engine with no setup, no
    // separate daemon, and no launch ordering.
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None, // offline
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .unwrap();
    assert_eq!(handle.mode(), EngineMode::InProcess);

    // Attach the way an external viewport would, and speak the same protocol.
    let token = komet_rpc::ipc_token::read_default(port).unwrap();
    let attached = connect_ws(&format!("ws://127.0.0.1:{port}"), &token)
        .await
        .expect("a second viewport must be able to attach");
    let harnesses = attached
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .unwrap();
    assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

    // Shutting the window down stops accepting, so the next viewport
    // starts its own engine rather than talking to closing stores.
    handle.shutdown().await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "the port must be released on shutdown"
    );
}

#[tokio::test]
async fn concurrent_bootstraps_elect_one_embedded_engine() {
    // Two viewports of one app booting at once (the Local-switch restart
    // path): both used to probe a closed port, both embedded, and one lost
    // the data-dir lock. The bootstrap gate must elect exactly one owner
    // and turn the other into a plain remote attach.
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let config = EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None, // offline
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    };
    let (a, b) = tokio::join!(
        EngineHandle::bootstrap(config.clone()),
        EngineHandle::bootstrap(config.clone()),
    );
    let a = a.expect("first viewport boots");
    let b = b.expect("second viewport boots");

    let modes = [a.mode(), b.mode()];
    assert_eq!(
        modes
            .iter()
            .filter(|mode| **mode == EngineMode::InProcess)
            .count(),
        1,
        "exactly one viewport embeds: {modes:?}"
    );
    assert_eq!(
        modes
            .iter()
            .filter(|mode| matches!(mode, EngineMode::Remote { .. }))
            .count(),
        1,
        "the other attaches over IPC: {modes:?}"
    );

    for handle in [&a, &b] {
        let mut deferred = handle.deferred_state().expect("lifecycle tracked");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_deferred_engine(&mut deferred),
        )
        .await
        .expect("readiness resolves")
        .expect("both viewports reach Ready");
    }

    b.shutdown().await;
    a.shutdown().await;
}

#[tokio::test]
async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
    // The port probe only proves *something* is listening. A process that
    // accepts TCP and never speaks WebSocket used to hang the dial forever;
    // now it times out and we embed instead, losing only the ability to
    // serve other viewports.
    let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = squatter.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .expect("a taken port must not fail the boot");
    assert_eq!(handle.mode(), EngineMode::InProcess);
    assert!(
        handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .is_ok(),
        "the window still works over its own transport"
    );
    handle.shutdown().await;
    drop(squatter);
}

#[tokio::test]
async fn production_bootstrap_opens_local_data_without_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: dir.path().to_path_buf(),
        ipc_port: free_port().await,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: Some("client_test".into()),
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .unwrap();

    // Komet is local-first: without a synced backend the boot resolves the
    // development profile (dev identity, no sign-in gate).
    assert_eq!(
        handle.engine_info().workspace_scope,
        WorkspaceScope::Development
    );
    let info: EngineInfo = handle
        .client()
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(info, *handle.engine_info());

    let mut auth = handle
        .client()
        .subscribe(methods::AUTH_STATUS, serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(
        parse_auth_state(&auth.recv().await.unwrap()),
        Some(AuthState::SignedIn { .. })
    ));
    let harnesses = handle
        .client()
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .expect("local data RPC is immediately available");
    assert!(harnesses.as_array().is_some_and(|items| !items.is_empty()));
    handle.shutdown().await;
}

#[tokio::test]
async fn bootstrap_connects_when_daemon_is_listening() {
    // Stand in for `komet headless`: an engine served over the WS IPC port.
    let daemon_dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        daemon_dir.path(),
        Arc::new(default_registry()),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let published = publish_engine_token(port);
    tokio::spawn(komet_rpc::serve_ws_listener(
        listener,
        core.rpc_service(),
        published.token().into(),
    ));

    let ui_dir = tempfile::tempdir().unwrap();
    let handle = EngineHandle::bootstrap(EngineBootConfig {
        data_dir: ui_dir.path().to_path_buf(),
        ipc_port: port,
        edge_url: "http://127.0.0.1:1".into(),
        edge_token: None,
        org_id: None,
        sync_token: None,
        default_harness: HarnessId::Mock,
        managed_engine: false,
    })
    .await
    .unwrap();
    assert_eq!(
        handle.mode(),
        EngineMode::Remote {
            url: format!("ws://127.0.0.1:{port}")
        }
    );
    assert_eq!(
        handle.engine_info().workspace_scope,
        WorkspaceScope::Development
    );
    let harnesses = handle
        .client()
        .call(methods::LIST_HARNESSES, serde_json::json!({}))
        .await
        .unwrap();
    assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    assert!(matches!(
        handle
            .client()
            .call(methods::STOP_ENGINE, serde_json::json!({}))
            .await,
        Err(RpcError::Failed(message))
            if message == format!("unknown method: {}", methods::STOP_ENGINE)
    ));
}

fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
    let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
        .unwrap()
        .to_utc();
    Chat {
        id: id.into(),
        device_id: "dev".into(),
        title: None,
        archived: false,
        pinned: false,
        cwd: None,
        branch: None,
        checkout_id: None,
        config: None,
        last_message_preview: None,
        last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
        created_at: base + TimeDelta::minutes(created_min),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: None,
        last_seen_at: None,
        room_gen: None,
    }
}

fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
    let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
        .unwrap()
        .to_utc();
    Space {
        id: id.into(),
        device_id: device_id.into(),
        path: path.into(),
        name: None,
        git_detected: false,
        git_checked_at: None,
        checkout_id: None,
        created_at: base + TimeDelta::minutes(created_min),
    }
}

fn session(
    chat_id: &str,
    status: SessionStatus,
    updated_secs_ago: i64,
    now: DateTime<Utc>,
) -> Session {
    Session {
        chat_id: chat_id.into(),
        device_id: "dev".into(),
        status,
        started_at: None,
        updated_at: now - TimeDelta::seconds(updated_secs_ago),
        context_usage: None,
    }
}

fn user_entry(id: &str) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role: komet_doc::MessageRole::User,
        parts: Vec::new(),
        created_at: 0,
        device_id: "dev".into(),
        status: None,
        continuation_of: None,
    }
}

fn device(id: &str, name: &str) -> Device {
    Device {
        id: id.into(),
        name: name.into(),
        platform: "macos".into(),
        last_seen_at: None,
        created_at: None,
        version: None,
    }
}

#[test]
fn local_workspace_hides_the_unknown_device_sentinel() {
    let mut state = AppState::new();
    state.workspace_scope = Some(WorkspaceScope::Local);
    state.local_device_id = Some("local".into());

    state.apply_devices(vec![
        device("local", "unknown-device"),
        device("remote", "unknown-device"),
    ]);

    assert_eq!(state.device_name("local"), Some("Local"));
    assert_eq!(state.device_name("remote"), Some("unknown-device"));

    state.apply_devices(vec![device("local", "José's MacBook Pro")]);
    assert_eq!(state.device_name("local"), Some("José's MacBook Pro"));
}

#[test]
fn workspace_host_is_what_the_switcher_shows() {
    assert_eq!(
        url_host("https://vm1.example.com:8443/"),
        "vm1.example.com:8443"
    );
    assert_eq!(url_host("http://192.168.1.20:8787"), "192.168.1.20:8787");
    assert_eq!(url_host("not a url"), "");
}

#[test]
fn a_paired_workspace_targets_its_own_machine_not_the_controller() {
    let devices = vec![device("vm1", "vm1"), device("other", "other")];
    assert_eq!(
        workspace_home_device(&devices, Some("fedora")).as_deref(),
        Some("vm1")
    );
    // The controller's row is not in this workspace: fall back to any device.
    assert_eq!(
        workspace_home_device(&[device("vm1", "vm1")], Some("vm1")).as_deref(),
        Some("vm1")
    );
    assert_eq!(workspace_home_device(&[], Some("vm1")), None);
}

#[test]
fn only_the_machine_under_the_app_is_trivially_online() {
    let now = Utc::now();
    let mut state = AppState::new();
    state.home_device_id = Some("fedora".into());
    // `local_device_id` follows the displayed workspace, so it must not be
    // the trivially-online answer: vm1's staleness is real information.
    state.local_device_id = Some("vm1".into());
    state.devices = vec![device("vm1", "vm1")];
    assert!(state.device_online("fedora", now));
    assert!(!state.device_online("vm1", now));
}

#[test]
fn a_late_local_device_answer_never_retargets_a_paired_workspace() {
    let mut state = AppState::new();
    state.active_workspace = "env-1".into();
    state.local_device_id = Some("vm1".into());
    state.note_home_device("fedora".into());
    assert_eq!(state.home_device_id.as_deref(), Some("fedora"));
    assert_eq!(
        state.local_device_id.as_deref(),
        Some("vm1"),
        "the controller's id must not become the displayed device's target"
    );

    state.active_workspace = LOCAL_ENV_ID.into();
    state.note_home_device("fedora".into());
    assert_eq!(state.local_device_id.as_deref(), Some("fedora"));
}

#[test]
fn a_row_reports_the_version_of_the_machine_under_it() {
    let mut vm1 = device("vm1", "vm1");
    vm1.version = Some("1.4.0".into());
    let mut laptop = device("fedora", "fedora");
    laptop.version = Some("1.3.0".into());

    // Inside a paired workspace the controller has no registry row, so the
    // row's version is that machine's — never the app's own.
    assert_eq!(
        workspace_version(&[vm1.clone()], Some("fedora")).as_deref(),
        Some("1.4.0")
    );
    assert_eq!(
        workspace_version(&[laptop, vm1], Some("fedora")).as_deref(),
        Some("1.3.0")
    );
    assert_eq!(workspace_version(&[], Some("fedora")), None);
}

#[test]
fn send_pending_overlays_working_until_ttl() {
    let now = Utc::now();
    let s_chat = chat("c", 0, Some(10)); // unseen, no session row
    let mut s = AppState::new();
    assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Completed);
    assert_eq!(s.indicator_for("c", now), Indicator::None);
    s.begin_pending_send("c", "m1", now);
    assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Working);
    assert_eq!(s.indicator_for("c", now), Indicator::Working);
    // Time-bounded: an offline host must not leave an eternal spinner.
    let later = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
    assert_eq!(
        s.display_status_for(&s_chat, later),
        ChatIndicator::Completed
    );
    assert_eq!(s.indicator_for("c", later), Indicator::None);
}

#[test]
fn send_pending_acked_when_the_host_writes_the_message_back() {
    let now = Utc::now();
    let mut s = AppState::new();
    s.selected_chat = Some("c".into());
    s.begin_pending_send("c", "m1", now);
    // A frame without the message keeps the overlay.
    s.apply_transcript(vec![user_entry("other")]);
    assert!(s.send_pending("c", now));
    // The host executed the command: our id comes back in the doc.
    s.apply_transcript(vec![user_entry("other"), user_entry("m1")]);
    assert!(!s.send_pending("c", now));
}

#[test]
fn send_failure_cleanup_only_ends_its_own_overlay() {
    let now = Utc::now();
    let mut s = AppState::new();
    s.begin_pending_send("c", "m1", now);
    s.begin_pending_send("c", "m2", now); // quick resend superseded m1
    s.end_pending_send("c", "m1"); // m1's failure cleanup arrives late
    assert!(s.send_pending("c", now), "m2's overlay must survive");
    s.end_pending_send("c", "m2");
    assert!(!s.send_pending("c", now));
}

#[test]
fn chats_sort_by_last_message_desc_with_created_fallback() {
    let mut chats = vec![
        chat("a", 0, Some(10)),
        chat("b", 5, None), // no messages → keys on created_at (+5min)
        chat("c", 1, Some(30)),
        chat("d", 40, None), // created after every message
    ];
    sort_chats(&mut chats);
    let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(order, ["d", "c", "a", "b"]);
}

#[test]
fn chat_sort_ties_are_deterministic() {
    let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
    sort_chats(&mut chats);
    assert_eq!(chats[0].id, "a");
}

#[test]
fn working_indicator_staleness() {
    let now = Utc::now();
    // Fresh working session shows.
    let fresh = session("c", SessionStatus::Working, 10, now);
    assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
    // Stale working session is suppressed — crashed backend, not eternal spinner.
    let stale = session("c", SessionStatus::Working, 46, now);
    assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
    // Exactly at the boundary still shows (strictly-older-than semantics).
    let edge = session("c", SessionStatus::Working, 45, now);
    assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
    // Future timestamps (clock skew) count as fresh.
    let skewed = session("c", SessionStatus::Working, -30, now);
    assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
}

#[test]
fn indicator_kinds() {
    let now = Utc::now();
    assert_eq!(effective_indicator(None, now), Indicator::None);
    let idle = session("c", SessionStatus::Idle, 0, now);
    assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
    // Errored is not staleness-gated: the error stays visible.
    let errored = session("c", SessionStatus::Errored, 600, now);
    assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
    let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
    assert_eq!(
        effective_indicator(Some(&awaiting), now),
        Indicator::AwaitingInput
    );
    let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
    assert_eq!(
        effective_indicator(Some(&awaiting_stale), now),
        Indicator::None
    );
}

#[test]
fn display_status_derivation() {
    let now = Utc::now();
    let mut c = chat("c", 0, Some(10));
    // Live states win regardless of seen.
    let working = session("c", SessionStatus::Working, 5, now);
    assert_eq!(
        display_status(&c, Some(&working), now),
        ChatIndicator::Working
    );
    let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
    assert_eq!(
        display_status(&c, Some(&awaiting), now),
        ChatIndicator::AwaitingInput
    );
    // Finished + unseen = Completed (no session row at all).
    assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
    // Idle session + unseen = Completed.
    let idle = session("c", SessionStatus::Idle, 5, now);
    assert_eq!(
        display_status(&c, Some(&idle), now),
        ChatIndicator::Completed
    );
    // Stale working session falls back to the seen check.
    let stale = session("c", SessionStatus::Working, 300, now);
    assert_eq!(
        display_status(&c, Some(&stale), now),
        ChatIndicator::Completed
    );
    // Seen after the last message = Idle.
    c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
    assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
    // Errored + unseen = Errored; seen clears it to Idle.
    let errored = session("c", SessionStatus::Errored, 600, now);
    assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
    c.last_seen_at = None;
    assert_eq!(
        display_status(&c, Some(&errored), now),
        ChatIndicator::Errored
    );
    // No messages at all: nothing to see — Idle.
    let fresh = chat("f", 0, None);
    assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
}

#[test]
fn active_list_sorts_by_recency_only_status_never_moves_rows() {
    let a = chat("a", 0, Some(10)); // Completed (older)
    let b = chat("b", 0, Some(20)); // Completed (newer)
    let c = chat("c", 0, Some(5)); // AwaitingInput
    let d = chat("d", 0, Some(1)); // Working
    let mut rows = vec![
        (ChatIndicator::Completed, &a),
        (ChatIndicator::Completed, &b),
        (ChatIndicator::AwaitingInput, &c),
        (ChatIndicator::Working, &d),
    ];
    sort_active(&mut rows);
    let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
    assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

    // Opening a completed session (completed → seen → idle) must NOT
    // change its position (user report: rows jumped under the pointer).
    let mut seen = vec![
        (ChatIndicator::Idle, &a),
        (ChatIndicator::Completed, &b),
        (ChatIndicator::AwaitingInput, &c),
        (ChatIndicator::Working, &d),
    ];
    sort_active(&mut seen);
    let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
    assert_eq!(order, order_after);
}

#[test]
fn tabs_order_by_creation_not_activity() {
    let a = chat("a", 5, Some(100)); // created later, very active
    let b = chat("b", 1, Some(2));
    let mut tabs = vec![&a, &b];
    sort_tabs(&mut tabs);
    let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(order, ["b", "a"]);
}

#[test]
fn apply_spaces_sorts_and_heals_selection() {
    let mut state = AppState::new();
    state.apply_spaces(vec![
        space("s2", "dev", "/b", 2),
        space("s1", "dev", "/a", 1),
    ]);
    let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["s1", "s2"]);
    // First frame auto-selects the first space.
    assert_eq!(state.selected_space.as_deref(), Some("s1"));
    state.selected_space = Some("s2".into());
    // Vanished selection heals to the first space.
    state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
    assert_eq!(state.selected_space.as_deref(), Some("s1"));
    // No spaces at all: selection clears.
    state.apply_spaces(vec![]);
    assert_eq!(state.selected_space, None);
}

#[test]
fn apply_sessions_stamps_context_usage_for_the_ring() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    let mut sess = session("c1", SessionStatus::Working, 0, Utc::now());
    sess.context_usage = Some(komet_proto::ContextUsageStats::window(
        1_000,
        10_000,
        komet_proto::ContextUsageSource::Native,
        900,
        0,
        100,
        0,
    ));
    state.apply_sessions(vec![sess]);
    let stats = state.current_context_usage();
    assert_eq!(stats.used(), 1_000);
    assert_eq!(stats.source, komet_proto::ContextUsageSource::Native);
}

#[test]
fn approximate_billed_overshoot_does_not_pin_the_ring_at_full() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    state.transcript = vec![komet_doc::SessionMessageEntry {
        id: "m1".into(),
        role: komet_doc::MessageRole::User,
        parts: vec![komet_doc::MessagePart::Text {
            id: "p1".into(),
            text: "x".repeat(40_000),
        }],
        created_at: 0,
        device_id: "d".into(),
        status: None,
        continuation_of: None,
    }];
    state.apply_context_usage(
        "c1",
        komet_proto::ContextUsageStats::window(
            131_072,
            131_072,
            komet_proto::ContextUsageSource::Approximate,
            1_800_000,
            0,
            24_000,
            0,
        ),
    );
    let stats = state.current_context_usage();
    assert_eq!(stats.source, komet_proto::ContextUsageSource::Estimated);
    assert_eq!(stats.used(), 10_000);
    assert!(stats.context_percent() < 100);
    assert_eq!(stats.input_tokens, 1_800_000);
}

#[test]
fn chats_in_space_filters_and_orders() {
    let mut state = AppState::new();
    state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
    let mut in_space_new = chat("new", 5, None);
    in_space_new.space_id = Some("s1".into());
    let mut in_space_old = chat("old", 1, Some(50)); // active but created first
    in_space_old.space_id = Some("s1".into());
    let mut other = chat("other", 2, None);
    other.space_id = Some("s2".into());
    let mut archived = chat("gone", 0, None);
    archived.space_id = Some("s1".into());
    archived.archived = true;
    let dangling = chat("dangling", 3, None); // no space id
    state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
    let ids: Vec<&str> = state
        .chats_in_space("s1")
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids, ["old", "new"]);
    // The overview shows every live-space chat (idle included) PLUS
    // project-less chats (first-class since the project selectors);
    // chats of unknown spaces stay hidden. Completed ("old") outranks
    // idle ("new"/"dangling").
    let now = Utc::now();
    let overview: Vec<&str> = state
        .overview_chats(now)
        .iter()
        .map(|(_, c)| c.id.as_str())
        .collect();
    assert_eq!(overview, ["old", "new", "dangling"]);
}

fn chat_at(id: &str, sandbox: komet_proto::SandboxLevel) -> Chat {
    let mut row = chat(id, 0, None);
    row.config = Some(komet_proto::ChatConfig {
        harness: HarnessId::ClaudeCode,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        sandbox,
        mcp_server_ids: Vec::new(),
    });
    row
}

// Scenario 4: an existing chat uses its own level, not the default.
#[test]
fn access_for_selection_prefers_the_chats_own_level() {
    use komet_proto::SandboxLevel::*;
    let chats = vec![chat_at("ro", ReadOnly), chat("legacy", 1, None)];
    // New-chat screen: the default.
    assert_eq!(
        access_for_selection(&chats, None, DangerFullAccess),
        DangerFullAccess
    );
    // Existing chat with a saved level: its own level.
    assert_eq!(
        access_for_selection(&chats, Some("ro"), DangerFullAccess),
        ReadOnly
    );
    // An existing chat without a saved configuration keeps the level such
    // chats always used, so a Full access default never reaches it.
    assert_eq!(
        access_for_selection(&chats, Some("legacy"), DangerFullAccess),
        WorkspaceWrite
    );
    // Same for a selected chat whose row has not loaded yet; the level is
    // recomputed when the row arrives.
    assert_eq!(
        access_for_selection(&chats, Some("gone"), DangerFullAccess),
        WorkspaceWrite
    );
}

#[test]
fn access_mode_follows_selection_and_remote_level_changes() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();
    state.new_chat_access = DangerFullAccess;
    state.apply_chats(vec![chat_at("a", ReadOnly)]);
    // No chat open: the default.
    assert_eq!(state.access_mode, DangerFullAccess);
    state.selected_chat = Some("a".into());
    state.sync_access_mode();
    assert_eq!(state.access_mode, ReadOnly);
    // Another device changes the chat's level: the new level applies.
    state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
    assert_eq!(state.access_mode, WorkspaceWrite);
    // A local change to the open chat's configuration applies too.
    state.apply_chat_config("a", chat_at("a", ReadOnly).config.unwrap());
    assert_eq!(state.access_mode, ReadOnly);
    // The open chat is deleted on another device: back to the default.
    state.apply_chats(Vec::new());
    assert_eq!(state.access_mode, DangerFullAccess);
}

#[test]
fn new_chat_access_chip_survives_chats_watch_frames() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();
    state.new_chat_access = WorkspaceWrite;
    state.apply_chats(vec![chat("a", 0, None)]);
    assert_eq!(state.access_mode, WorkspaceWrite);
    state.set_access_mode(DangerFullAccess);
    assert_eq!(state.access_mode, DangerFullAccess);
    // A sidebar refresh must not snap Full access back to Sandboxed.
    state.apply_chats(vec![chat("a", 0, None)]);
    assert_eq!(state.access_mode, DangerFullAccess);
}

/// Regression (2026-09-21): the composer must snapshot `access_mode`
/// BEFORE calling `select_chat` on a newly-minted chat id.
///
/// `select_chat` clears `pending_access` and calls `sync_access_mode`.
/// For a brand-new chat whose row has not yet arrived from the engine the
/// sync falls back to `new_chat_access` (WorkspaceWrite by default),
/// silently downgrading a Full Access first run even when the user had
/// explicitly clicked the chip before sending.
#[test]
fn access_mode_snapshot_before_select_chat_preserves_full_access() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();

    // User clicks the access chip to Full access on the new-chat canvas.
    state.set_access_mode(DangerFullAccess);
    assert_eq!(state.access_mode, DangerFullAccess);

    // CORRECT (fixed) order: snapshot THEN select.
    let sandbox_before = state.access_mode;

    // Simulate select_chat with a brand-new UUID that has no row yet:
    // it clears pending_access and syncs (falling back to new_chat_access).
    state.selected_chat = Some("new-uuid".into());
    state.pending_access = None;
    state.sync_access_mode(); // resolves to new_chat_access = WorkspaceWrite

    assert_eq!(state.access_mode, WorkspaceWrite); // row hasn't landed yet
    assert_eq!(sandbox_before, DangerFullAccess); // snapshot is still correct

    // BAD (old) order: snapshot AFTER select — simulates the pre-fix bug.
    state.selected_chat = None;
    state.pending_access = None;
    state.sync_access_mode();
    state.set_access_mode(DangerFullAccess);
    assert_eq!(state.access_mode, DangerFullAccess);

    state.selected_chat = Some("new-uuid-2".into());
    state.pending_access = None;
    state.sync_access_mode(); // access_mode reset to WorkspaceWrite

    let sandbox_after = state.access_mode; // taken AFTER — was the bug
    assert_eq!(
        sandbox_after, WorkspaceWrite,
        "old order reads WorkspaceWrite instead of DangerFullAccess"
    );
}

#[test]
fn access_chip_survives_watch_when_chat_config_is_not_saved() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();
    state.apply_chats(vec![chat("a", 0, None)]);
    state.selected_chat = Some("a".into());
    state.sync_access_mode();
    assert_eq!(state.access_mode, WorkspaceWrite);
    // Unknown harness: the click cannot be written, but the next message
    // still uses this level — a watch frame must not snap it back.
    state.set_access_mode(DangerFullAccess);
    state.apply_chats(vec![chat("a", 0, None)]);
    assert_eq!(state.access_mode, DangerFullAccess);
}

#[test]
fn set_chat_config_echo_survives_a_stale_chats_frame() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();
    state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
    state.selected_chat = Some("a".into());
    state.sync_access_mode();
    assert_eq!(state.access_mode, WorkspaceWrite);
    state.apply_chat_config("a", chat_at("a", DangerFullAccess).config.unwrap());
    assert_eq!(state.access_mode, DangerFullAccess);
    // Watch frame that left before the mutate landed.
    state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
    assert_eq!(state.access_mode, DangerFullAccess);
    // Engine caught up: the echo is released.
    state.apply_chats(vec![chat_at("a", DangerFullAccess)]);
    assert_eq!(state.access_mode, DangerFullAccess);
    // A later remote change is not stuck behind the echo.
    state.apply_chats(vec![chat_at("a", ReadOnly)]);
    assert_eq!(state.access_mode, ReadOnly);
}

#[test]
fn discarded_chat_config_echo_lets_the_watch_win() {
    use komet_proto::SandboxLevel::*;
    let mut state = AppState::new();
    state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
    state.selected_chat = Some("a".into());
    state.sync_access_mode();
    state.apply_chat_config("a", chat_at("a", DangerFullAccess).config.unwrap());
    state.discard_pending_chat_config();
    // Local stamp still shows the click until a watch frame arrives.
    assert_eq!(state.access_mode, DangerFullAccess);
    state.apply_chats(vec![chat_at("a", WorkspaceWrite)]);
    assert_eq!(state.access_mode, WorkspaceWrite);
}

#[test]
fn apply_chats_drops_vanished_selection() {
    let mut state = AppState::new();
    state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
    state.selected_chat = Some("a".into());
    state.transcript = vec![];
    state.apply_chats(vec![chat("b", 1, None)]);
    assert_eq!(state.selected_chat, None);
    // Still-present selection survives.
    state.selected_chat = Some("b".into());
    state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
    assert_eq!(state.selected_chat.as_deref(), Some("b"));
}

#[test]
fn apply_chat_config_stamps_the_row() {
    let mut state = AppState::new();
    state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
    let config = komet_proto::ChatConfig {
        harness: HarnessId::ClaudeCode,
        model: Some("claude-fable-5".into()),
        reasoning: Some(komet_proto::ReasoningLevel::XHigh),
        model_options: serde_json::Map::new(),
        sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
        mcp_server_ids: Vec::new(),
    };
    state.apply_chat_config("a", config.clone());
    assert_eq!(
        state.chats.iter().find(|c| c.id == "a").unwrap().config,
        Some(config)
    );
    assert!(
        state
            .chats
            .iter()
            .find(|c| c.id == "b")
            .unwrap()
            .config
            .is_none()
    );
    // Unknown chat: no-op, no panic.
    state.apply_chat_config(
        "missing",
        komet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            sandbox: komet_proto::SandboxLevel::WorkspaceWrite,
            mcp_server_ids: Vec::new(),
        },
    );
}

#[test]
fn visible_chats_filters_archived() {
    let mut state = AppState::new();
    let mut archived = chat("a", 0, Some(99));
    archived.archived = true;
    state.apply_chats(vec![archived, chat("b", 1, None)]);
    let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
    assert_eq!(visible, ["b"]);
}

#[test]
fn echoes_show_until_doc_frame_confirms() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    let echo = SessionMessageEntry {
        id: "m1".into(),
        role: komet_doc::MessageRole::User,
        parts: vec![],
        created_at: 0,
        device_id: "local".into(),
        status: None,
        continuation_of: None,
    };
    state.push_echo("c1", echo.clone());
    // Duplicate pushes dedupe.
    state.push_echo("c1", echo.clone());
    assert_eq!(state.pending_echoes().len(), 1);
    // Frames without the id keep the echo.
    state.apply_transcript(vec![]);
    assert_eq!(state.pending_echoes().len(), 1);
    // The confirming frame prunes it.
    state.apply_transcript(vec![SessionMessageEntry {
        id: "m1".into(),
        ..echo.clone()
    }]);
    assert!(state.pending_echoes().is_empty());
    // Failure path: explicit removal.
    state.push_echo(
        "c1",
        SessionMessageEntry {
            id: "m2".into(),
            ..echo.clone()
        },
    );
    state.remove_echo("c1", "m2");
    assert!(state.pending_echoes().is_empty());
    // Echoes are per chat.
    state.push_echo(
        "other",
        SessionMessageEntry {
            id: "m3".into(),
            ..echo
        },
    );
    assert!(state.pending_echoes().is_empty());
}

/// Fold a cycle's frames the way the pipeline does: `inProgress` then
/// `completed` land on ONE part (refined in place by compaction id), so a
/// thread never holds two parts for one boundary.
fn compaction_parts(
    compaction_id: &str,
    frames: &[(komet_proto::CompactionStatus, Option<&str>)],
) -> Vec<komet_doc::MessagePart> {
    let mut parts = Vec::new();
    for (status, trigger) in frames {
        komet_doc::fold_event_into_parts(
            &mut parts,
            &komet_proto::AgentEvent::ContextCompaction {
                id: compaction_id.into(),
                status: *status,
                trigger: trigger.map(str::to_owned),
                tokens_before: Some(165_000),
                tokens_after: None,
                summary: None,
                summary_delta: None,
                error: None,
            },
        );
    }
    parts
}

fn compaction_entry(
    id: &str,
    frames: &[(komet_proto::CompactionStatus, Option<&str>)],
) -> SessionMessageEntry {
    SessionMessageEntry {
        id: format!("sys-{id}"),
        role: komet_doc::MessageRole::System,
        parts: compaction_parts(id, frames),
        created_at: 0,
        device_id: "local".into(),
        status: None,
        continuation_of: None,
    }
}

#[test]
fn thread_compactions_reads_the_doc_when_the_agent_says_nothing() {
    use komet_proto::CompactionStatus::{Completed, InProgress};
    let mut state = AppState::new();
    assert_eq!(state.thread_compactions(), ThreadCompactions::default());
    // A cycle in flight is not a boundary yet — the popover must not count
    // it, only mark it live.
    state.transcript = vec![compaction_entry("c1", &[(InProgress, None)])];
    let mid = state.thread_compactions();
    assert_eq!((mid.count, mid.in_progress), (0, true));
    assert_eq!(mid.last_trigger, None, "no reason was ever reported");
    // The same cycle settling refines its part (it is not a second
    // boundary), and a second cycle adds one — with the newest reported
    // reason winning.
    state.transcript = vec![
        compaction_entry("c1", &[(InProgress, None), (Completed, Some("auto"))]),
        compaction_entry("c2", &[(Completed, Some("manual"))]),
    ];
    let done = state.thread_compactions();
    assert_eq!(done.count, 2, "two cycles, two parts");
    assert!(!done.in_progress, "the refined part is no longer in flight");
    assert_eq!(done.last_trigger.as_deref(), Some("manual"));
}

fn plan_entry(id: &str, items: Vec<komet_proto::TodoItem>) -> SessionMessageEntry {
    let mut parts = Vec::new();
    komet_doc::fold_event_into_parts(
        &mut parts,
        &komet_proto::AgentEvent::ToolCall {
            id: "acp-plan".into(),
            call: komet_proto::ToolCall::Todo { items },
        },
    );
    SessionMessageEntry {
        id: id.into(),
        role: komet_doc::MessageRole::Assistant,
        parts,
        created_at: 0,
        device_id: "local".into(),
        status: None,
        continuation_of: None,
    }
}

#[test]
fn active_plan_tracks_the_latest_todo_call() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    let first = vec![komet_proto::TodoItem {
        text: "a".into(),
        done: false,
    }];
    let last = vec![komet_proto::TodoItem {
        text: "a".into(),
        done: true,
    }];
    state.apply_transcript(vec![
        plan_entry("m1", first),
        plan_entry("m2", last.clone()),
    ]);
    assert_eq!(state.active_plan, Some(last));
}

#[test]
fn active_plan_follows_in_place_delta_frames() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    let first = vec![komet_proto::TodoItem {
        text: "a".into(),
        done: false,
    }];
    state.apply_transcript(vec![plan_entry("m1", first.clone())]);
    assert_eq!(state.active_plan, Some(first));

    // The streaming case: the live entry is replaced in place, so the
    // transcript keeps one entry and the scan must still run — the plan
    // card is driven by exactly this frame.
    let last = vec![komet_proto::TodoItem {
        text: "a".into(),
        done: true,
    }];
    state
        .apply_transcript_frame(TranscriptFrame::Delta {
            upsert: vec![komet_doc::TranscriptUpsert {
                after: None,
                entry: plan_entry("m1", last.clone()),
            }],
            append: vec![],
            remove: vec![],
            count: 1,
        })
        .expect("in-place plan frame");
    assert_eq!(state.active_plan, Some(last));

    // Removing the plan entry changes the length, which is the other
    // trigger: the cache must not stay on a snapshot no entry carries.
    state
        .apply_transcript_frame(TranscriptFrame::Delta {
            upsert: vec![],
            append: vec![],
            remove: vec!["m1".into()],
            count: 0,
        })
        .expect("plan entry removed");
    assert_eq!(state.active_plan, None);
}

#[test]
fn active_plan_clears_when_its_entry_is_truncated() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    state.apply_transcript(vec![plan_entry(
        "m1",
        vec![komet_proto::TodoItem {
            text: "a".into(),
            done: false,
        }],
    )]);
    assert!(state.active_plan.is_some());
    state.truncate_transcript_from("c1", "m1");
    assert_eq!(state.active_plan, None);
    // A later full snapshot without a plan also clears it.
    state.apply_transcript(vec![plan_entry(
        "m2",
        vec![komet_proto::TodoItem {
            text: "a".into(),
            done: true,
        }],
    )]);
    state.apply_transcript(vec![]);
    assert_eq!(state.active_plan, None);
}

#[test]
fn gate_phases() {
    assert_eq!(
        gate_phase(&ConnectionStatus::Connecting, None, None),
        GatePhase::Loading
    );
    assert_eq!(
        gate_phase(
            &ConnectionStatus::Failed {
                message: "boom".into(),
                retryable: false
            },
            None,
            None
        ),
        GatePhase::Failed("boom".into())
    );
    assert_eq!(
        gate_phase(
            &ConnectionStatus::Ready,
            Some(WorkspaceScope::Local),
            Some(&AuthState::SignedOut),
        ),
        GatePhase::Ready
    );
    assert_eq!(
        gate_phase(
            &ConnectionStatus::Ready,
            Some(WorkspaceScope::Synced),
            Some(&AuthState::SignedOut),
        ),
        GatePhase::Ready
    );
    assert_eq!(
        gate_phase(&ConnectionStatus::Ready, Some(WorkspaceScope::Synced), None),
        GatePhase::Ready
    );
    assert_eq!(
        gate_phase(
            &ConnectionStatus::Ready,
            Some(WorkspaceScope::Synced),
            Some(&AuthState::SignedOut)
        ),
        GatePhase::Ready
    );
}

#[test]
fn connection_backoff_is_bounded_and_visible() {
    // The whole chain, driven with jitter switched off (unit 0.5 → no
    // displacement): 500 ms → 1 s → 2 s → 5 s → 10 s, saturating, and it
    // STOPS. An unbounded redial loop is the "spinner that answers nothing"
    // this task exists to remove.
    let mut attempt = 0u32;
    let mut chain = Vec::new();
    while let Some((next, delay)) = next_reconnect_step(attempt, 0.5) {
        chain.push((next, delay));
        attempt = next;
    }
    assert_eq!(
        chain,
        vec![
            (1, 500),
            (2, 1_000),
            (3, 2_000),
            (4, 5_000),
            (5, 10_000),
            (6, 10_000),
        ],
        "six attempts, the last two at the ceiling"
    );
    assert_eq!(attempt, RECONNECT_MAX_ATTEMPTS);

    // ±20 %, clamped: a bad jitter source cannot leave the band.
    assert_eq!(reconnect_jittered_delay_ms(1_000, 0.0), 800);
    assert_eq!(reconnect_jittered_delay_ms(1_000, 1.0), 1_200);
    assert_eq!(reconnect_jittered_delay_ms(1_000, 7.0), 1_200);

    // The wait is EXPOSED while it runs, not just scheduled: the ordinal and
    // the deadline are what the banner counts down against, and the gate
    // stays open (no white screen) for as long as the chain has links left.
    let mut state = AppState::new();
    state.connection = ConnectionStatus::Reconnecting {
        attempt: 3,
        retry_in_ms: 2_000,
        reason: "engine connection lost".into(),
    };
    state.reconnect_attempt = 3;
    state.reconnect_deadline = Some(Utc::now() + chrono::Duration::milliseconds(2_000));
    assert_eq!(state.reconnect_attempt, 3);
    assert!(state.reconnect_deadline.is_some());
    assert_eq!(gate_phase(&state.connection, None, None), GatePhase::Ready);

    // Ceiling reached: the chain hands the Retry button to the user, and
    // `retryable: true` is what keeps that button from being a lie.
    assert_eq!(next_reconnect_step(RECONNECT_MAX_ATTEMPTS, 0.5), None);
    state.connection = ConnectionStatus::Failed {
        message: "engine connection lost".into(),
        retryable: true,
    };
    assert!(matches!(
        state.connection,
        ConnectionStatus::Failed {
            retryable: true,
            ..
        }
    ));
}

#[test]
fn reconnect_keeps_the_last_frame_and_the_echoes() {
    let mut state = AppState::new();
    state.selected_chat = Some("c1".into());
    state.connection = ConnectionStatus::Ready;
    state.transcript = vec![user_entry("m1"), user_entry("m2")];
    state.push_echo("c1", user_entry("m3"));
    state.watch_tasks.push(Task::ready(()));
    state.data_tasks.push(Task::ready(()));
    state.transcript_task = Some(Task::ready(()));
    state.workspaces = vec![WorkspaceEntry {
        id: "env-1".into(),
        label: "Work".into(),
        host: "sync.example".into(),
        status: WorkspaceStatus::Online,
        device_count: 1,
        version: None,
    }];

    // What the dropped link throws away is the pumps — nothing the user is
    // reading. (`attach_engine` re-makes them and re-subscribes the
    // transcript of the still-selected chat.)
    state.drop_engine_subscriptions();
    assert!(state.watch_tasks.is_empty());
    assert!(state.data_tasks.is_empty());
    assert!(state.transcript_task.is_none());
    assert!(state.workspaces.is_empty());
    assert_eq!(state.transcript.len(), 2, "the last frame survives the gap");
    assert_eq!(
        state.pending_echoes().len(),
        1,
        "an unconfirmed echo survives too"
    );
    assert_eq!(state.selected_chat.as_deref(), Some("c1"));

    // And the state it lands in keeps the shell up rather than revealing a
    // gate card over an empty canvas.
    state.connection = ConnectionStatus::Reconnecting {
        attempt: 1,
        retry_in_ms: 500,
        reason: "engine connection lost".into(),
    };
    assert_eq!(gate_phase(&state.connection, None, None), GatePhase::Ready);
}

#[test]
fn paired_environment_reconnect_uses_the_same_states() {
    let mut state = AppState::new();
    state.connection = ConnectionStatus::Ready;
    let work = WorkspaceEntry {
        id: "env-1".into(),
        label: "Work".into(),
        host: "sync.example".into(),
        status: WorkspaceStatus::Online,
        device_count: 1,
        version: None,
    };

    // Local workspace: the engine link IS the status.
    assert_eq!(state.link_status(), ConnectionStatus::Ready);
    state.connection = ConnectionStatus::Reconnecting {
        attempt: 2,
        retry_in_ms: 1_000,
        reason: "engine connection lost".into(),
    };
    assert!(matches!(
        state.link_status(),
        ConnectionStatus::Reconnecting { attempt: 2, .. }
    ));

    // A paired environment speaks the SAME vocabulary — the banner needs no
    // second code path — and never borrows the local engine's health to
    // look alive: that is how one environment's rows could show up as
    // another's.
    state.active_workspace = "env-1".into();
    state.workspaces = vec![work.clone()];
    state.connection = ConnectionStatus::Ready;
    state.workspaces[0].status = WorkspaceStatus::Connecting;
    assert_eq!(state.link_status(), ConnectionStatus::Connecting);
    state.workspaces[0].status = WorkspaceStatus::Offline;
    assert_eq!(
        state.link_status(),
        ConnectionStatus::Degraded {
            reason: "Work is offline".into()
        }
    );
    // Reachable environment: the engine's own state shows through again.
    // Keep the reconnect banner alive until the runtime has actually
    // recovered, then drop back to Ready as soon as the engine reports it.
    state.workspaces[0].status = WorkspaceStatus::Online;
    state.connection = ConnectionStatus::Reconnecting {
        attempt: 2,
        retry_in_ms: 1_000,
        reason: "engine connection lost".into(),
    };
    assert!(matches!(
        state.link_status(),
        ConnectionStatus::Reconnecting { .. }
    ));
    state.connection = ConnectionStatus::Ready;
    assert_eq!(state.link_status(), ConnectionStatus::Ready);
}

#[test]
fn auth_changes_do_not_change_a_local_runtime_scope_or_watches() {
    let mut state = AppState::new();
    state.workspace_scope = Some(WorkspaceScope::Local);
    state.watch_tasks.push(Task::ready(()));

    state.apply_auth(AuthState::NeedsOrganization {
        user: UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        },
    });
    assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
    assert_eq!(state.watch_tasks.len(), 1);

    state.apply_auth(AuthState::SignedIn {
        user: UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        },
        org_id: Some("org-1".into()),
    });
    assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
    assert_eq!(state.watch_tasks.len(), 1);
}

#[test]
fn auth_frames_parse_both_wire_shapes() {
    // Proto shape.
    let proto = serde_json::json!({ "state": "signedOut" });
    assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
    // Engine shape (`_tag`, PascalCase, orgId).
    let engine = serde_json::json!({
        "_tag": "SignedIn",
        "user": { "id": "u1", "email": "w@example.com" },
        "orgId": "org-1",
    });
    let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
        panic!("expected SignedIn");
    };
    assert_eq!(user.email, "w@example.com");
    assert_eq!(org_id.as_deref(), Some("org-1"));
    let needs = serde_json::json!({
        "_tag": "NeedsOrganization",
        "user": { "id": "u1", "email": "w@example.com", "name": "W" },
    });
    assert!(matches!(
        parse_auth_state(&needs),
        Some(AuthState::NeedsOrganization { .. })
    ));
    // Garbage → None (frame dropped, not a crash).
    assert_eq!(
        parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
        None
    );
    assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
}

fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
    let mut c = chat(id, created_min, None);
    c.cwd = cwd.map(str::to_string);
    c
}

#[test]
fn project_labels_from_cwd() {
    assert_eq!(project_label(Some("/home/w/dev/komet")), "komet");
    assert_eq!(project_label(Some("/home/w/dev/komet/")), "komet");
    assert_eq!(project_label(None), "No project");
    assert_eq!(project_label(Some("   ")), "No project");
    assert_eq!(project_label(Some("/")), "/");
}

#[test]
fn grouped_sidebar_preserves_recency_order() {
    // Input is sidebar-sorted (most recent first).
    let chats = [
        chat_with_cwd("a", 9, Some("/dev/komet")),
        chat_with_cwd("b", 8, Some("/dev/zed")),
        chat_with_cwd("c", 7, Some("/dev/komet")),
        chat_with_cwd("d", 6, None),
    ];
    let groups = group_chats(chats.iter());
    let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
    // Groups ordered by their most recent chat; rows keep order.
    assert_eq!(labels, ["komet", "zed", "No project"]);
    let komet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(komet_ids, ["a", "c"]);
    assert!(group_chats(std::iter::empty()).is_empty());
}

#[test]
fn relative_times_match_komet_format() {
    let now = Utc::now();
    let ago = |secs: i64| now - chrono::Duration::seconds(secs);
    assert_eq!(format_time_ago(ago(0), now), "now");
    assert_eq!(format_time_ago(ago(59), now), "now");
    assert_eq!(format_time_ago(ago(60), now), "1m");
    assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
    assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
    assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
    assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
    assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
    assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
    assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
    assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
    assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
    // Clock skew (future timestamps) clamps to "now".
    assert_eq!(
        format_time_ago(now + chrono::Duration::hours(2), now),
        "now"
    );
}

#[test]
fn chat_location_joins_project_and_branch() {
    let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
    c.branch = Some("komet/rebalance".into());
    assert_eq!(
        chat_location(&c).as_deref(),
        Some("soccertcg · komet/rebalance")
    );
    c.branch = None;
    assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
    c.cwd = None;
    c.branch = Some("main".into());
    assert_eq!(chat_location(&c).as_deref(), Some("main"));
    c.branch = Some("   ".into());
    assert_eq!(chat_location(&c), None);
    c.branch = None;
    assert_eq!(chat_location(&c), None);
}

#[test]
fn org_gate_reducers() {
    assert!(org_name_valid("Acme"));
    assert!(org_name_valid("  padded  "));
    assert!(!org_name_valid(""));
    assert!(!org_name_valid("   "));
    assert!(!org_name_valid(&"x".repeat(65)));

    let rows = parse_orgs(&serde_json::json!({ "orgs": [
        { "id": "m2", "organizationId": "o2", "name": "beta" },
        { "id": "m1", "organizationId": "o1", "name": "Alpha" },
        { "id": "m3", "organizationId": "o1", "name": "Alpha" },
    ]}));
    assert_eq!(rows.len(), 3);
    let sorted = sort_memberships(rows);
    let names: Vec<&str> = sorted.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        ["Alpha", "beta"],
        "case-insensitive sort + dedupe by org id"
    );
    // Bare-array replies parse too; garbage yields empty.
    assert_eq!(
        parse_orgs(&serde_json::json!([{ "id": "m", "organizationId": "o", "name": "n" }])).len(),
        1
    );
    assert!(parse_orgs(&serde_json::json!("nope")).is_empty());
}
