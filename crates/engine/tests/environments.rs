//! Environments end to end: a second sync server becomes a second workspace whose
//! rows never reach the local one.
//!
//! Each environment is a real `komet-sync-server` on loopback with its own data dir,
//! token and identity — the same shape `komet serve` produces on a VPS.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use komet_engine::{EngineCore, EnvAddError, EnvHosts, HarnessRegistry, Namespace};
use komet_proto::{Environment, HarnessId, PairPayload, decode_pair_link, encode_pair_link};
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

    // A second controller (another machine's engine) gets that row from the
    // server's `state`, not from any local write: the room persisted it.
    let other = assemble(&dirs.path().join("other"), "device-other");
    let seen = other
        .envs
        .add(&link(&url_b, TOKEN, "VPS B"))
        .await
        .expect("second controller pairs");
    let other_runtime = other.envs.runtime(&seen.id).expect("second runtime");
    wait_until(
        || other_runtime.workspace.connected(),
        "second registry join",
    )
    .await;
    wait_until(
        || {
            other_runtime
                .workspace
                .read_chats()
                .is_ok_and(|chats| chats.iter().any(|chat| chat.id == "chat-b"))
        },
        "chat-b to arrive from the server state",
    )
    .await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_environments_streams_the_paired_list() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url_b = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    let core = assemble(&dirs.path().join("a"), "device-a");

    // Environment management is this machine's surface, so the switcher's stream
    // lives on the local connection — an environment connection would refuse it.
    let client = memory_client(core.rpc_service());
    let mut frames = client
        .subscribe(methods::WATCH_ENVIRONMENTS, serde_json::Value::Null)
        .await
        .expect("watch environments");

    let frame = tokio::time::timeout(Duration::from_secs(10), frames.recv())
        .await
        .expect("first listing before timeout")
        .expect("stream alive");
    let listed: Vec<Environment> = serde_json::from_value(frame).expect("environments array");
    assert_eq!(listed.len(), 1, "only this computer before any pairing");
    assert_eq!(listed[0].id, komet_engine::environments::LOCAL_ENV_ID);

    core.envs
        .add(&link(&url_b, TOKEN, "VPS B"))
        .await
        .expect("add environment");

    let frame = tokio::time::timeout(Duration::from_secs(10), frames.recv())
        .await
        .expect("a frame after the pairing")
        .expect("stream alive");
    let listed: Vec<Environment> = serde_json::from_value(frame).expect("environments array");
    let paired = listed
        .iter()
        .find(|env| env.id != komet_engine::environments::LOCAL_ENV_ID)
        .expect("the paired device reaches the stream");
    assert_eq!(paired.label, "VPS B");
    assert_eq!(paired.url, url_b, "the host is known to the switcher");
    assert!(
        paired.token.is_empty() && !frame_text_has_token(&listed),
        "no listing frame ever carries the bearer"
    );
}

/// The bearer is a shell grant on the host: it must not appear in any listing,
/// even serialized.
fn frame_text_has_token(envs: &[Environment]) -> bool {
    envs.iter().any(|env| env.token == TOKEN)
}

