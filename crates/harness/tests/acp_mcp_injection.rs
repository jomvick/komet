use komet_harness::acp::build_acp_session_params;
use komet_proto::{McpInjection, McpServerConfig, McpTransport, ResolvedMcpServer};
use std::collections::HashMap;

#[test]
fn acp_session_params_merges_internal_and_external() {
    let internal = McpInjection {
        server_name: "komet".into(),
        url: "http://127.0.0.1/mcp".into(),
        auth_token: "t".into(),
    };
    let external = ResolvedMcpServer {
        config: McpServerConfig {
            id: "gh".into(),
            name: "GitHub".into(),
            enabled: true,
            transport: McpTransport::Http,
            url: Some("https://mcp.github.com".into()),
            ..Default::default()
        },
        resolved_headers: [("Authorization".into(), "Bearer x".into())].into(),
        resolved_env: Default::default(),
    };
    let params = build_acp_session_params("/ws", Some(internal), vec![external]);
    assert_eq!(params["mcpServers"].as_array().unwrap().len(), 2);
    assert_eq!(params["env"], serde_json::json!([]));
    // internal komet server should be first, type http with Bearer
    assert_eq!(params["mcpServers"][0]["type"], "http");
    assert_eq!(params["mcpServers"][0]["name"], "komet");
    assert_eq!(params["mcpServers"][0]["url"], "http://127.0.0.1/mcp");
    assert_eq!(
        params["mcpServers"][0]["headers"],
        serde_json::json!([{ "name": "Authorization", "value": "Bearer t" }])
    );
    // external http
    assert_eq!(params["mcpServers"][1]["type"], "http");
    assert_eq!(params["mcpServers"][1]["name"], "gh");
    assert_eq!(params["mcpServers"][1]["url"], "https://mcp.github.com");
}

#[test]
fn acp_session_params_handles_all_transports() {
    let internal = None;
    let http = ResolvedMcpServer {
        config: McpServerConfig {
            id: "gh".into(),
            name: "GitHub".into(),
            enabled: true,
            transport: McpTransport::Http,
            url: Some("https://mcp.github.com".into()),
            ..Default::default()
        },
        resolved_headers: [("Authorization".into(), "Bearer x".into())].into(),
        resolved_env: Default::default(),
    };
    let sse = ResolvedMcpServer {
        config: McpServerConfig {
            id: "notion".into(),
            name: "Notion".into(),
            enabled: true,
            transport: McpTransport::Sse,
            url: Some("https://mcp.notion.com/sse".into()),
            ..Default::default()
        },
        resolved_headers: HashMap::new(),
        resolved_env: Default::default(),
    };
    let stdio = ResolvedMcpServer {
        config: McpServerConfig {
            id: "fs".into(),
            name: "FS".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "mcp-fs".into()],
            ..Default::default()
        },
        resolved_headers: HashMap::new(),
        resolved_env: [("TOKEN".into(), "secret".into())].into(),
    };
    let params = build_acp_session_params("/ws", internal, vec![http, sse, stdio]);
    assert_eq!(params["mcpServers"].as_array().unwrap().len(), 3);
    assert_eq!(params["mcpServers"][0]["type"], "http");
    assert_eq!(params["mcpServers"][1]["type"], "sse");
    assert_eq!(params["mcpServers"][2]["name"], "fs");
    assert_eq!(params["mcpServers"][2]["command"], "npx");
    assert_eq!(params["env"], serde_json::json!([]));
    assert_eq!(params["cwd"], "/ws");
}

#[test]
fn acp_session_params_empty_is_env_only() {
    let params = build_acp_session_params("/tmp", None, vec![]);
    assert_eq!(params["mcpServers"].as_array().unwrap().len(), 0);
    assert_eq!(params["env"], serde_json::json!([]));
}
