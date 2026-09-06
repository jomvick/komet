pub mod catalog;
pub mod config;
pub mod discovery;
pub mod policy;
pub mod registry;
pub mod secrets;
pub mod server;
pub mod status;

pub use config::{McpServerConfig, McpTransport, PublicMcpServerConfig};
pub use discovery::DiscoveredTool;
pub use registry::{McpRegistry, ResolvedMcpServer};
pub use secrets::McpSecretStore;
pub use status::McpStatus;

/// Serde default for `SaveMcpServerParams.enabled` (missing field = enabled).
pub fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests_config;
#[cfg(test)]
mod tests_policy;
#[cfg(test)]
mod tests_registry;