/// "Copy link" is re-encoded by the engine: every other reply is redacted, so a
/// link the UI built from its own rows would silently carry no bearer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_link_re_encodes_a_working_link_for_the_local_connection() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url_b = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    let core = assemble(&dirs.path().join("a"), "device-a");
    let env = core
        .envs
        .add(&link(&url_b, TOKEN, "VPS B"))
        .await
        .expect("add environment");

    let local = memory_client(core.rpc_service());
    let answer = local
        .call(
            methods::ENVIRONMENT_LINK,
            serde_json::json!({ "id": env.id }),
        )
        .await
        .expect("a paired environment has a link");
    let raw = answer
        .get("link")
        .and_then(|v| v.as_str())
        .expect("link field");
    let payload = decode_pair_link(raw).expect("the copied link decodes");
    assert_eq!(payload.token, TOKEN, "the copy pastes as a working link");
    assert_eq!(payload.url, url_b);
    assert_eq!(payload.label, "VPS B");

    // The rows the page renders from stay redacted — this method is the one
    // deliberate carry of the secret.
    let listed = local
        .call(methods::LIST_ENVIRONMENTS, serde_json::Value::Null)
        .await
        .expect("list environments");
    assert!(
        !listed.to_string().contains(TOKEN),
        "a listing reply must never carry the bearer"
    );

    // No link for a host we don't have, and none for this computer: the local
    // row is a switcher entry, not an environment store record.
    for id in ["nope", komet_engine::environments::LOCAL_ENV_ID] {
        let err = local
            .call(methods::ENVIRONMENT_LINK, serde_json::json!({ "id": id }))
            .await
            .expect_err("neither id is a pairable environment");
        assert!(err.to_string().contains("unknown_environment"), "{err}");
    }

    let inside = memory_client(core.env_service(Some(&env.id)).expect("env service"));
    let err = inside
        .call(
            methods::ENVIRONMENT_LINK,
            serde_json::json!({ "id": env.id }),
        )
        .await
        .expect_err("a remote workspace does not hand out machine secrets");
    assert!(err.to_string().contains("local_only"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_environment_previews_without_pairing() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url_b = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    let core = assemble(&dirs.path().join("a"), "device-a");
    let client = memory_client(core.rpc_service());

    // The wizard's first step: answer from the link alone, store nothing.
    let preview = client
        .call(
            methods::PREVIEW_ENVIRONMENT,
            serde_json::json!({ "link": link(&url_b, TOKEN, "VPS B") }),
        )
        .await
        .expect("preview the link");
    assert_eq!(preview["label"], "VPS B");
    assert_eq!(preview["url"], url_b);
    assert!(
        !preview.to_string().contains(TOKEN),
        "a preview reply never echoes the bearer"
    );
    assert_eq!(
        core.envs.list().len(),
        1,
        "a preview must not pair anything"
    );

    // A malformed link gets the typed code the settings page branches on — and
    // the reply must not quote back what was pasted.
    let err = client
        .call(
            methods::PREVIEW_ENVIRONMENT,
            serde_json::json!({ "link": "komet://pair/not-base64" }),
        )
        .await
        .expect_err("a bad link is refused");
    assert!(err.to_string().contains("invalid_link"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_keeps_the_mirror_unless_the_purge_is_asked_for() {
    // Two hosts, same stored entry: the mirror survives a plain removal (it is
    // the last copy when the server itself goes away) and only a purge deletes it.
    let kept = tempfile::tempdir().expect("tempdir");
    let purged = tempfile::tempdir().expect("tempdir");

    /// Open the stored environment and seed its mirror with a file, so `purge`
    /// has something to take whatever the docs store wrote on its own.
    fn boot_and_seed(root: &Path) -> (EnvHosts, PathBuf) {
        std::fs::write(
            root.join("environments.json"),
            serde_json::json!({
                "v": 1,
                "environments": [{
                    "id": "env-x", "label": "VPS X", "url": "http://127.0.0.1:1",
                    "token": "env-token",
                    "namespace": { "org_id": "org-x", "user_id": "user-x" },
                    "added_at_ms": 1
                }]
            })
            .to_string(),
        )
        .expect("environments file");
        let hosts = EnvHosts::boot(
            root,
            "device-a",
            "laptop".into(),
            HarnessId::Mock,
            Namespace {
                org_id: "org-a".into(),
                user_id: "user-a".into(),
            },
        );
        assert_eq!(hosts.list().len(), 2, "local + the stored environment");
        let mirror = root.join("environments").join("env-x");
        std::fs::create_dir_all(&mirror).expect("mirror dir");
        std::fs::write(mirror.join("docs.db"), "rows").expect("mirror file");
        (hosts, mirror)
    }

    let (hosts, mirror) = boot_and_seed(kept.path());
    hosts.remove("env-x", false).await.expect("remove");
    assert_eq!(hosts.list().len(), 1, "the entry is gone from the listing");
    assert!(
        mirror.join("docs.db").exists(),
        "without the purge the mirror stays readable — and re-pairable"
    );

    let (hosts, mirror) = boot_and_seed(purged.path());
    hosts.remove("env-x", true).await.expect("purge");
    assert!(
        !mirror.exists(),
        "an explicit purge deletes {mirror:?} with the entry"
    );
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
    );
    assert_eq!(hosts.list().len(), 2);
    assert!(hosts.runtime("env-x").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_stops_the_room_a_live_connection_still_holds() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let url = start_server(&dirs.path().join("server-b"), "org-b", "user-b", "VPS B").await;
    let core = assemble(&dirs.path().join("a"), "device-a");
    let env = core
        .envs
        .add(&link(&url, TOKEN, "VPS B"))
        .await
        .expect("add environment");

    let runtime = core.envs.runtime(&env.id).expect("live runtime");
    wait_until(
        || runtime.workspace.connected(),
        "environment registry join",
    )
    .await;

    // A served environment connection keeps its own clones of this runtime.
    let _client = memory_client(core.env_service(Some(&env.id)).expect("env service"));

    core.envs.remove(&env.id, false).await.expect("remove");
    wait_until(
        || !runtime.workspace.connected(),
        "environment room teardown",
    )
    .await;
}

#[tokio::test]
async fn an_unreadable_environments_file_boots_read_only() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let file = dirs.path().join("environments.json");
    let broken = r#"{"v":1,"environments":[{"id":"env-x"#;
    std::fs::write(&file, broken).expect("broken environments file");
    let hosts = EnvHosts::boot(
        dirs.path(),
        "device-a",
        "laptop".into(),
        HarnessId::Mock,
        Namespace {
            org_id: "org-a".into(),
            user_id: "user-a".into(),
        },
    );
    assert_eq!(hosts.list().len(), 1, "local only: the file is unreadable");
    assert!(
        hosts.remove("env-x", false).await.is_err(),
        "a booted read-only host refuses writes"
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("file survives"),
        broken,
        "the unreadable file is never overwritten, so its tokens stay put"
    );
}
