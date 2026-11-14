//! Grok CLI local usage reader.
//!
//! Format verified by the probe (`docs/research/usage-sources.md`):
//! `~/.grok/sessions/<urlencoded-cwd>/<session-uuid>/updates.jsonl`, one
//! JSON-RPC-style notification per line. Only `update.sessionUpdate ==
//! "turn_completed"` records carry usage:
//! `params.update.usage.{inputTokens,outputTokens,cachedReadTokens,
//! reasoningTokens,modelUsage{<model>:…}}`; the timestamp is
//! `params._meta.agentTimestampMs` (epoch ms), falling back to the
//! top-level `timestamp` (epoch seconds).
//!
//! Token mapping (verified on 13 real records): `totalTokens =
//! inputTokens + outputTokens`; `reasoningTokens` is a subset of output,
//! `cachedReadTokens` a subset of input — so `input → input_tokens`,
//! `cachedReadTokens → cached_input_tokens`, `outputTokens → output_tokens`,
//! `reasoningTokens → reasoning_tokens`. `cacheCreationTokens` has no
//! `LocalTurn` slot and is ignored.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::usage_sources::{LocalTurn, LocalUsageReader, ReadOutcome};

use super::{JsonlState, MAX_TURNS, collect_files, now_ms, outcome, read_appended};

/// Grok session updates live under `<grok home>/sessions/`.
pub struct GrokReader {
    root: PathBuf,
    state: Mutex<JsonlState>,
}

impl GrokReader {
    /// `root` is the `sessions` directory (`~/.grok/sessions`).
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

/// Parse one `updates.jsonl` record into a [`LocalTurn`]. Only
/// `turn_completed` notifications carry usage; everything else is skipped.
fn parse_record(rec: &Value) -> Option<LocalTurn> {
    let params = rec.get("params")?;
    let update = params.get("update")?;
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("turn_completed") {
        return None;
    }
    let usage = update.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let ts = params
        .get("_meta")
        .and_then(|m| m.get("agentTimestampMs"))
        .and_then(Value::as_i64)
        .or_else(|| {
            rec.get("timestamp")
                .and_then(Value::as_i64)
                .map(|s| s * 1000)
        })?;
    // `modelUsage` maps model id → its own counters; a turn served by
    // several models has no single model, so it stays unpriced rather than
    // guessing.
    let model = usage
        .get("modelUsage")
        .and_then(Value::as_object)
        .filter(|models| models.len() == 1)
        .and_then(|models| models.keys().next().cloned());
    Some(LocalTurn {
        ts,
        session: params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        model,
        input_tokens: u64_field(usage, "inputTokens"),
        cached_input_tokens: u64_field(usage, "cachedReadTokens"),
        output_tokens: u64_field(usage, "outputTokens"),
        reasoning_tokens: u64_field(usage, "reasoningTokens"),
    })
}

impl LocalUsageReader for GrokReader {
    fn agent(&self) -> &'static str {
        "grok"
    }

