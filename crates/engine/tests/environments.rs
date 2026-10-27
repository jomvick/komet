//! Environments end to end: a second sync server becomes a second workspace whose
//! rows never reach the local one.
//!
//! Each environment is a real `komet-sync-server` on loopback with its own data dir,
//! token and identity — the same shape `komet serve` produces on a VPS.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use komet_engine::{EngineCore, EnvAddError, EnvHosts, HarnessRegistry, Namespace};
use komet_proto::{HarnessId, PairPayload, encode_pair_link};
use komet_rpc::{memory_client, methods};

fn assemble(dir: &Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble(dir, Arc::new(HarnessRegistry::new()), HarnessId::Mock, None)
        .expect("engine assembles")
}

/// Start a sync server that reports `org_id`/`user_id` from `GET /env`.
async fn start_server(root: &Path, org_id: &str, user_id: &str, label: &str) -> String {
    std::fs::create_dir_all(root).expect("server data dir");
    std::fs::write(
        root.join("server-identity.json"),
        serde_json::json!({ "org_id": org_id, "user_id": user_id, "label": label }).to_string(),
    )
    .expect("identity file");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sync server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(komet_sync_server::serve_listener(
        listener,
        root.to_path_buf(),
        TOKEN.to_string(),
    ));
    format!("http://127.0.0.1:{port}")
}

const TOKEN: &str = "env-token";

fn link(url: &str, token: &str, label: &str) -> String {
    encode_pair_link(&PairPayload {
        v: 1,
        label: label.into(),
        url: url.into(),
        token: token.into(),
    })
}

async fn wait_until(mut check: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// Blocked by a gap *below* environments: `komet-sync-server` relays room frames but
// never answers the registry protocol's `hello` with `state`, so no engine can join a
// registry room on it (`registry handshake failed`). The test is the acceptance
// criterion for that fix, not for this branch.
#[ignore = "komet-sync-server has no registry protocol yet (see docs/superpowers/plans)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_rows_never_reach_the_local_workspace() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url_b = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    let core = assemble(&dirs.path().join("a"), "device-a");

    let env = core
        .envs
        .add(&link(&url_b, TOKEN, "VPS B"))
        .await
        .expect("add environment");
    assert_eq!(env.label, "VPS B");
    assert!(env.token.is_empty(), "RPC replies never carry the bearer");
    assert_eq!(env.namespace.as_ref().expect("namespace").org_id, "org-b");

    // The environment stays connected in the background.
    let runtime = core.envs.runtime(&env.id).expect("live runtime");
    wait_until(
        || runtime.workspace.connected(),
        "environment registry join",
    )
    .await;

    // A row written inside that environment exists there and nowhere else.
    runtime
        .workspace
        .create_chat("chat-b", None, Some("device-vps"), None, None)
        .expect("create chat in B");
    assert!(
        runtime
            .workspace
            .read_chats()
            .expect("read B chats")
            .iter()
            .any(|chat| chat.id == "chat-b")
    );
    assert!(
        !core
            .workspace
            .read_chats()
            .expect("read local chats")
            .iter()
            .any(|chat| chat.id == "chat-b"),
        "a remote chat must never surface in the local registry"
    );

    // Over RPC, the environment connection serves B's rows to the same UI code.
    let client = memory_client(core.env_service(Some(&env.id)).expect("env service"));
    let chats = client
        .subscribe(methods::WATCH_CHATS, serde_json::Value::Null)
        .await
        .expect("watch env chats");
    let mut chats = chats;
    let first = chats.recv().await.expect("first frame");
    assert!(
        first.to_string().contains("chat-b"),
        "the environment connection must read that environment's registry: {first}"
    );

    // Machine-level surfaces are refused on an environment connection.
    let err = client
        .call(methods::LIST_ENVIRONMENTS, serde_json::Value::Null)
        .await
        .expect_err("environment management is local-only");
    assert!(err.to_string().contains("local_only"), "{err}");

    // An unknown id fails instead of falling back to the local workspace.
    assert!(core.env_service(Some("nope")).is_err());

    // Pairing twice is the same environment, not a second copy of the host.
    let again = core
        .envs
        .add(&link(&url_b, TOKEN, "VPS B"))
        .await
        .expect("second pairing");
    assert_eq!(again.id, env.id);
    assert_eq!(core.envs.list().len(), 2, "local + one environment");

    // Removing stops serving it; the local workspace is not an environment.
    assert!(matches!(
        core.envs.remove("local", false).await,
        Err(komet_engine::EnvStoreError::Reserved)
    ));
    core.envs
        .remove(&env.id, false)
        .await
        .expect("remove environment");
    assert_eq!(core.envs.list().len(), 1);
    assert!(core.env_service(Some(&env.id)).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_refuses_a_bad_token_and_the_server_already_synced_to() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url_b = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    // A server whose identity matches the profile this engine serves.
    let url_own = start_server(
        &dirs.path().join("server-own"),
        komet_engine::DEFAULT_ORG_ID,
        komet_engine::DEFAULT_USER_ID,
        "same server",
    )
    .await;
    let core = assemble(&dirs.path().join("a"), "device-a");

    let err = core
        .envs
        .add(&link(&url_b, "wrong-token", "VPS B"))
        .await
        .expect_err("bad token must be refused");
    assert!(matches!(err, EnvAddError::Unauthorized), "{err:?}");
    assert_eq!(core.envs.list().len(), 1, "nothing was stored");

    let err = core
        .envs
        .add(&link(&url_own, TOKEN, "same server"))
        .await
        .expect_err("the profile's own server must be refused");
    assert!(matches!(err, EnvAddError::AlreadySyncedHere), "{err:?}");

    let err = core
        .envs
        .add("http://not-a-komet-link")
        .await
        .expect_err("a non-link must be refused");
    assert!(matches!(err, EnvAddError::Link(_)), "{err:?}");
}

#[test]
fn boot_skips_an_environment_that_cannot_open() {
    let dirs = tempfile::tempdir().expect("tempdir");
    // A stored entry with no namespace (hand-edited file, older format) must not
    // fail boot; it stays stored so re-pairing can repair it.
    std::fs::write(
        dirs.path().join("environments.json"),
        serde_json::json!({
            "v": 1,
            "environments": [{
                "id": "env-x", "label": "broken", "url": "http://127.0.0.1:1",
                "token": "t", "added_at_ms": 1
            }]
        })
        .to_string(),
    )
    .expect("environments file");
    let hosts = EnvHosts::boot(
        dirs.path(),
        "device-a",
        "laptop".into(),
        HarnessId::Mock,
        Namespace {
            org_id: "org-a".into(),
            user_id: "user-a".into(),
        },
    )
    .expect("boot tolerates a broken environment");
    assert_eq!(hosts.list().len(), 2);
    assert!(hosts.runtime("env-x").is_none());
}
