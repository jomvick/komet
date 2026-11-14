//! Local, read-only usage readers for agents that keep their own logs
//! (issue #53). Each reader implements [`LocalUsageReader`]: it scans the
//! files the agent CLI leaves on disk and returns [`LocalTurn`]s — counters,
//! timestamps and model names only, never message text.
//!
//! The formats implemented here were verified by `scripts/probe-usage-sources.py`
//! on real machines; see `docs/research/usage-sources.md` for the per-agent
//! record shapes and token semantics. An agent whose logs carry no token
//! counters (Muse, on the probed install) reports `Unavailable` with the
//! reason — never an invented zero.
//!
//! Incremental reads: JSONL readers keep a per-file `(mtime, size, offset)`
//! cursor and only parse bytes appended since the last call; the OpenCode
//! reader keeps the last seen `rowid`. Memory is bounded by dropping turns
//! older than [`RETENTION_MS`] (the same 180-day horizon as the usage store).

mod grok;
mod muse;
mod opencode;
mod pi;

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::usage_sources::{LocalTurn, LocalUsageReader, ReadOutcome};

/// Same rolling horizon as `usage::RETENTION_DAYS` — readers never keep
/// turns older than this in memory.
pub(crate) const RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;

/// Per-file read cursor for append-mostly JSONL logs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FileCursor {
    pub offset: u64,
    pub mtime_ms: i64,
    pub size: u64,
}

/// What one incremental scan produced.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Complete lines parsed this call.
    pub lines: Vec<String>,
    /// Lines that looked like records but failed to parse (torn writes are
    /// NOT counted — a trailing partial line is simply not consumed yet).
    pub malformed: usize,
}

/// Read only the bytes appended to `path` since `cursor`, returning complete
/// lines. A trailing partial line stays unconsumed (its bytes are re-read on
/// the next call once the writer terminates it).
pub(crate) fn read_appended(path: &Path, cursor: &mut FileCursor) -> io::Result<Scan> {
    let meta = fs::metadata(path)?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len();
    // Rotation/truncation: the file shrank below our offset — start over.
    if size < cursor.offset {
        cursor.offset = 0;
    }
    if cursor.offset == size && cursor.mtime_ms == mtime_ms {
        return Ok(Scan::default());
    }
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(cursor.offset))?;
    let mut buf = Vec::new();
    // Bound a single call: at most 64 MiB of new bytes per file per scan.
    let mut limited = file.take(64 * 1024 * 1024);
    limited.read_to_end(&mut buf)?;
    // Only consume through the last newline; a trailing partial line belongs
    // to the next scan.
    let consumed = match buf.iter().rposition(|b| *b == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    let mut scan = Scan::default();
    if consumed > 0 {
        let chunk = &buf[..consumed];
        for line in chunk.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            match std::str::from_utf8(line) {
                Ok(text) => scan.lines.push(text.to_string()),
                Err(_) => scan.malformed += 1,
            }
        }
        cursor.offset += consumed as u64;
    }
    cursor.mtime_ms = mtime_ms;
    cursor.size = size;
    Ok(scan)
}

/// Outcome helpers shared by every reader.
pub(crate) fn unavailable(reason: impl Into<String>) -> ReadOutcome {
    ReadOutcome::unavailable(reason)
}

/// Coverage from scan counters: unreadable/malformed inputs downgrade a
/// complete read to partial; an empty source is unavailable.
pub(crate) fn outcome(
    turns: Vec<LocalTurn>,
    files_found: bool,
    root_missing: bool,
    malformed: usize,
    unreadable: usize,
    no_token_source: Option<&'static str>,
) -> ReadOutcome {
    if root_missing {
        return unavailable("no session directory");
    }
    if !files_found {
        return unavailable("no local session logs found");
    }
    if let Some(reason) = no_token_source {
        return unavailable(reason);
    }
    let mut reason = None;
    if malformed > 0 || unreadable > 0 {
        reason = Some(format!(
            "{} unreadable line(s), {} unreadable file(s) skipped",
            malformed, unreadable
        ));
    }
    ReadOutcome {
        turns,
        coverage: if reason.is_some() {
            crate::usage_sources::Coverage::Partial
        } else {
            crate::usage_sources::Coverage::Complete
        },
        reason,
    }
}

/// Recursively collect files under `root` whose file name passes `accept`,
/// bounded in count and depth. Returns `None` when `root` does not exist.
pub(crate) fn collect_files(
    root: &Path,
    accept: impl Fn(&Path) -> bool,
    max_files: usize,
) -> Option<(Vec<PathBuf>, bool)> {
    if !root.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    let mut truncated = false;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if depth < 8 {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file() && accept(&path) {
                if files.len() >= max_files {
                    truncated = true;
                    break;
                }
                files.push(path);
            }
        }
        if truncated {
            break;
        }
    }
    Some((files, truncated))
}

/// Shared mutable scan state for JSONL readers: per-file cursors plus the
/// accumulated turns (already pruned to the retention horizon).
#[derive(Default)]
pub(crate) struct JsonlState {
    pub cursors: std::collections::HashMap<PathBuf, FileCursor>,
    pub turns: Vec<LocalTurn>,
    pub malformed: usize,
    pub unreadable: usize,
}

impl JsonlState {
    /// Drop turns older than the retention horizon.
    pub fn prune(&mut self, now_ms: i64) {
        let horizon = now_ms - RETENTION_MS;
        self.turns.retain(|t| t.ts >= horizon);
    }

    /// Bound the in-memory turn list even within the horizon.
    pub fn cap(&mut self, max: usize) {
        if self.turns.len() > max {
            let drop = self.turns.len() - max;
            self.turns.drain(..drop);
        }
    }
}

/// Turn cap per reader — generous for 180 days of one agent's turns.
pub(crate) const MAX_TURNS: usize = 200_000;

/// Current time in epoch millis.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Local readers for the agents that keep token logs on disk (issue #53),
/// wired from the same paths the account store uses. Agents without a
/// verified local token source are absent — the Usage page falls back to
/// measured turns and says why.
pub fn readers_for(
    config: &crate::agent_accounts::AgentAccountsConfig,
) -> Vec<std::sync::Arc<dyn LocalUsageReader>> {
    let mut readers: Vec<std::sync::Arc<dyn LocalUsageReader>> = Vec::new();
    // Grok: `~/.grok/auth.json` → sessions at `~/.grok/sessions`.
    if let Some(dir) = config.cli_files.grok_auth.parent() {
        readers.push(std::sync::Arc::new(GrokReader::new(dir.join("sessions"))));
    }
    // Pi: `~/.pi/agent/auth.json` → sessions at `~/.pi/agent/sessions`.
    if let Some(dir) = config.pi_auth.parent() {
        readers.push(std::sync::Arc::new(PiReader::new(dir.join("sessions"))));
    }
    readers.push(std::sync::Arc::new(OpenCodeReader::new(
        config.opencode_db.clone(),
    )));
    readers.push(std::sync::Arc::new(MuseReader::new(
        config.cli_files.muse_sessions.clone(),
    )));
    readers
}

pub use grok::GrokReader;
pub use muse::MuseReader;
pub use opencode::OpenCodeReader;
pub use pi::PiReader;
