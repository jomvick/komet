use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Transport for MCP servers — wire-safe, no secrets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

/// Full external MCP server config — secrets included ONLY for harness launch (RunRequest.mcp_external).
/// Never exposed via Public / logs; internal komet MCP stays in RunRequest.mcp.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub always_load: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("url", &self.url)
            .field("headers", &format_args!("{{{} keys masked}}", self.headers.len()))
            .field("env", &format_args!("{{{} keys masked}}", self.env.len()))
            .field("always_load", &self.always_load)
            .finish()
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            url: None,
            headers: HashMap::new(),
            env: HashMap::new(),
            always_load: false,
        }
    }
}

/// Resolved external server ready for provider launch: config + resolved secret values.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedMcpServer {
    pub config: McpServerConfig,
    #[serde(default)]
    pub resolved_headers: HashMap<String, String>,
    #[serde(default)]
    pub resolved_env: HashMap<String, String>,
}

impl std::fmt::Debug for ResolvedMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedMcpServer")
            .field("config", &self.config)
            .field(
                "resolved_headers",
                &format_args!("{{{} keys masked}}", self.resolved_headers.len()),
            )
            .field(
                "resolved_env",
                &format_args!("{{{} keys masked}}", self.resolved_env.len()),
            )
            .finish()
    }
}

/// Public view of an MCP server — safe to send over RPC / logs / UI.
/// Canonical `McpServerConfig` (with `headers`/`env` secret values) lives in
/// `komet-engine::mcp::config` and is the source of truth; the wire type
/// intentionally omits secret values and only exposes `has_secrets`.
///
/// Decision: engine is source of truth to avoid duplicating secret-bearing
/// fields on the wire. Proto re-exports only `PublicMcpServerConfig` +
/// `McpTransport` to eliminate leak surface (no `headers`/`env` on wire).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicMcpServerConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub always_load: bool,
    pub has_secrets: bool,
}
