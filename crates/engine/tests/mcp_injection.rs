//! Dispatch injects the per-run internal Komet MCP. External servers stay
//! with the agent — `mcp_external` is left empty.

use std::collections::HashMap;
use std::sync::Arc;

use komet_engine::EngineCore;
use komet_engine::mcp::{McpRegistry, McpSecretStore, McpServerConfig, McpTransport};
use komet_proto::{ChatConfig, HarnessId, RunRequest, SandboxLevel};
use tempfile::TempDir;

const SECRET: &str = "Bearer super-secret-token-xyz";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        sandbox_options: None,
        auto_approve: true,
        attachments: Vec::new(),
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
        mcp: None,
        mcp_external: Vec::new(),
    }
}

async fn engine_with_github() -> (EngineCore, TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec!["mcp-github".into()],
        url: None,
        headers: HashMap::from([("Authorization".into(), "placeholder".into())]),
        env: HashMap::new(),
        always_load: false,
    })
    .unwrap();
    McpSecretStore::new(dir.path().join("mcp-secrets.json"))
        .set_secret("gh", "Authorization", SECRET)
        .unwrap();

    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(komet_engine::default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let chat_id = format!("chat-{}", uuid::Uuid::new_v4());
    core.workspace
        .create_chat(
            &chat_id,
            None,
            Some(&core.device_id),
            Some(ChatConfig {
                harness: HarnessId::Mock,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                mcp_server_ids: vec!["gh".into()],
            }),
            Some("/tmp".into()),
        )
        .unwrap();
    (core, dir, chat_id)
}

#[tokio::test]
async fn dispatch_does_not_inject_external_mcp_or_refuse_the_run() {
    let (core, _dir, chat_id) = engine_with_github().await;
    core.sessions
        .dispatch(
            &chat_id,
            HarnessId::Mock,
            run_request("list issues"),
            Some("msg-1".into()),
        )
        .await
        .unwrap();
    let req = core.sessions.last_request(&chat_id).expect("dispatched");
    assert!(
        req.mcp_external.is_empty(),
        "external MCP is the agent's, not Komet's"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn dispatch_injects_internal_komet_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        dir.path(),
        Arc::new(komet_engine::default_registry()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let chat_id = format!("chat-{}", uuid::Uuid::new_v4());
    core.workspace
        .create_chat(
            &chat_id,
            None,
            Some(&core.device_id),
            Some(ChatConfig {
                harness: HarnessId::Mock,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                mcp_server_ids: Vec::new(),
            }),
            Some("/tmp".into()),
        )
        .unwrap();
    core.sessions
        .dispatch(
            &chat_id,
            HarnessId::Mock,
            run_request("hello"),
            Some("msg-internal".into()),
        )
        .await
        .unwrap();
    let req = core.sessions.last_request(&chat_id).expect("dispatched");
    let mcp = req.mcp.as_ref().expect("internal komet MCP");
    assert_eq!(mcp.server_name, "komet");
    assert!(mcp.url.starts_with("http://127.0.0.1:"), "url={}", mcp.url);
    assert!(
        mcp.url.contains("/mcp/agents?callerAgentId="),
        "url={}",
        mcp.url
    );
    assert!(!mcp.auth_token.is_empty());
    let debug = format!("{req:?}");
    assert!(!debug.contains(&mcp.auth_token));
    core.shutdown().await;
}
