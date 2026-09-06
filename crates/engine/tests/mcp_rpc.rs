//! Task 7 — RPC surface for external MCP servers.
//!
//! Tests run against an assembled engine and its `rpc_service()`. The
//! security invariant under test: secret values are write-only (they flow in
//! via `SaveMcpServer`, never back out); replies carry the secret-free
//! `PublicMcpServerConfig` view only.

use std::sync::Arc;

use futures::StreamExt as _;
use komet_engine::{EngineCore, EngineRpc};
use komet_proto::HarnessId;
use komet_rpc::{RpcError, RpcReply, RpcService, methods};
use tempfile::TempDir;

const SECRET: &str = "super-secret-token-abc123";

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

fn save_params(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": "GitHub",
        "transport": "stdio",
        "command": "npx",
        "args": ["mcp-github"],
        "env": { "GH_TOKEN": SECRET },
        "headers": {},
    })
}

#[tokio::test]
async fn save_list_roundtrip_hides_secret_values() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();

    let saved = call(&rpc, methods::SAVE_MCP_SERVER, save_params("gh"))
        .await
        .expect("save ok");
    // Reply is the public view: hasSecrets true, but no secret value anywhere.
    assert_eq!(saved["id"], "gh");
    assert_eq!(saved["hasSecrets"], true);
    let saved_str = saved.to_string();
    assert!(
        !saved_str.contains(SECRET),
        "save reply leaks secret: {saved_str}"
    );
    assert!(
        !saved_str.contains("\"env\""),
        "save reply must not expose the env map: {saved_str}"
    );

    let list = call(&rpc, methods::LIST_MCP_SERVERS, serde_json::json!({}))
        .await
        .expect("list ok");
    assert_eq!(list["servers"].as_array().unwrap().len(), 1);
    assert_eq!(list["servers"][0]["id"], "gh");
    let list_str = list.to_string();
    assert!(!list_str.contains(SECRET));
    assert!(
        !list_str.contains("\"env\""),
        "list reply must not expose the env map: {list_str}"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn get_unknown_server_fails() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let err = call(
        &rpc,
        methods::GET_MCP_SERVER,
        serde_json::json!({ "id": "nope" }),
    )
    .await
    .expect_err("unknown id must fail");
    assert!(err.to_string().contains("not found"));
    core.shutdown().await;
}

#[tokio::test]
async fn save_preserves_secret_on_masked_roundtrip() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    call(&rpc, methods::SAVE_MCP_SERVER, save_params("gh"))
        .await
        .unwrap();

    // UI-style re-save: the secret field comes back as an empty string (the
    // form only shows masked placeholders); the stored value must survive.
    let resaved = call(
        &rpc,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({
            "id": "gh",
            "name": "GitHub Renamed",
            "transport": "stdio",
            "command": "npx",
            "args": ["mcp-github"],
            "env": { "GH_TOKEN": "" },
            "headers": {},
        }),
    )
    .await
    .expect("resave ok");
    assert_eq!(resaved["name"], "GitHub Renamed");
    assert_eq!(
        resaved["hasSecrets"], true,
        "empty secret value must keep the stored one"
    );

    let resolved = core.mcp_secrets.resolve("gh");
    assert_eq!(
        resolved.get("GH_TOKEN").map(String::as_str),
        Some(SECRET),
        "masked re-save must not wipe the stored secret"
    );

    core.shutdown().await;
}

#[tokio::test]
async fn delete_purges_secrets_and_registry_entry() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    call(&rpc, methods::SAVE_MCP_SERVER, save_params("gh"))
        .await
        .unwrap();
    call(
        &rpc,
        methods::DELETE_MCP_SERVER,
        serde_json::json!({ "id": "gh" }),
    )
    .await
    .expect("delete ok");
    assert!(
        core.mcp_secrets.resolve("gh").is_empty(),
        "secrets purged on delete"
    );
    let list = call(&rpc, methods::LIST_MCP_SERVERS, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(list["servers"].as_array().unwrap().len(), 0);

    core.shutdown().await;
}

