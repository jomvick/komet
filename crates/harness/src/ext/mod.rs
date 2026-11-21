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
