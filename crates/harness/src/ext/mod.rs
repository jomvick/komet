//! Read-only inventory of the skills, MCP servers and plugins that live in
//! each agent's own configuration, plus the komet-side overlay that decides —
//! without ever touching the agent's files — which of them a komet-launched
//! session may see.
//!
//! Two rules carry the whole design:
//! - scanners only read; the overlay is the single place komet persists a
//!   per-(kind, agent, item) enabled flag;
//! - an item found in several agents' configs is ONE `ExtItem` with one
//!   `ExtSource` per (agent, path) pair, so the UI shows a single row whose
//!   toggle is still per-agent.

mod model;
mod overlay;
mod scan;

pub use model::{ExtItem, ExtKind, ExtSource, Origin};
pub use overlay::ExtOverlay;
pub use scan::scan;

/// Well-known overlay location, mirroring `McpSecretStore::default_path`:
/// `$KOMET_DATA_DIR/agent-ext-overlay.json`, else `~/.komet/…`, else `/tmp`.
/// Engine and harness share the process (or the engine is spawned with
/// `KOMET_DATA_DIR` set), so both resolve the same file.
pub fn default_overlay_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("KOMET_EXT_OVERLAY_PATH")
        && !path.is_empty()
    {
        return std::path::PathBuf::from(path);
    }
    if let Ok(dir) = std::env::var("KOMET_DATA_DIR")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir).join("agent-ext-overlay.json");
    }
    if let Some(home) = crate::platform::home_dir() {
        return home.join(".komet").join("agent-ext-overlay.json");
    }
    std::path::PathBuf::from("/tmp/komet-agent-ext-overlay.json")
}

/// Load the persisted overlay; a missing/corrupt file yields an empty overlay
/// (launch must never fail because a preference file is unreadable).
/// Unit tests always get an empty overlay so argv assertions never depend on
/// the developer's real `~/.komet` state; `KOMET_EXT_OVERLAY_PATH` still wins
/// for tests that want a fixture (integration tests link the non-test lib).
pub fn load_overlay() -> ExtOverlay {
    let path = default_overlay_path();
    if cfg!(test) {
        return ExtOverlay::empty(path);
    }
    ExtOverlay::load(&path).unwrap_or_else(|_| ExtOverlay::empty(path))
}
