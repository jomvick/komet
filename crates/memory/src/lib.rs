//! komet-memory — local-first, per-device project memory (issue #78, Phase 1).
//!
//! Stores an allow-listed projection of the run journal (event metadata only —
//! never prompts, message text, tool arguments, command strings, diffs, or
//! file contents) in a per-device SQLite database under
//! `<data_dir>/memory/`, outside session/workspace sync. Provides FTS search,
//! per-project enable/exclusions, forget operations, and a deterministic
//! handoff briefing injected at the harness boundary.
//!
//! The engine feeds it through a bounded queue + single writer thread; the
//! run journal stays the authoritative source and catch-up replays gaps
//! idempotently (`UNIQUE(project_id, chat_id, source_seq)`).

mod briefing;
mod policy;
mod record;
mod store;

pub use briefing::build as build_briefing;
pub use policy::{
    contains_secret_value, is_default_excluded, project_id_for, project_relative, user_excluded,
};
pub use record::{ChatCursor, MemoryEvent, MemoryItem, MemoryRecord};
pub use store::{MemoryError, MemoryService, MemoryStats, QUEUE_CAPACITY};

/// Lowercase hex of raw bytes (same shape as `repos::hex` in the engine).
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}
