//! Per-session on-disk event journal (port of komet's `run-journal.ts`, JSONL-shaped).
//!
//! One append-only JSONL file per chat under `{data_dir}/journals/{chat_id}.jsonl`; each
//! line is `{"seq": n, "event": AgentEvent}` with a monotonically increasing `seq`. The
//! journal is the durable replay source for live streams (`Subscribe` = replay then tail
//! the broadcast hub) and the crash-recovery gauge: a journal whose LAST event is not
//! `Done` belongs to a run that died mid-stream — boot recovery stamps its doc entry
//! `aborted` and closes the journal with a synthetic `Done`.
//!
//! Bounded-window compaction is deferred (whole file kept for now, per M2 scope); a torn
//! trailing line from a crash mid-write is tolerated everywhere.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use komet_proto::AgentEvent;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLine {
    seq: u64,
    event: AgentEvent,
}

/// Borrowing twin of [`JournalLine`] for writes; identical JSON shape.
#[derive(Serialize)]
struct JournalLineRef<'a> {
    seq: u64,
    event: &'a AgentEvent,
}

/// Open-descriptor cap; the oldest-used entry is evicted past it.
const OPEN_FILE_CAP: usize = 16;
/// Initial tail window for recovering the last seq.
const TAIL_CHUNK: u64 = 64 * 1024;

struct ChatJournal {
    file: File,
    next_seq: u64,
    last_used: u64,
    /// True when the file ends without a newline (torn write) — the next append
    /// starts with one so the torn line stays isolated.
    needs_newline: bool,
}

/// Append-only JSONL journal store, one file per chat.
pub struct RunJournal {
    dir: PathBuf,
    open_files: Mutex<HashMap<String, ChatJournal>>,
    tick: AtomicU64,
}

