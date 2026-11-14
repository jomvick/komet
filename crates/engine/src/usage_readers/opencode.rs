//! OpenCode local usage reader (SQLite store).
//!
//! Format verified by the probe (`docs/research/usage-sources.md`):
//! `~/.local/share/opencode/opencode.db`. Each model turn is a `part` row
//! whose `data` JSON has `type == "step-finish"` and a `tokens` object
//! `{input, output, reasoning, total, cache: {read, write}}`; the row's
//! `time_created`/`session_id` columns give the timestamp and session, and
//! `json_extract(message.data, '$.modelID')` the per-turn model (verified on
//! the live DB — the field is `modelID`, not `model.id`).
//!
//! Token mapping: `input → input_tokens`, `cache.read → cached_input_tokens`,
//! `output → output_tokens`, `reasoning → reasoning_tokens`. The provider's
//! own `total` is NOT used — on ~19k real records it only approximates
//! `input + cache.read + output + reasoning`. `cache.write` has no
//! `LocalTurn` slot and is ignored.
//!
//! Incremental: rows are append-mostly, so the reader remembers the last
//! seen `rowid` and only parses newer rows; turns older than the retention
//! horizon are never queried. The DB is opened read-only — OpenCode owns it.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::usage_sources::{LocalTurn, LocalUsageReader, ReadOutcome};

use super::{MAX_TURNS, RETENTION_MS, now_ms, outcome};

/// OpenCode's SQLite store (`opencode.db`).
pub struct OpenCodeReader {
    db: PathBuf,
    state: Mutex<OpencodeState>,
}

#[derive(Default)]
struct OpencodeState {
    /// Highest `part.rowid` already consumed. `None` = not initialized.
    last_rowid: Option<i64>,
    turns: Vec<LocalTurn>,
    malformed: usize,
    /// Set when the DB could not be opened/queried — surfaced as the reason.
    error: Option<String>,
}

impl OpenCodeReader {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db: db_path,
            state: Mutex::new(OpencodeState::default()),
        }
    }
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse a `part.data` JSON blob into a [`LocalTurn`]. Only `step-finish`
/// records carry tokens.
fn parse_part(data: &str, ts: i64, session: String, model: Option<String>) -> Option<LocalTurn> {
    let rec: Value = serde_json::from_str(data).ok()?;
    if rec.get("type").and_then(Value::as_str) != Some("step-finish") {
        return None;
    }
    let tokens = rec.get("tokens")?;
    if !tokens.is_object() {
        return None;
    }
    Some(LocalTurn {
        ts,
        session,
        model,
        input_tokens: u64_field(tokens, "input"),
        cached_input_tokens: tokens
            .get("cache")
            .and_then(|c| c.get("read"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: u64_field(tokens, "output"),
        reasoning_tokens: u64_field(tokens, "reasoning"),
    })
}

impl OpenCodeReader {
    /// Rows appended since `last_rowid`, restricted to the retention horizon
    /// and to `step-finish` parts (the JSON type check runs in SQLite so the
    /// multi-MB `data` blobs of other part types are never parsed here).
    fn fetch_new(&self, state: &mut OpencodeState, horizon_ms: i64) -> Result<(), String> {
        if !self.db.is_file() {
            return Err("no opencode database".to_string());
        }
        let conn = Connection::open_with_flags(
            &self.db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| format!("cannot open opencode.db read-only: {e}"))?;
        // First run: skip everything older than the horizon without parsing
        // JSON — rowid order tracks insertion order, so the cursor starts at
        // the newest row that could still matter.
        let last_rowid = match state.last_rowid {
            Some(id) => id,
            None => {
                let id: i64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(rowid), 0) FROM part WHERE time_created < ?1",
                        [horizon_ms],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("cannot read part cursor: {e}"))?;
                state.last_rowid = Some(id);
                id
            }
        };
        let mut stmt = conn
            .prepare(
                "SELECT p.rowid, p.time_created, p.session_id, p.data, \
                 COALESCE(json_extract(m.data, '$.modelID'), json_extract(m.data, '$.model.id')) \
                 FROM part p JOIN message m ON m.id = p.message_id \
                 WHERE p.rowid > ?1 AND p.time_created >= ?2 \
                   AND json_extract(p.data, '$.type') = 'step-finish' \
                 ORDER BY p.rowid",
            )
            .map_err(|e| format!("cannot query opencode parts: {e}"))?;
        let rows = stmt
            .query_map([last_rowid, horizon_ms], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| format!("cannot read opencode parts: {e}"))?;
        state.error = None;
        let mut max_rowid = last_rowid;
        for row in rows {
            let (rowid, ts, session, data, model) =
                row.map_err(|e| format!("cannot decode opencode part: {e}"))?;
            max_rowid = max_rowid.max(rowid);
            match parse_part(&data, ts, session, model) {
                Some(turn) => state.turns.push(turn),
                None => state.malformed += 1,
            }
        }
        state.last_rowid = Some(max_rowid);
        Ok(())
    }
}

