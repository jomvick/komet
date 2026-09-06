use std::path::PathBuf;

use komet_proto::{HarnessId, McpInjection, McpServerConfig, McpTransport, ResolvedMcpServer, RunRequest};
use serde_json::Value;

use crate::capabilities::supports_dynamic_mcp;
use crate::claude::ClaudeHarness;

fn resolved_github() -> ResolvedMcpServer {
    ResolvedMcpServer {
        config: McpServerConfig {
            id: "github".into(),
            name: "GitHub".into(),
            enabled: true,
            transport: McpTransport::Http,
            url: Some("https://mcp.github.com/mcp".into()),
            headers: Default::default(),
            env: Default::default(),
            command: None,
            args: vec![],
            always_load: false,
        },
        resolved_headers: [("Authorization".into(), "Bearer gh-secret".into())].into(),
        resolved_env: Default::default(),
    }
}

fn resolved_notion() -> ResolvedMcpServer {
    ResolvedMcpServer {
        config: McpServerConfig {
            id: "notion".into(),
            name: "Notion".into(),
            enabled: true,
            transport: McpTransport::Sse,
            url: Some("https://mcp.notion.com/sse".into()),
            headers: Default::default(),
            env: Default::default(),
            command: None,
            args: vec![],
            always_load: false,
        },
        resolved_headers: Default::default(),
        resolved_env: Default::default(),
    }
}

fn base_request_with_mcp(externals: Vec<ResolvedMcpServer>) -> RunRequest {
    RunRequest {
        prompt: "hi".into(),
        harness: Some(HarnessId::ClaudeCode),
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: komet_proto::SandboxLevel::DangerFullAccess,
        sandbox_options: None,
        auto_approve: true,
        attachments: vec![],
        permission_timeout_ms: None,
        worktree: None,
        resume: None,
        mcp: Some(McpInjection {
            server_name: "komet".into(),
            url: "http://127.0.0.1:9/mcp".into(),
            auth_token: "tok".into(),
        }),
        mcp_external: externals,
    }
}

fn extract_mcp_config(cmd: &tokio::process::Command) -> Value {
    let std_cmd = cmd.as_std();
    let args: Vec<String> = std_cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let idx = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("--mcp-config present");
    let json_str = &args[idx + 1];
    serde_json::from_str(json_str).expect("valid mcp json")
}

#[test]
fn claude_mcp_config_merges_without_duplicate() {
    let req = base_request_with_mcp(vec![resolved_github(), resolved_notion()]);
    let h = ClaudeHarness::new();
    let cmd = h.build_command(&PathBuf::from("claude"), &req);
    let cfg = extract_mcp_config(&cmd);
    assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 3); // komet + gh + notion
}

#[test]
fn capability_matrix_reports_codex_status() {
    assert_eq!(supports_dynamic_mcp(HarnessId::ClaudeCode), (true, ""));
    assert_eq!(
        supports_dynamic_mcp(HarnessId::Codex),
        (false, "Ce provider ne supporte pas l'injection MCP dynamique")
    );
}

#[test]
fn claude_mcp_dedup_by_id_and_url() {
    // Duplicate github twice — should dedup to 1 + komet + notion = 3
    let req = base_request_with_mcp(vec![
        resolved_github(),
        resolved_github(),
        resolved_notion(),
    ]);
    let h = ClaudeHarness::new();
    let cmd = h.build_command(&PathBuf::from("claude"), &req);
    let cfg = extract_mcp_config(&cmd);
    assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 3);
}

#[test]
fn claude_mcp_dedup_by_url_suppresses_duplicate_url() {
    let mut gh2 = resolved_github();
    gh2.config.id = "github2".into();
    // same url as github, different id — url dedup should suppress
    let req = base_request_with_mcp(vec![resolved_github(), gh2]);
    let h = ClaudeHarness::new();
    let cmd = h.build_command(&PathBuf::from("claude"), &req);
    let cfg = extract_mcp_config(&cmd);
    // komet + only one of the github ids
    assert_eq!(cfg["mcpServers"].as_object().unwrap().len(), 2);
}