impl RunJournal {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            open_files: Mutex::new(HashMap::new()),
            tick: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ChatJournal>> {
        self.open_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path_for(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_id(chat_id)))
    }

    fn attempts_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.resume", sanitize_id(chat_id)))
    }

    /// Auto-resume revival budget (komet `resumeAttempt`/`MAX_AUTO_RESUME`):
    /// persisted beside the journal so a run that CRASHES THE ENGINE cannot
    /// revive itself in an infinite boot loop.
    pub fn resume_attempts(&self, chat_id: &str) -> u32 {
        std::fs::read_to_string(self.attempts_path(chat_id))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn note_resume_attempt(&self, chat_id: &str) -> u32 {
        let next = self.resume_attempts(chat_id) + 1;
        if let Err(err) = std::fs::write(self.attempts_path(chat_id), next.to_string()) {
            tracing::warn!(chat = %chat_id, error = %err, "resume-attempt ledger write failed");
        }
        next
    }

    /// A cleanly completed turn resets the budget — only consecutive
    /// crash-revive-crash cycles exhaust it.
    pub fn clear_resume_attempts(&self, chat_id: &str) {
        let _ = std::fs::remove_file(self.attempts_path(chat_id));
    }

    /// Append one event; returns its journal seq.
    pub fn append(&self, chat_id: &str, event: &AgentEvent) -> Result<u64, JournalError> {
        let mut files = self.lock();
        if !files.contains_key(chat_id) {
            // Dropping a handle is safe: the next append reopens and reads the tail.
            if files.len() >= OPEN_FILE_CAP {
                evict_oldest(&mut files);
            }
            let path = self.path_for(chat_id);
            let (next_seq, needs_newline) = scan_tail(&path)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            files.insert(
                chat_id.to_string(),
                ChatJournal {
                    file,
                    next_seq,
                    last_used: 0,
                    needs_newline,
                },
            );
        }
        // Entry guaranteed present; avoid unwrap in a library path regardless.
        let Some(journal) = files.get_mut(chat_id) else {
            return Err(JournalError::Io(std::io::Error::other(
                "journal entry vanished under lock",
            )));
        };
        journal.last_used = self.tick.fetch_add(1, Ordering::Relaxed);
        let seq = journal.next_seq;
        let line = serde_json::to_string(&JournalLineRef { seq, event })?;
        let mut buf = Vec::with_capacity(line.len() + 2);
        if journal.needs_newline {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        journal.file.write_all(&buf)?;
        journal.file.flush()?;
        journal.needs_newline = false;
        journal.next_seq = seq + 1;
        Ok(seq)
    }

    /// Events with `seq > after_seq`, in order. A cursor ahead of the last issued seq is
    /// from a previous era (file replaced) — falls back to a full replay, mirroring komet.
    pub fn replay(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let all = read_lines(&path)?;
        let last_seq = all.last().map(|(seq, _)| *seq).unwrap_or(0);
        let from = if after_seq > last_seq { 0 } else { after_seq };
        Ok(all.into_iter().filter(|(seq, _)| *seq > from).collect())
    }

    /// The last event in a chat's journal, if any (ignores a torn tail line).
    pub fn last_event(&self, chat_id: &str) -> Result<Option<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(read_tail(&path)?.0)
    }

    /// Last valid seq (tail read only), `None` for a missing/empty journal.
    pub fn last_seq(&self, chat_id: &str) -> Result<Option<u64>, JournalError> {
        Ok(read_tail(&self.path_for(chat_id))?.0.map(|(seq, _)| seq))
    }

    /// Targeted lookup (issue #10): the `ToolCall` + `ToolResult` events for
    /// one tool part, in journal order. No full replay — one linear scan of
    /// the chat's file, keeping only matching lines. Unknown id or missing /
    /// rotated file yields an empty vec, never an error.
    pub fn events_for_part(&self, chat_id: &str, part_id: &str) -> Vec<AgentEvent> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Vec::new();
        }
        let lines = read_lines(&path).unwrap_or_default();
        lines
            .into_iter()
            .filter_map(|(_, event)| match &event {
                AgentEvent::ToolCall { id, .. } | AgentEvent::ToolResult { id, .. }
                    if id == part_id =>
                {
                    Some(event)
                }
                _ => None,
            })
            .collect()
    }

    /// The compaction summary the journal kept for one cycle (the doc carries
    /// only its size). Frames are replayed in journal order with the fold's own
    /// rule: a `summary` replaces whatever earlier chunks accumulated, a
    /// `summary_delta` extends it — so an interrupted stream that replayed a
    /// cycle rebuilds the same text the host saw live.
    ///
    /// `None` means "there is no summary here" (unknown cycle, a provider that
    /// compacts without one, or a file rotated away), which the RPC reports as
    /// an explicit `unavailable` — never an empty string that reads as "the
    /// agent summarized nothing".
    pub fn compaction_summary(&self, chat_id: &str, compaction_id: &str) -> Option<String> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return None;
        }
        let mut text: Option<String> = None;
        for (_, event) in read_lines(&path).ok()? {
            if let AgentEvent::ContextCompaction {
                id,
                summary,
                summary_delta,
                ..
            } = &event
                && id == compaction_id
            {
                if let Some(full) = summary {
                    text = Some(full.clone());
                }
                if let Some(delta) = summary_delta {
                    text.get_or_insert_with(String::new).push_str(delta);
                }
            }
        }
        text.filter(|text| !text.is_empty())
    }

    /// Chat ids with a journal file on this host, from the file names alone
    /// (no parsing). Used to find journals a remote tombstone orphaned.
    pub fn chat_ids(&self) -> Result<Vec<String>, JournalError> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(chat_id) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(chat_id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Crash-recovery scan: chat ids whose journal's last event is NOT a `Done` — their
    /// runs died mid-stream and need recovery (stamp `aborted`, close the journal).
    pub fn stale_sessions(&self) -> Result<Vec<String>, JournalError> {
        let mut stale = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(chat_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let last = read_lines(&path)?.into_iter().next_back();
            match last {
                Some((_, AgentEvent::Done { .. })) | None => {}
                Some(_) => stale.push(chat_id.to_string()),
            }
        }
        stale.sort();
        Ok(stale)
    }

    /// The transcript of one subagent, projected from the chat's journal.
    ///
    /// Nested traffic is journaled exactly as it arrived: every
    /// [`AgentEvent::Subagent`] wrapper carries the tool id of its DIRECT
    /// parent, so a feed is "the payloads whose wrapper says
    /// `parent_tool_use_id`, in journal order". Wrapper chains are descended,
    /// never assumed — a wire that double-wraps (`B` inside `A`'s line)
    /// attributes `B`'s payload to `B` and shows `A` only the wrapper that
    /// spawned it. That is what keeps a sub-subagent out of its grandparent's
    /// transcript while staying reachable from it.
    ///
    /// `None` means "this host has nothing to show" (never ran the chat, the
    /// journal was discarded with the chat, or the file is unreadable);
    /// `Some(vec![])` means "the journal is here and this feed is empty (yet)".
    /// The RPC turns the first into an explicit `Unavailable` so a missing
    /// journal never renders as an empty transcript.
    pub fn subagent_events(
        &self,
        chat_id: &str,
        parent_tool_use_id: &str,
    ) -> Option<Vec<(u64, AgentEvent)>> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return None;
        }
        let lines = match read_lines(&path) {
            Ok(lines) => lines,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal: subagent projection unreadable");
                return None;
            }
        };
        let mut out = Vec::new();
        for (seq, event) in lines {
            project_feed(&event, parent_tool_use_id, seq, &mut out);
        }
        Some(out)
    }

    /// Remove a chat's journal and its resume budget. Deleting a chat must not
    /// leave a transcript behind, and a stale `.resume` would hand a deleted
    /// run its revival budget on the next crash scan.
    pub fn discard(&self, chat_id: &str) -> Result<(), JournalError> {
        self.lock().remove(chat_id);
        for path in [self.path_for(chat_id), self.attempts_path(chat_id)] {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

/// One journal line into `out`: payloads of the wrapper tagged `target` are
/// emitted unwrapped, a wrapper for someone else is descended, and a wrapper
/// nested under our own feed stops there — it belongs to its own parent.
///
/// `pub(crate)` because a live subscriber applies the same rule to the frames it
/// receives: the replay and the live tail must attribute nested traffic
/// identically, or the two halves of one transcript disagree about who said what.
pub(crate) fn project_feed(
    event: &AgentEvent,
    target: &str,
    seq: u64,
    out: &mut Vec<(u64, AgentEvent)>,
) {
    let AgentEvent::Subagent {
        parent_tool_use_id,
        event: inner,
    } = event
    else {
        return;
    };
    if parent_tool_use_id == target {
        out.push((seq, (**inner).clone()));
    } else {
        project_feed(inner, target, seq, out);
    }
}

/// Parse every valid line; malformed lines (torn tail writes) are skipped.
fn read_lines(path: &Path) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalLine>(&line) {
            Ok(parsed) => out.push((parsed.seq, parsed.event)),
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "journal: skipping malformed line");
            }
        }
    }
    Ok(out)
}

