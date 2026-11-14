//! Muse local usage reader.
//!
//! Verified by the probe (`docs/research/usage-sources.md`): Muse writes
//! `~/.local/share/muse/sessions/YYYY/MM/DD/<uuid>/session.jsonl` (plus
//! `subagent/*/session.jsonl`) — a JSONL event envelope whose `usage`
//! fields are process counters (CPU/RSS/FDS), and whose
//! `model_input_trace_recorded` records carry byte counts, not tokens.
//! `session-index.db` holds session metadata (model, prompt_count) but no
//! token counters either.
//!
//! On the probed install **no local token source exists**: this reader
//! therefore reports `Unavailable` with the precise reason instead of
//! inventing zeros. It still distinguishes "no session directory" /
//! "no session logs" / "logs carry no token counters" so the UI can say
//! exactly what was found. If a future Muse version starts writing token
//! counters, extend [`MuseReader::extract`] — do not guess field names.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::usage_sources::{LocalTurn, LocalUsageReader, ReadOutcome};

use super::{JsonlState, collect_files, outcome};

/// Muse session logs live under `<data dir>/muse/sessions/`.
pub struct MuseReader {
    root: PathBuf,
    /// Kept for symmetry with the other readers; Muse scans are cheap
    /// (few, small files) and always end unavailable today.
    #[allow(dead_code)]
    state: Mutex<JsonlState>,
}

impl MuseReader {
    /// `root` is the `sessions` directory (`~/.local/share/muse/sessions`).
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            root: sessions_root,
            state: Mutex::new(JsonlState::default()),
        }
    }
}

/// Placeholder for a future token-carrying record. Today every record is
/// inspected only enough to prove the file is a Muse session log; nothing
/// is extracted.
fn extract(_line: &str) -> Option<LocalTurn> {
    None
}

impl LocalUsageReader for MuseReader {
    fn agent(&self) -> &'static str {
        "muse"
    }

    fn source_label(&self) -> &'static str {
        "local logs"
    }

    fn read_since(&self, _since_ms: i64) -> ReadOutcome {
        let _ = extract; // see module doc — no token counters exist to extract
        let Some((files, _truncated)) = collect_files(
            &self.root,
            |p| p.file_name().is_some_and(|n| n == "session.jsonl"),
            20_000,
        ) else {
            return outcome(Vec::new(), false, true, 0, 0, None);
        };
        if files.is_empty() {
            return outcome(Vec::new(), false, false, 0, 0, None);
        }
        // Files exist but the probed format has no token counters.
        outcome(
            Vec::new(),
            true,
            false,
            0,
            0,
            Some("muse session logs carry no token counters"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directory_is_unavailable() {
        let reader = MuseReader::new(PathBuf::from("/nonexistent/muse/sessions"));
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(out.reason.as_deref(), Some("no session directory"));
    }

    #[test]
    fn empty_directory_is_unavailable() {
        let dir = std::env::temp_dir().join(format!(
            "komet-muse-reader-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reader = MuseReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.reason.as_deref(), Some("no local session logs found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_logs_without_tokens_report_the_reason() {
        let dir = std::env::temp_dir().join(format!(
            "komet-muse-reader-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        let session = dir.join("2026/08/20/uuid-1");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("session.jsonl"),
            "{\"payload\":{\"event\":{\"kind\":\"assistant_message_committed\"}}}\n",
        )
        .unwrap();
        let reader = MuseReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(
            out.reason.as_deref(),
            Some("muse session logs carry no token counters")
        );
        assert!(out.turns.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
