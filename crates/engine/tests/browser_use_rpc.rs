//! Browser-use preset RPC (issue #45): status probe, enable gated on Chrome +
//! npx, disable, and the registry entry shape every compatible harness gets.

use std::sync::Arc;

use komet_engine::mcp::browser_use;
use komet_engine::{EngineCore, EngineRpc};
use komet_proto::HarnessId;
use komet_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

fn test_engine() -> (EngineCore, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(komet_engine::default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    (core, dir)
}

async fn call(
    rpc: &EngineRpc,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    match rpc.handle(method, params).await? {
        RpcReply::Value(v) => Ok(v),
        RpcReply::Stream(_) => panic!("{method}: unexpected stream reply"),
    }
}

#[tokio::test]
async fn status_reports_probe_and_registration() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let v = call(&rpc, methods::BROWSER_USE_STATUS, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(v["serverId"], browser_use::SERVER_ID);
    assert_eq!(v["status"]["configured"], false);
    assert_eq!(v["status"]["enabled"], false);
    assert_eq!(v["status"]["package"], browser_use::package_spec());
    assert_eq!(
        v["status"]["chromeInstalled"].as_bool().unwrap(),
        browser_use::probe_chrome().is_some()
    );
    assert!(
        v["systemPrompt"]
            .as_str()
            .unwrap()
            .contains("take_snapshot")
    );
}

#[tokio::test]
async fn enable_is_gated_unless_forced_then_registers_pinned_server() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let blocked = browser_use::blocker();
    let plain = call(&rpc, methods::ENABLE_BROWSER_USE, serde_json::json!({})).await;
    match (&blocked, plain) {
        (Some(reason), Err(RpcError::Failed(msg))) => assert_eq!(&msg, reason),
        (None, Ok(v)) => assert_eq!(v["status"]["enabled"], true),
        (blocked, other) => panic!("blocker={blocked:?} reply={other:?}"),
    }

    let v = call(
        &rpc,
        methods::ENABLE_BROWSER_USE,
        serde_json::json!({ "force": true, "alwaysLoad": true }),
    )
    .await
    .expect("forced enable");
    assert_eq!(v["status"]["configured"], true);
    assert_eq!(v["status"]["enabled"], true);
    assert_eq!(v["status"]["alwaysLoad"], true);
    assert_eq!(v["server"]["id"], browser_use::SERVER_ID);
    assert_eq!(v["server"]["command"], "npx");
    assert_eq!(v["server"]["hasSecrets"], false);
    let args: Vec<String> = serde_json::from_value(v["server"]["args"].clone()).unwrap();
    assert_eq!(
        args,
        vec![
            "-y".to_string(),
            browser_use::package_spec(),
            "--headless".into(),
            "--isolated".into()
        ]
    );

    // Re-enable without `alwaysLoad` keeps the previous assignment.
    let v = call(
        &rpc,
        methods::ENABLE_BROWSER_USE,
        serde_json::json!({ "force": true }),
    )
    .await
    .unwrap();
    assert_eq!(v["status"]["alwaysLoad"], true);

    // Listed like any other server, and resolved for runs via always_load.
    let list = call(&rpc, methods::LIST_MCP_SERVERS, serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        list["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == browser_use::SERVER_ID)
    );
    let resolved = core
        .sessions
        .harness_mcp_servers_for_run("chat-without-assignment");
    assert!(resolved.iter().any(browser_use::is_browser_use));

    let v = call(&rpc, methods::DISABLE_BROWSER_USE, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(v["status"]["configured"], false);
    assert!(
        core.sessions
            .harness_mcp_servers_for_run("chat-without-assignment")
            .is_empty()
    );
}
