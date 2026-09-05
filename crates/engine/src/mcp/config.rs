use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

/// Canonical external MCP server configuration.
/// Secrets (header values, env values) are stored in this struct in-memory
/// after resolve, but must never be serialized into RunRequest/docs/sync.
/// Presentation layer should use `PublicMcpServerConfig`.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
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
            args: vec![],
            url: None,
            headers: HashMap::new(),
            env: HashMap::new(),
            always_load: false,
        }
    }
}

/// Public view without secret values — safe to send over RPC / logs / UI.
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

impl McpServerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            anyhow::bail!("id/name required");
        }
        match self.transport {
            McpTransport::Stdio => {
                let cmd = self.command.as_deref().unwrap_or("").trim();
                if cmd.is_empty() {
                    anyhow::bail!("stdio requires command");
                }
            }
            McpTransport::Http | McpTransport::Sse => {
                let url = self.url.as_deref().unwrap_or("").trim();
                if url.is_empty() {
                    anyhow::bail!("http/sse requires url");
                }
            }
        }
        Ok(())
    }

    pub fn public_view(&self) -> PublicMcpServerConfig {
        PublicMcpServerConfig {
            id: self.id.clone(),
            name: self.name.clone(),
            enabled: self.enabled,
            transport: self.transport.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            url: self.url.clone(),
            always_load: self.always_load,
            has_secrets: !self.headers.is_empty() || !self.env.is_empty(),
        }
    }
}
