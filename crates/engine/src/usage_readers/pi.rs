//! Pi (`pi` / `pi-acp`) local usage reader.
//!
//! Format verified by the probe (`docs/research/usage-sources.md`):
//! `~/.pi/agent/sessions/<cwd-slug>/<ISO-ts>_<uuid>.jsonl`, one JSON object
//! per line. Assistant messages carry
//! `message.usage.{input,output,cacheRead,cacheWrite,totalTokens}` and
//! `message.model`; `message.timestamp` is epoch millis (the record-level
//! `timestamp` is an ISO string, used as fallback).
//!
//! Token mapping: `input → input_tokens`, `cacheRead → cached_input_tokens`,
//! `output → output_tokens`. There is no reasoning field. `cacheWrite` and
//! `totalTokens` are ignored — the probe could not verify whether
//! `totalTokens` includes the cache fields (every sampled record was an
//! error turn with all-zero counters), so the components are used instead.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::usage_sources::{LocalTurn, LocalUsageReader, ReadOutcome};

use super::{JsonlState, MAX_TURNS, collect_files, now_ms, outcome, read_appended};

/// Pi session logs live next to the agent's auth file:
/// `<agent dir>/sessions/**`. `root` is that `sessions` directory.
pub struct PiReader {
    root: PathBuf,
    state: Mutex<JsonlState>,
}

impl PiReader {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            root: sessions_root,
            state: Mutex::new(JsonlState::default()),
        }
    }
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse one JSONL record into a [`LocalTurn`]. Returns `None` for every
/// record that is not an assistant message with a usage block.
fn parse_record(rec: &Value, session: &str) -> Option<LocalTurn> {
    if rec.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = rec.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let usage = message.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let ts = message
        .get("timestamp")
        .and_then(Value::as_i64)
        .or_else(|| {
            rec.get("timestamp")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp_millis())
        })?;
    Some(LocalTurn {
        ts,
        session: session.to_string(),
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_tokens: u64_field(usage, "input"),
        cached_input_tokens: u64_field(usage, "cacheRead"),
        output_tokens: u64_field(usage, "output"),
        reasoning_tokens: 0,
    })
}

impl LocalUsageReader for PiReader {
    fn agent(&self) -> &'static str {
        "pi"
    }

    fn source_label(&self) -> &'static str {
        "local logs"
    }

    fn read_since(&self, since_ms: i64) -> ReadOutcome {
        let now = now_ms();
        let Some((files, truncated)) = collect_files(
            &self.root,
            |p| p.extension().is_some_and(|e| e == "jsonl"),
            20_000,
        ) else {
            return outcome(Vec::new(), false, true, 0, 0, None);
        };
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        state.prune(now);
        let file_set: std::collections::HashSet<&Path> =
            files.iter().map(PathBuf::as_path).collect();
        for path in &files {
            // Session id = file stem (`<ISO-ts>_<uuid>`).
            let session = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let cursor = state.cursors.entry(path.clone()).or_default();
            match read_appended(path, cursor) {
                Ok(scan) => {
                    state.malformed += scan.malformed;
                    for line in scan.lines {
                        match serde_json::from_str::<Value>(&line) {
                            Ok(rec) => {
                                if let Some(turn) = parse_record(&rec, &session) {
                                    state.turns.push(turn);
                                }
                            }
                            Err(_) => state.malformed += 1,
                        }
                    }
                }
                Err(_) => state.unreadable += 1,
            }
        }
        state.cap(MAX_TURNS);
        // Cursors for files that disappeared are dropped to bound the map.
        state.cursors.retain(|p, _| file_set.contains(p.as_path()));
        let malformed = state.malformed;
        let unreadable = state.unreadable + truncated as usize;
        let turns: Vec<LocalTurn> = state
            .turns
            .iter()
            .filter(|t| t.ts >= since_ms)
            .cloned()
            .collect();
        outcome(turns, !files.is_empty(), false, malformed, unreadable, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "komet-pi-reader-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(dir: &Path, name: &str, lines: &[&str]) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    fn assistant_line(model: &str, input: u64, output: u64, cache_read: u64, ts_ms: i64) -> String {
        serde_json::json!({
            "type": "message",
            "id": "m1",
            "parentId": null,
            "timestamp": "2026-11-14T10:00:00.000Z",
            "message": {
                "role": "assistant",
                "model": model,
                "usage": {
                    "input": input,
                    "output": output,
                    "cacheRead": cache_read,
                    "cacheWrite": 0,
                    "totalTokens": input + output,
                    "cost": {"input": 0, "output": 0, "total": 0}
                },
                "timestamp": ts_ms,
                "stopReason": "stop"
            }
        })
        .to_string()
    }

    #[test]
    fn missing_directory_is_unavailable() {
        let reader = PiReader::new(PathBuf::from("/nonexistent/pi/sessions"));
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(out.reason.as_deref(), Some("no session directory"));
    }

    #[test]
    fn empty_directory_is_unavailable() {
        let dir = tempdir("empty");
        let reader = PiReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(out.reason.as_deref(), Some("no local session logs found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assistant_messages_become_turns_and_user_messages_do_not() {
        let dir = tempdir("turns");
        let ts = now_ms() - 60_000;
        let line = assistant_line("gpt-5-mini", 100, 20, 30, ts);
        write_session(
            &dir,
            "cwd-slug/2026-11-14T10-00-00-000Z_abc.jsonl",
            &[r#"{"type":"message","message":{"role":"user"}}"#, &line],
        );
        let reader = PiReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Complete);
        assert_eq!(out.turns.len(), 1);
        let turn = &out.turns[0];
        assert_eq!(turn.input_tokens, 100);
        assert_eq!(turn.cached_input_tokens, 30);
        assert_eq!(turn.output_tokens, 20);
        assert_eq!(turn.model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(turn.ts, ts);
        assert!(turn.session.contains("2026-11-14"));
        // since_ms filters without re-reading.
        assert!(reader.read_since(turn.ts + 1).turns.is_empty());
        assert_eq!(reader.read_since(0).turns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_reads_pick_up_appends_and_skip_malformed() {
        let dir = tempdir("incremental");
        let path = dir.join("s/2026-11-14T10-00-00-000Z_x.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let first = assistant_line("m", 1, 1, 0, now_ms() - 60_000);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{first}").unwrap();
            writeln!(f, "not json").unwrap();
        }
        let reader = PiReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 1);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Partial);
        assert!(out.reason.unwrap().contains("unreadable"));
        // Append a second turn: only the new line is parsed.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{}", assistant_line("m", 2, 2, 0, now_ms() - 30_000)).unwrap();
        }
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 2);
        assert_eq!(out.turns[1].input_tokens, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_last_line_waits_for_its_newline() {
        let dir = tempdir("truncated");
        let path = dir.join("s/f.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line = assistant_line("m", 5, 5, 0, now_ms() - 60_000);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "{line}").unwrap(); // no trailing newline yet
        }
        let reader = PiReader::new(dir.clone());
        assert!(reader.read_since(0).turns.is_empty());
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f).unwrap();
        }
        assert_eq!(reader.read_since(0).turns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn many_files_stay_bounded() {
        let dir = tempdir("volume");
        let line = assistant_line("m", 1, 1, 0, now_ms() - 60_000);
        for i in 0..50 {
            write_session(&dir, &format!("s/f{i}.jsonl"), &[&line]);
        }
        let reader = PiReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 50);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Complete);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