#[tokio::test]
async fn set_enabled_roundtrip() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    call(&rpc, methods::SAVE_MCP_SERVER, save_params("gh"))
        .await
        .unwrap();
    let off = call(
        &rpc,
        methods::SET_MCP_SERVER_ENABLED,
        serde_json::json!({ "id": "gh", "enabled": false }),
    )
    .await
    .unwrap();
    assert_eq!(off["enabled"], false);
    let on = call(
        &rpc,
        methods::SET_MCP_SERVER_ENABLED,
        serde_json::json!({ "id": "gh", "enabled": true }),
    )
    .await
    .unwrap();
    assert_eq!(on["enabled"], true);

    core.shutdown().await;
}

#[tokio::test]
async fn set_enabled_missing_server_fails() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let err = call(
        &rpc,
        methods::SET_MCP_SERVER_ENABLED,
        serde_json::json!({ "id": "ghost", "enabled": true }),
    )
    .await
    .expect_err("missing server must fail");
    assert!(err.to_string().contains("not found"));
    core.shutdown().await;
}

#[tokio::test]
async fn save_rejects_invalid_config() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    // stdio without command
    let err = call(
        &rpc,
        methods::SAVE_MCP_SERVER,
        serde_json::json!({ "id": "bad", "name": "Bad", "transport": "stdio" }),
    )
    .await
    .expect_err("stdio without command must fail");
    assert!(matches!(err, RpcError::BadParams(_)));
    // registry must stay untouched
    let list = call(&rpc, methods::LIST_MCP_SERVERS, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(list["servers"].as_array().unwrap().len(), 0);

    core.shutdown().await;
}

#[tokio::test]
async fn test_mcp_server_probes_unsaved_config_without_persisting() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    // Probe unsaved form values via the {config: ...} override; a dead stdio
    // command yields an error status in the reply, not an RPC error.
    let probe = call(
        &rpc,
        methods::TEST_MCP_SERVER,
        serde_json::json!({
            "config": {
                "id": "dead",
                "name": "Dead",
                "enabled": true,
                "transport": "stdio",
                "command": "__dead__",
                "args": [],
                "headers": {},
                "env": {},
            }
        }),
    )
    .await
    .expect("test reply carries status, not an RPC error");
    assert_eq!(probe["id"], "dead");
    assert_eq!(probe["status"], "error");
    assert!(probe["error"].as_str().unwrap_or_default().len() > 0);
    assert_eq!(probe["toolsCount"], 0);
    // Nothing persisted by the probe.
    let list = call(&rpc, methods::LIST_MCP_SERVERS, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(list["servers"].as_array().unwrap().len(), 0);
    assert!(core.mcp_secrets.resolve("dead").is_empty());

    core.shutdown().await;
}

#[tokio::test]
async fn list_mcp_tools_requires_existing_server() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let err = call(
        &rpc,
        methods::LIST_MCP_TOOLS,
        serde_json::json!({ "id": "missing" }),
    )
    .await
    .expect_err("unknown server must fail");
    assert!(err.to_string().contains("not found"));
    core.shutdown().await;
}

#[tokio::test]
async fn watch_mcp_status_streams_error_then_terminates() {
    let (core, _dir) = test_engine();
    let rpc = core.rpc_service();
    let reply = rpc
        .handle(
            methods::WATCH_MCP_STATUS,
            serde_json::json!({ "id": "ghost" }),
        )
        .await
        .expect("stream reply");
    let RpcReply::Stream(mut stream) = reply else {
        panic!("WATCH_MCP_STATUS must stream");
    };
    // The probe for a missing server errors immediately; the bridged watch
    // stream must carry the error state and then terminate (tx dropped).
    let mut saw_error = false;
    let mut count = 0;
    while let Some(item) = stream.next().await {
        count += 1;
        if item == serde_json::json!("error") {
            saw_error = true;
        }
        assert!(
            count < 10,
            "status stream should end shortly after the error"
        );
    }
    assert!(saw_error, "status stream must carry the error state");
    assert!(count >= 1);

    core.shutdown().await;
}
