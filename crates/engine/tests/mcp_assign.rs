use std::collections::HashMap;
use std::sync::Arc;

use komet_engine::{EngineCore, HarnessRegistry};
use komet_engine::mcp::{McpRegistry, McpSecretStore, McpServerConfig, McpTransport};
use komet_proto::{ChatConfig, HarnessId, RunRequest, SandboxLevel};
use tempfile::TempDir;

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
    }
}

fn registry_with_mock() -> Arc<HarnessRegistry> {
    Arc::new(komet_engine::default_registry())
}

async fn test_engine_with_mcp() -> (EngineCore, TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    // Pre-seed MCP registry file with gh server
    let cfg = McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: true,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec!["mcp-github".into()],
        url: None,
        headers: HashMap::from([("Authorization".into(), "placeholder".into())]),
        env: HashMap::from([("GH_TOKEN".into(), "placeholder".into())]),
        always_load: false,
    };
    // Use McpRegistry::load to persist
    let mut reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&cfg).unwrap();

    // Secret store with actual secret containing word "secret"
    let secrets_path = dir.path().join("mcp-secrets.json");
    let mut store = McpSecretStore::new(&secrets_path);
    store
        .set_secret("gh", "Authorization", "Bearer super-secret-token-xyz")
        .unwrap();
    store
        .set_secret("gh", "GH_TOKEN", "ghp_secret_value_123")
        .unwrap();

    let core = EngineCore::assemble(dir.path(), registry_with_mock(), HarnessId::Mock, None)
        .expect("engine core assembles");

    // Create chat with mcp_server_ids = ["gh"]
    let chat_id = format!("chat-{}", uuid::Uuid::new_v4());
    let chat_config = ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        mcp_server_ids: vec!["gh".into()],
    };
    core.workspace
        .create_chat(&chat_id, None, Some(&core.device_id), Some(chat_config), Some("/tmp".into()))
        .unwrap();

    (core, dir, chat_id)
}

#[tokio::test]
async fn prepare_mcp_for_run_resolves_via_registry() {
    let (core, _dir, chat_id) = test_engine_with_mcp().await;
    let resolved = core.sessions.prepare_mcp_for_run(&chat_id);
    assert_eq!(resolved.len(), 1, "should resolve gh");
    assert_eq!(resolved[0].config.id, "gh");
    // Resolved headers/env contain secret values
    assert_eq!(
        resolved[0].resolved_headers.get("Authorization").unwrap(),
        "Bearer super-secret-token-xyz"
    );
    assert_eq!(
        resolved[0].resolved_env.get("GH_TOKEN").unwrap(),
        "ghp_secret_value_123"
    );
    // Debug must mask secrets
    let debug = format!("{:?}", resolved[0]);
    assert!(!debug.contains("super-secret-token-xyz"));
    assert!(!debug.contains("ghp_secret_value_123"));
    // Public view must not contain secret
    let public = resolved[0].public_view();
    let json = serde_json::to_string(&public).unwrap();
    assert!(!json.contains("super-secret-token-xyz"));
    core.shutdown().await;
}

