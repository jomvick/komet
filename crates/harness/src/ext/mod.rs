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
pub(crate) use scan::codex_mcp_server_names;
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

/// Fallible load for MUTATION paths: a corrupt overlay must surface as an
/// error, never be silently replaced — persisting an empty replacement would
/// drop every stored decision.
pub fn load_overlay_strict() -> anyhow::Result<ExtOverlay> {
    let path = default_overlay_path();
    if cfg!(test) {
        return Ok(ExtOverlay::empty(path));
    }
    ExtOverlay::load(&path)
}

/// Serializes load → mutate → persist across the whole process: the engine
/// can serve several RPC connections, each holding its own snapshot, and
/// concurrent writers would otherwise lose each other's decisions (and race
/// on the shared `*.tmp` rename target).
static OVERLAY_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Locked read-modify-write for overlay mutations. `f` receives the freshly
/// loaded overlay and applies its change; nothing is persisted unless `f`
/// succeeds.
pub fn update_overlay(f: impl FnOnce(&mut ExtOverlay) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let _guard = OVERLAY_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut overlay = load_overlay_strict()?;
    f(&mut overlay)
}
