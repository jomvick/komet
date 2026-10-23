use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const COMPOSIO_SERVER_ID: &str = "composio";
pub const COMPOSIO_SERVER_URL: &str = "https://connect.composio.dev/mcp";
pub const COMPOSIO_API_KEY_HEADER: &str = "x-consumer-api-key";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
}

/// Canonical external MCP server configuration.
/// Secrets (header values, env values) are stored in this struct in-memory
/// after resolve; only the launch-only resolved type carries them to a run.
/// They must never be written to docs or sync.
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
            .field(
                "headers",
                &format_args!("{{{} keys masked}}", self.headers.len()),
            )
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
#[serde(rename_all = "camelCase")]
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
        if self.id == COMPOSIO_SERVER_ID
            && (self.transport != McpTransport::Http
                || self.url.as_deref() != Some(COMPOSIO_SERVER_URL)
                || !self.headers.contains_key(COMPOSIO_API_KEY_HEADER)
                || self
                    .headers
                    .keys()
                    .any(|key| key != COMPOSIO_API_KEY_HEADER)
                || !self.env.is_empty()
                || self.command.is_some()
                || !self.args.is_empty())
        {
            anyhow::bail!("Composio must use its fixed HTTP endpoint and API-key header");
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

#[cfg(test)]
mod composio_tests {
    use super::*;

    #[test]
    fn composio_must_use_fixed_http_endpoint() {
        let mut config = McpServerConfig {
            id: COMPOSIO_SERVER_ID.into(),
            name: "Composio".into(),
            enabled: true,
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            url: Some(COMPOSIO_SERVER_URL.into()),
            headers: [(COMPOSIO_API_KEY_HEADER.into(), String::new())].into(),
            env: Default::default(),
            always_load: true,
        };
        assert!(config.validate().is_ok());

        config.url = Some("https://attacker.invalid/mcp".into());
        assert!(config.validate().is_err());
    }
}