#[tokio::test]
async fn run_does_not_store_secrets_in_journal() {
    let (core, dir, chat_id) = test_engine_with_mcp().await;

    // Verify chat config holds mcp_server_ids
    let chat = core.workspace.read_chats().unwrap().into_iter().find(|c| c.id == chat_id).unwrap();
    assert_eq!(chat.config.as_ref().unwrap().mcp_server_ids, vec!["gh".to_string()]);

    // Build run request (simulated dispatch without actually running harness) — ensure internal mcp field is None
    let req = run_request("hello");
    assert!(req.mcp.is_none() || !format!("{:?}", req).contains("secret"));

    // Prepare MCP for run via registry.resolve — should filter correctly
    let resolved = core.sessions.prepare_mcp_for_run(&chat_id);
    assert_eq!(resolved.len(), 1);
    // Convert to harness config (in-memory only, contains secrets, never persisted)
    let harness_cfgs: Vec<_> = resolved.into_iter().map(|r| r.into_harness_config()).collect();
    assert_eq!(harness_cfgs[0].id, "gh");
    // That harness config DOES contain secret in-memory (for launch), but we must not persist it
    // Verify that its Debug is masked (so logs don't leak)
    let harness_debug = format!("{:?}", harness_cfgs[0]);
    assert!(!harness_debug.contains("super-secret-token-xyz"));

    // Actually dispatch a run via mock harness to generate journal, then verify journal has no secret

    // Register mock harness for this test's registry (already default mock)
    // Dispatch through sessions engine directly
    let harness_req = run_request("do the thing");
    // Use the core's sessions.dispatch (which internally calls prepare_mcp_for_run)
    let run_id = core
        .sessions
        .dispatch(&chat_id, HarnessId::Mock, harness_req, Some("msg-1".into()))
        .await
        .unwrap();

    // Give the mock run a moment to journal events (MockHarness may need a script; we didn't configure mock behaviour)
    // Our mock harness with no script will error on run -> still journals Error/Done without secrets.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Verify journal does not contain secret (replay is the durable form; file is JSONL on disk)
    let replay = core.sessions.subscribe(&chat_id, 0).unwrap().0;
    let replay_debug = format!("{:?}", replay);
    assert!(!replay_debug.contains("super-secret-token-xyz"));
    assert!(!replay_debug.contains("ghp_secret_value_123"));

    // Also try to locate the journal file on disk and assert no secret (rg secret journals/ empty)
    fn find_journal(dir: &std::path::Path, chat_id: &str) -> Option<std::path::PathBuf> {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(p) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                for e in entries.filter_map(|e| e.ok()) {
                    let path = e.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|e| e == "jsonl")
                        && path.file_name().is_some_and(|n| n.to_string_lossy().contains(chat_id))
                    {
                        return Some(path);
                    }
                }
            }
        }
        None
    }
    if let Some(path) = find_journal(dir.path(), &chat_id) {
        let journal = std::fs::read_to_string(&path).unwrap();
        assert!(!journal.contains("super-secret-token-xyz"), "journal must not contain secret: {}", path.display());
        assert!(!journal.contains("ghp_secret_value_123"));
    }

    // Also verify transcript (SessionDoc) doesn't contain secret
    let handle = core.doc_host.open(&chat_id).unwrap();
    let entries = handle.doc().read_entries().unwrap();
    let transcript_json = serde_json::to_string(&entries).unwrap();
    assert!(!transcript_json.contains("super-secret-token-xyz"));
    assert!(!transcript_json.contains("ghp_secret_value_123"));

    // Verify RunRequest's debug repr doesn't leak
    let last_req = core.sessions.last_request(&chat_id).unwrap();
    let req_debug = format!("{:?}", last_req);
    assert!(!req_debug.contains("super-secret-token-xyz"));

    core.shutdown().await;
    let _ = run_id;
}

#[tokio::test]
async fn disabled_or_missing_mcp_filtered() {
    let dir = tempfile::tempdir().unwrap();
    // Create registry with disabled gh
    let mut reg = McpRegistry::load(dir.path()).unwrap();
    reg.save(&McpServerConfig {
        id: "gh".into(),
        name: "GitHub".into(),
        enabled: false,
        transport: McpTransport::Stdio,
        command: Some("npx".into()),
        args: vec![],
        url: None,
        headers: Default::default(),
        env: [("GH_TOKEN".into(), "placeholder".into())].into(),
        always_load: false,
    })
    .unwrap();
    let mut store = McpSecretStore::new(dir.path().join("mcp-secrets.json"));
    store.set_secret("gh", "GH_TOKEN", "secret123").unwrap();

    let core = EngineCore::assemble(dir.path(), registry_with_mock(), HarnessId::Mock, None).unwrap();
    let chat_id = format!("chat-{}", uuid::Uuid::new_v4());
    let cfg = ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        mcp_server_ids: vec!["gh".into(), "missing".into()],
    };
    core.workspace
        .create_chat(&chat_id, None, Some(&core.device_id), Some(cfg), Some("/tmp".into()))
        .unwrap();
    let resolved = core.sessions.prepare_mcp_for_run(&chat_id);
    assert!(resolved.is_empty(), "disabled and missing should be filtered");
    core.shutdown().await;
}
