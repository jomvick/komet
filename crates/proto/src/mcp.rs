use serde::{Deserialize, Serialize};

/// Transport for MCP servers — wire-safe, no secrets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
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