/// Remove the least recently used entry.
fn evict_oldest(files: &mut HashMap<String, ChatJournal>) {
    if let Some(victim) = files
        .iter()
        .min_by_key(|(_, j)| j.last_used)
        .map(|(k, _)| k.clone())
    {
        files.remove(&victim);
    }
}

/// Last valid line plus whether the file ends mid-line, reading only the end
/// of the file (window doubles until a complete valid line is found).
fn read_tail(path: &Path) -> Result<(Option<(u64, AgentEvent)>, bool), JournalError> {
    read_tail_with(path, TAIL_CHUNK)
}

fn read_tail_with(
    path: &Path,
    chunk: u64,
) -> Result<(Option<(u64, AgentEvent)>, bool), JournalError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    let mut window = chunk.max(1);
    let mut needs_newline = false;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        (&mut file).take(len - start).read_to_end(&mut buf)?;
        if window == chunk.max(1) {
            needs_newline = buf.last().is_some_and(|b| *b != b'\n');
        }
        // A window that starts mid-file begins with a partial line: skip it.
        let body = if start > 0 {
            match buf.iter().position(|b| *b == b'\n') {
                Some(i) => &buf[i + 1..],
                None => &buf[buf.len()..],
            }
        } else {
            &buf[..]
        };
        for raw in body.split(|b| *b == b'\n').rev() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if let Ok(parsed) = serde_json::from_slice::<JournalLine>(raw) {
                return Ok((Some((parsed.seq, parsed.event)), needs_newline));
            }
        }
        if start == 0 {
            return Ok((None, needs_newline));
        }
        window = window.saturating_mul(2);
    }
}

