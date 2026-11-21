use std::path::PathBuf;

use komet_proto::HarnessId;
use serde::{Deserialize, Serialize};

/// What kind of agent-native extension an item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtKind {
    Skill,
    McpServer,
    Plugin,
}

impl ExtKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtKind::Skill => "skill",
            ExtKind::McpServer => "mcp_server",
            ExtKind::Plugin => "plugin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "skill" => Some(ExtKind::Skill),
            "mcp_server" => Some(ExtKind::McpServer),
            "plugin" => Some(ExtKind::Plugin),
            _ => None,
        }
    }
}

/// Where an item comes from. `External` = found in an agent's own config
/// (never written by komet). `Komet` = installed/managed by komet itself.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    External,
    Komet,
}

impl Origin {
    /// Stable key segment for the persisted overlay (`external`/`komet`).
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::External => "external",
            Origin::Komet => "komet",
        }
    }
}

/// One place an item was discovered for one agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtSource {
    pub agent: HarnessId,
    /// Config file or skill directory the item was read from.
    pub path: PathBuf,
    /// The agent-native enabled flag when the source config declares one
    /// (e.g. `"enabled": false` in an opencode.json MCP entry). `None` means
    /// the agent has no such knob — presence implies enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// A discovered skill / MCP server / plugin, deduplicated across agents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtItem {
    pub kind: ExtKind,
    /// Stable identity inside its kind: skill dir name, MCP server name,
    /// `name@marketplace` for plugins. Unique per (kind, id) — NOT per agent.
    pub id: String,
    /// Human-facing label (frontmatter description first line, config `name`,
    /// or the id itself).
    pub name: String,
    pub origin: Origin,
    /// Optional safe-to-display detail (e.g. `"http"` or `"stdio · npx"` for a
    /// komet MCP). NEVER carries secrets, header/env values, tokens in URLs,
    /// or command arguments that may embed credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Every (agent, path) location this item was found at. An item with no
    /// source for a given agent is simply not available to that agent.
    #[serde(default)]
    pub sources: Vec<ExtSource>,
}

impl ExtItem {
    /// The enabled hint the agent's own config declares for `agent`, if the
    /// item is available to that agent at all.
    pub fn hint_for(&self, agent: HarnessId) -> Option<bool> {
        self.sources
            .iter()
            .find(|s| s.agent == agent)
            .and_then(|s| s.enabled)
    }

    pub fn available_to(&self, agent: HarnessId) -> bool {
        self.sources.iter().any(|s| s.agent == agent)
    }
}
