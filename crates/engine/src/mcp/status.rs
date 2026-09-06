use serde::Serialize;

/// Shared MCP status – single source of truth for discovery and server.
/// Previously duplicated in `discovery.rs` and `server.rs`; now canonical here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStatus {
    Starting,
    Ready,
    Error(String),
    Stopped,
}

impl Serialize for McpStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Error(_) => "error",
            Self::Stopped => "stopped",
        })
    }
}

impl std::fmt::Display for McpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Ready => write!(f, "ready"),
            Self::Error(e) => write!(f, "error: {e}"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}