/// Next seq (last valid seq + 1, starting at 1) and whether the file ends mid-line.
fn scan_tail(path: &Path) -> Result<(u64, bool), JournalError> {
    let (last, needs_newline) = read_tail(path)?;
    Ok((last.map_or(1, |(seq, _)| seq + 1), needs_newline))
}

/// Journal (`.jsonl`) and resume-budget (`.resume`) paths for `chat_id` under an
/// arbitrary journals directory — profile import copies these files between
/// profiles without opening a `RunJournal`.
pub fn journal_paths(dir: &Path, chat_id: &str) -> (PathBuf, PathBuf) {
    let stem = sanitize_id(chat_id);
    (
        dir.join(format!("{stem}.jsonl")),
        dir.join(format!("{stem}.resume")),
    )
}

/// Chat ids become file names; anything outside a conservative set is replaced so a
/// hostile id cannot traverse paths. (Ids are uuids in practice.)
fn sanitize_id(chat_id: &str) -> String {
    chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use komet_proto::DoneStatus;

    fn text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            reason: None,
        }
    }

    #[test]
    fn appends_are_monotonic_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("a")).unwrap(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        assert_eq!(journal.append("chat-1", &done()).unwrap(), 3);

        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 3);
        let after = journal.replay("chat-1", 2).unwrap();
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0].1, AgentEvent::Done { .. }));
        // Era fallback: cursor ahead of last seq replays everything.
        assert_eq!(journal.replay("chat-1", 99).unwrap().len(), 3);
    }

    #[test]
    fn seq_continues_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
    }

    #[test]
    fn stale_scan_flags_journals_without_terminal_done() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("dead", &text("partial")).unwrap();
        journal.append("clean", &text("full")).unwrap();
        journal.append("clean", &done()).unwrap();
        assert_eq!(journal.stale_sessions().unwrap(), vec!["dead".to_string()]);
        // Closing the stale journal with a Done clears the flag.
        journal.append("dead", &done()).unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
    }

    #[test]
    fn torn_tail_line_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        // Simulate a crash mid-write: garbage with no trailing newline.
        let path = dir.path().join("chat-1.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"event\":{\"type\":\"textD")
            .unwrap();
        drop(f);

        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.replay("chat-1", 0).unwrap().len(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, 2);
    }

    fn tool_call(id: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            id: id.into(),
            call: komet_proto::ToolCall::Exec {
                command: format!("run {id}"),
            },
        }
    }

    fn tool_result(id: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            is_error: false,
            output: Some(format!("out {id}")),
            diff: None,
        }
    }

    #[test]
    fn events_for_part_round_trips_call_and_result_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &text("hello")).unwrap();
        journal.append("chat-1", &tool_call("t1")).unwrap();
        journal.append("chat-1", &tool_call("t2")).unwrap();
        journal.append("chat-1", &tool_result("t1")).unwrap();
        journal.append("chat-1", &done()).unwrap();

        let got = journal.events_for_part("chat-1", "t1");
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], AgentEvent::ToolCall { id, .. } if id == "t1"));
        assert!(matches!(&got[1], AgentEvent::ToolResult { id, .. } if id == "t1"));
    }

    #[test]
    fn events_for_part_unknown_id_or_chat_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &tool_call("t1")).unwrap();
        assert!(journal.events_for_part("chat-1", "nope").is_empty());
        assert!(journal.events_for_part("missing-chat", "t1").is_empty());
    }

    fn compaction(id: &str, summary: Option<&str>, summary_delta: Option<&str>) -> AgentEvent {
        AgentEvent::ContextCompaction {
            id: id.into(),
            status: komet_proto::CompactionStatus::Completed,
            trigger: Some("auto".into()),
            tokens_before: Some(165_000),
            tokens_after: None,
            summary: summary.map(str::to_owned),
            summary_delta: summary_delta.map(str::to_owned),
            error: None,
        }
    }

    #[test]
    fn summary_deltas_are_concatenated_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // Interleaved noise + a second cycle: the fetch must read only its own.
        journal.append("chat-1", &text("warming up")).unwrap();
        journal
            .append("chat-1", &compaction("c1", None, Some("They met ")))
            .unwrap();
        journal
            .append("chat-1", &compaction("c2", None, Some("other cycle")))
            .unwrap();
        journal
            .append("chat-1", &compaction("c1", None, Some("in Paris, then ")))
            .unwrap();
        journal
            .append("chat-1", &compaction("c1", None, Some("parted")))
            .unwrap();

        assert_eq!(
            journal.compaction_summary("chat-1", "c1").as_deref(),
            Some("They met in Paris, then parted"),
            "chunks rebuild in journal order, across a reopen-free read"
        );
        assert_eq!(
            journal.compaction_summary("chat-1", "c2").as_deref(),
            Some("other cycle")
        );
        assert!(journal.compaction_summary("chat-1", "nope").is_none());
        assert!(journal.compaction_summary("missing-chat", "c1").is_none());
    }

    #[test]
    fn a_full_summary_replaces_the_chunks_it_follows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal
            .append("chat-1", &compaction("c1", None, Some("partial ")))
            .unwrap();
        // The provider then emits the whole text: the fold (and so the fetch)
        // replaces, it does not append the recap twice.
        journal
            .append("chat-1", &compaction("c1", Some("the recap"), None))
            .unwrap();
        assert_eq!(
            journal.compaction_summary("chat-1", "c1").as_deref(),
            Some("the recap")
        );
    }

    #[test]
    fn a_cycle_without_summary_is_absent_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // Codex-style: the boundary arrives with no recap at all.
        journal
            .append("chat-1", &compaction("c1", None, None))
            .unwrap();
        assert!(
            journal.compaction_summary("chat-1", "c1").is_none(),
            "no summary must read as unavailable, not as an empty recap"
        );
    }

    fn subagent(parent: &str, event: AgentEvent) -> AgentEvent {
        AgentEvent::Subagent {
            parent_tool_use_id: parent.into(),
            event: Box::new(event),
        }
    }

    #[test]
    fn subagent_feed_is_projected_in_journal_order() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // The parent's own feed interleaves with the child's — the projection
        // must pick out the child's lines and keep their order.
        journal.append("chat-1", &text("parent says")).unwrap();
        journal
            .append("chat-1", &subagent("spawn-a", text("a thinks")))
            .unwrap();
        journal
            .append("chat-1", &subagent("spawn-a", tool_call("a-tool")))
            .unwrap();
        journal
            .append("chat-1", &subagent("spawn-b", text("b thinks")))
            .unwrap();
        journal
            .append("chat-1", &subagent("spawn-a", tool_result("a-tool")))
            .unwrap();
        journal.append("chat-1", &done()).unwrap();

        let a = journal.subagent_events("chat-1", "spawn-a").unwrap();
        assert_eq!(a.len(), 3, "only A's payloads, in journal order");
        assert_eq!(a[0].0, 2);
        assert!(matches!(&a[0].1, AgentEvent::TextDelta { text } if text == "a thinks"));
        assert!(matches!(&a[1].1, AgentEvent::ToolCall { id, .. } if id == "a-tool"));
        assert!(
            matches!(&a[2].1, AgentEvent::ToolResult { id, .. } if id == "a-tool"),
            "the child's tool settles on the child's feed"
        );
        let b = journal.subagent_events("chat-1", "spawn-b").unwrap();
        assert_eq!(b.len(), 1);
        assert!(matches!(&b[0].1, AgentEvent::TextDelta { text } if text == "b thinks"));
        // A tool id is not a spawn id: the parent's chips have no feed.
        assert!(journal.subagent_events("chat-1", "t1").unwrap().is_empty());
    }

    #[test]
    fn a_nested_wrapper_stays_under_its_own_parent() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // A wire that double-wraps: B's content arrives INSIDE A's line. It is
        // B's content, so it must not read as A's — A sees the wrapper (the
        // spawn chip), and B's feed is reached by B's id.
        journal
            .append(
                "chat-1",
                &subagent("spawn-a", subagent("spawn-b", text("deep"))),
            )
            .unwrap();

        let a = journal.subagent_events("chat-1", "spawn-a").unwrap();
        assert_eq!(a.len(), 1);
        assert!(
            matches!(&a[0].1, AgentEvent::Subagent { parent_tool_use_id, .. }
                if parent_tool_use_id == "spawn-b"),
            "A's feed carries B's wrapper, not B's prose: {:?}",
            a[0].1
        );
        let b = journal.subagent_events("chat-1", "spawn-b").unwrap();
        assert_eq!(b.len(), 1, "the chain is descended, not flattened away");
        assert!(matches!(&b[0].1, AgentEvent::TextDelta { text } if text == "deep"));
        assert_eq!(b[0].0, 1, "the deep event keeps its journal seq");
    }

    #[test]
    fn a_missing_journal_is_absent_not_an_empty_feed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &text("no children here")).unwrap();
        assert!(
            journal.subagent_events("chat-1", "spawn-a").is_some(),
            "a journal with no nested traffic is an empty feed, not an error"
        );
        assert!(
            journal.subagent_events("never-ran", "spawn-a").is_none(),
            "no journal on this host must read as unavailable"
        );
    }

    #[test]
    fn discard_removes_the_resume_budget_with_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("chat-1", &text("half a turn")).unwrap();
        journal.note_resume_attempt("chat-1");
        assert!(
            journal
                .stale_sessions()
                .unwrap()
                .contains(&"chat-1".to_string())
        );

        journal.discard("chat-1").unwrap();

        // No resurrection: the crash scan has nothing to flag, and the revival
        // budget restarts rather than carrying over a deleted run's counts.
        assert!(journal.stale_sessions().unwrap().is_empty());
        assert_eq!(journal.resume_attempts("chat-1"), 0);
        assert!(journal.subagent_events("chat-1", "spawn-a").is_none());
        assert!(journal.replay("chat-1", 0).unwrap().is_empty());
    }

    fn line(seq: u64) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&JournalLineRef {
                seq,
                event: &text("x")
            })
            .unwrap()
        )
    }

    #[test]
    fn tail_reader_handles_empty_partial_and_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        assert!(read_tail(&path).unwrap().0.is_none());
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_tail(&path).unwrap(), (None, false));

        std::fs::write(&path, format!("{}{}{{\"seq\":3,\"ev", line(1), line(2))).unwrap();
        let (last, nl) = read_tail(&path).unwrap();
        assert_eq!(last.unwrap().0, 2);
        assert!(nl);

        // Far larger than the chunk; a tiny chunk forces window growth past a long last line.
        let body: String = (1..=500).map(line).collect();
        std::fs::write(&path, &body).unwrap();
        let (last, nl) = read_tail_with(&path, 16).unwrap();
        assert_eq!(last.unwrap().0, 500);
        assert!(!nl);
        assert_eq!(read_tail(&path).unwrap().0.unwrap().0, 500);
        assert_eq!(scan_tail(&path).unwrap(), (501, false));
    }

    #[test]
    fn eviction_drops_a_single_least_recently_used_entry() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        for i in 0..OPEN_FILE_CAP {
            journal.append(&format!("c{i}"), &text("a")).unwrap();
        }
        journal.append("c0", &text("b")).unwrap();
        journal.append("new", &text("a")).unwrap();
        let files = journal.lock();
        assert_eq!(files.len(), OPEN_FILE_CAP);
        assert!(files.contains_key("c0") && files.contains_key("new"));
        assert!(!files.contains_key("c1"));
        drop(files);
        assert_eq!(journal.append("c1", &text("c")).unwrap(), 2);
    }
}