impl LocalUsageReader for OpenCodeReader {
    fn agent(&self) -> &'static str {
        "opencode"
    }

    fn source_label(&self) -> &'static str {
        "local logs"
    }

    fn read_since(&self, since_ms: i64) -> ReadOutcome {
        let now = now_ms();
        let horizon = now - RETENTION_MS;
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        state.turns.retain(|t| t.ts >= horizon);
        if let Err(err) = self.fetch_new(&mut state, horizon) {
            state.error = Some(err);
        }
        if state.turns.len() > MAX_TURNS {
            let drop = state.turns.len() - MAX_TURNS;
            state.turns.drain(..drop);
        }
        if let Some(err) = &state.error {
            return ReadOutcome::unavailable(err.clone());
        }
        let turns: Vec<LocalTurn> = state
            .turns
            .iter()
            .filter(|t| t.ts >= since_ms)
            .cloned()
            .collect();
        outcome(turns, self.db.is_file(), false, state.malformed, 0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "komet-opencode-reader-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_db(path: &Path, parts: &[(&str, i64, &str, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, model TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                                time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        for (i, (part_id, ts, session, data)) in parts.iter().enumerate() {
            let msg_id = format!("msg_{i}");
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    msg_id,
                    session,
                    r#"{"modelID":"gpt-5","providerID":"p","role":"assistant"}"#
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                rusqlite::params![part_id, msg_id, session, ts, data],
            )
            .unwrap();
        }
    }

    fn step_finish(input: u64, output: u64, reasoning: u64, cache_read: u64) -> String {
        serde_json::json!({
            "type": "step-finish",
            "reason": "stop",
            "tokens": {
                "total": input + output + reasoning,
                "input": input,
                "output": output,
                "reasoning": reasoning,
                "cache": {"write": 0, "read": cache_read}
            },
            "cost": 0
        })
        .to_string()
    }

    #[test]
    fn missing_database_is_unavailable() {
        let reader = OpenCodeReader::new(PathBuf::from("/nonexistent/opencode.db"));
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Unavailable);
        assert_eq!(out.reason.as_deref(), Some("no opencode database"));
    }

    #[test]
    fn step_finish_parts_become_turns() {
        let dir = tempdir("turns");
        let db = dir.join("opencode.db");
        let t1 = now_ms() - 60_000;
        let t2 = now_ms() - 30_000;
        build_db(
            &db,
            &[
                ("p1", t1, "ses_1", &step_finish(1000, 100, 50, 400)),
                ("p2", t2 - 1, "ses_1", r#"{"type":"text","text":"hi"}"#),
                ("p3", t2, "ses_2", &step_finish(2000, 200, 0, 0)),
            ],
        );
        let reader = OpenCodeReader::new(db);
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Complete);
        assert_eq!(out.turns.len(), 2);
        let first = &out.turns[0];
        assert_eq!(first.ts, t1);
        assert_eq!(first.session, "ses_1");
        assert_eq!(first.model.as_deref(), Some("gpt-5"));
        assert_eq!(first.input_tokens, 1000);
        assert_eq!(first.cached_input_tokens, 400);
        assert_eq!(first.output_tokens, 100);
        assert_eq!(first.reasoning_tokens, 50);
        assert!(reader.read_since(first.ts + 1).turns.len() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn incremental_reads_pick_up_new_rows_only() {
        let dir = tempdir("incremental");
        let db = dir.join("opencode.db");
        build_db(
            &db,
            &[("p1", now_ms() - 60_000, "ses_1", &step_finish(10, 1, 0, 0))],
        );
        let reader = OpenCodeReader::new(db.clone());
        assert_eq!(reader.read_since(0).turns.len(), 1);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES ('msg_x', 'ses_1', '{}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES ('p2', 'msg_x', 'ses_1', ?2, ?2, ?1)",
                rusqlite::params![step_finish(20, 2, 0, 0), now_ms() - 30_000],
            )
            .unwrap();
        }
        let out = reader.read_since(0);
        assert_eq!(out.turns.len(), 2);
        assert_eq!(out.turns[1].input_tokens, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_database_is_complete_with_no_turns() {
        let dir = tempdir("empty");
        let db = dir.join("opencode.db");
        build_db(&db, &[]);
        let reader = OpenCodeReader::new(db);
        let out = reader.read_since(0);
        assert_eq!(out.coverage, crate::usage_sources::Coverage::Complete);
        assert!(out.turns.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