    fn source_label(&self) -> &'static str {
        "local logs"
    }

    fn read_since(&self, since_ms: i64) -> ReadOutcome {
        let now = now_ms();
        // Layout: sessions/<urlencoded-cwd>/<uuid>/updates.jsonl — the file
        // name filter keeps chat_history/events/summary out of the scan.
        let Some((files, truncated)) = collect_files(
            &self.root,
            |p| p.file_name().is_some_and(|n| n == "updates.jsonl"),
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
            // The session id comes from the record itself (`params.sessionId`).
            let cursor = state.cursors.entry(path.clone()).or_default();
            match read_appended(path, cursor) {
                Ok(scan) => {
                    state.malformed += scan.malformed;
                    for line in scan.lines {
                        match serde_json::from_str::<Value>(&line) {
                            Ok(rec) => {
                                if let Some(turn) = parse_record(&rec) {
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
        state.cursors.retain(|p, _| file_set.contains(p.as_path()));
        let turns: Vec<LocalTurn> = state
            .turns
            .iter()
            .filter(|t| t.ts >= since_ms)
            .cloned()
            .collect();
        outcome(
            turns,
            !files.is_empty(),
            false,
            state.malformed,
            state.unreadable + truncated as usize,
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "komet-grok-reader-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn turn_line(ts_ms: i64, model: &str, input: u64, output: u64) -> String {
        serde_json::json!({
            "timestamp": ts_ms / 1000,
            "method": "_x.ai/session/update",
            "params": {
                "sessionId": "sess-1",
                "update": {
                    "sessionUpdate": "turn_completed",
                    "stop_reason": "end_turn",
                    "usage": {
                        "inputTokens": input,
                        "outputTokens": output,
                        "totalTokens": input + output,
                        "cachedReadTokens": input / 2,
                        "cacheCreationTokens": 0,
                        "reasoningTokens": output / 2,
                        "modelCalls": 1,
                        "modelUsage": {
                            model: {
                                "inputTokens": input,
                                "outputTokens": output,
                                "totalTokens": input + output,
                                "cachedReadTokens": input / 2,
                                "cacheCreationTokens": 0,
                                "reasoningTokens": output / 2,
                                "modelCalls": 1
                            }
                        },
                        "numTurns": 1
                    },
                    "elapsed_ms": 100
                },
                "_meta": {"eventId": "e1", "agentTimestampMs": ts_ms}
            }
        })
        .to_string()
    }

    fn chunk_line() -> &'static str {
        r#"{"timestamp":1763000000,"method":"_x.ai/session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk"}}}"#
    }

    #[test]
    fn missing_directory_is_unavailable() {
        let reader = GrokReader::new(PathBuf::from("/nonexistent/grok/sessions"));
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(out.reason.as_deref(), Some("no session directory"));
    }

    #[test]
    fn turn_completed_becomes_a_turn_other_updates_do_not() {
        let dir = tempdir("turns");
        let session_dir = dir.join("%2Fhome%2Fuser").join("sess-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("updates.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let ts = now_ms() - 60_000;
        writeln!(f, "{}", turn_line(ts, "grok-4.7", 1000, 100)).unwrap();
        writeln!(f, "{}", chunk_line()).unwrap();
        let reader = GrokReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Complete);
        assert_eq!(out.turns.len(), 1);
        let turn = &out.turns[0];
        assert_eq!(turn.ts, ts);
        assert_eq!(turn.session, "sess-1");
        assert_eq!(turn.model.as_deref(), Some("grok-4.7"));
        assert_eq!(turn.input_tokens, 1000);
        assert_eq!(turn.cached_input_tokens, 500);
        assert_eq!(turn.output_tokens, 100);
        assert_eq!(turn.reasoning_tokens, 50);
        assert!(reader.read_since(turn.ts + 1).turns.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_model_turn_has_no_single_model() {
        let dir = tempdir("multimodel");
        let session_dir = dir.join("cwd").join("sess-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        // `timestamp` fallback is epoch seconds — keep the fixture on a
        // whole second so the parsed ms value round-trips exactly.
        let ts = (now_ms() - 60_000) / 1000 * 1000;
        let line = serde_json::json!({
            "timestamp": ts / 1000,
            "params": {
                "sessionId": "sess-1",
                "update": {
                    "sessionUpdate": "turn_completed",
                    "usage": {
                        "inputTokens": 10, "outputTokens": 5, "totalTokens": 15,
                        "modelUsage": {"grok-4.6": {}, "grok-4.7": {}}
                    }
                }
            }
        })
        .to_string();
        std::fs::write(session_dir.join("updates.jsonl"), format!("{line}\n")).unwrap();
        let reader = GrokReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 1);
        assert_eq!(out.turns[0].model, None);
        assert_eq!(out.turns[0].ts, ts);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_and_malformed_lines() {
        let dir = tempdir("incremental");
        let session_dir = dir.join("cwd").join("sess-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("updates.jsonl");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{}", turn_line(now_ms() - 60_000, "grok-4.6", 10, 1)).unwrap();
            writeln!(f, "not-a-json-record").unwrap();
        }
        let reader = GrokReader::new(dir.clone());
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 1);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Partial);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{}", turn_line(now_ms() - 30_000, "grok-4.6", 20, 2)).unwrap();
        }
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 2);
        assert_eq!(out.turns[1].input_tokens, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
