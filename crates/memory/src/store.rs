//! The local SQLite memory store: one database per device data dir
//! (`<data_dir>/memory/memory.sqlite`), WAL mode, FTS5 when the bundled
//! SQLite provides it.
//!
//! Writers go through a single background thread fed by a bounded channel —
//! the engine's publish path only ever does a `try_send`. All reads
//! (search, briefing, settings, forget) share one `Mutex<Connection>` with
//! the writer; WAL keeps readers from blocking it meaningfully.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::policy;
use crate::record::{
    AgentDigest, BriefingDigest, ChatCursor, FailureDigest, FileTouch, MemoryEvent, MemoryItem,
    MemoryRecord,
};

const DB_FILE: &str = "memory.sqlite";
/// Bounded queue between the engine's publish path and the writer thread.
/// Overflow is not an error: the journal stays authoritative and catch-up
/// replays the gap (see `gap_seq` in `memory_chat_state`).
pub const QUEUE_CAPACITY: usize = 2048;
/// Writer flush cadence: batch up to this many items or this much time.
const WRITE_BATCH: usize = 100;
const WRITE_FLUSH: Duration = Duration::from_millis(100);
/// Recency window for the handoff briefing.
pub const BRIEFING_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// The assembled memory service. Cheap to share (`Arc`): every method takes
/// `&self` and the heavy work lives on the writer thread.
pub struct MemoryService {
    /// Read connection (search, recent, stats, cursors, settings reads).
    conn: Mutex<Connection>,
    /// Dedicated write connection for admin mutations (`forget_*`,
    /// `set_*`, gap markers) so they never queue behind — or block — the
    /// read path. WAL admits one writer at a time; these are rare ops.
    write: Mutex<Connection>,
    tx: SyncSender<MemoryItem>,
    /// Kept so the writer's lifetime is observable in tests; dropping the
    /// service does not join it (engine lifetime ≈ process lifetime).
    #[allow(dead_code)]
    writer: JoinHandle<()>,
    dir: PathBuf,
    fts: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MemoryService {
    /// Open (or create) the store under `dir` — typically
    /// `<data_dir>/memory`. Creates the schema, enables WAL, and spawns the
    /// single writer thread.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(DB_FILE);
        let open_conn = |path: &Path| -> Result<Connection, MemoryError> {
            let conn = Connection::open(path)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.pragma_update(None, "cache_size", -2000)?; // ~2 MB
            conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            Ok(conn)
        };
        let mut conn = open_conn(&path)?;
        // Must precede table creation to take effect; a no-op on databases
        // created before this pragma existed (they'd need a VACUUM).
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        migrate(&mut conn)?;
        // One-shot planner stats after migrations (cheap on a fresh/small db).
        conn.execute_batch("PRAGMA optimize;")?;
        let fts = ensure_fts(&conn)?;
        let (tx, rx) = sync_channel::<MemoryItem>(QUEUE_CAPACITY);
        let writer_conn = open_conn(&path)?;
        let write_conn = open_conn(&path)?;
        let writer = std::thread::Builder::new()
            .name("komet-memory-writer".into())
            .spawn(move || writer_loop(writer_conn, rx))
            .map_err(|e| MemoryError::Other(format!("spawn writer: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
            write: Mutex::new(write_conn),
            tx,
            writer,
            dir,
            fts,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the bundled SQLite gave us FTS5 (verified at open).
    pub fn fts_enabled(&self) -> bool {
        self.fts
    }

    /// Non-blocking enqueue for the publish path. `false` = queue full; the
    /// caller records the gap and catch-up heals it from the journal.
    pub fn try_enqueue(&self, item: MemoryItem) -> bool {
        self.tx.try_send(item).is_ok()
    }

    /// Blocking enqueue for catch-up (bounded by the same queue; the writer
    /// keeps draining so this cannot stall forever in practice).
    pub fn enqueue(&self, item: MemoryItem) -> Result<(), MemoryError> {
        self.tx
            .send(item)
            .map_err(|_| MemoryError::Other("memory writer queue closed".into()))
    }

    /// Synchronously record an enqueue gap (called ONLY on queue overflow —
    /// rare, and a tiny single-row write; the hot path never reaches this).
    /// Goes through the dedicated write connection so it can't stall reads.
    pub fn note_gap(&self, chat_id: &str, seq: u64) -> Result<(), MemoryError> {
        let conn = lock(&self.write);
        conn.execute(
            "INSERT INTO memory_chat_state(chat_id, gap_seq) VALUES(?1, ?2)
             ON CONFLICT(chat_id) DO UPDATE SET
               gap_seq = MIN(COALESCE(memory_chat_state.gap_seq, ?2), ?2)",
            params![chat_id, seq as i64],
        )?;
        Ok(())
    }

    /// Replay cursor for one chat (`last_seq` = highest processed seq,
    /// `gap_seq` = lowest known-dropped seq).
    pub fn chat_cursor(&self, chat_id: &str) -> Result<ChatCursor, MemoryError> {
        let conn = lock(&self.conn);
        let row = conn
            .query_row(
                "SELECT last_seq, gap_seq FROM memory_chat_state WHERE chat_id = ?1",
                params![chat_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((last, gap)) => ChatCursor {
                last_seq: last.max(0) as u64,
                gap_seq: gap.map(|g| g.max(0) as u64),
            },
            None => ChatCursor {
                last_seq: 0,
                gap_seq: None,
            },
        })
    }

    /// All chat ids the indexer has seen (for catch-up scans).
    pub fn known_chats(&self) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare("SELECT chat_id FROM memory_chat_state")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── settings ──────────────────────────────────────────────────────────

    /// Whether memory indexing/injection is enabled for a project.
    /// Default: enabled — the store is local-only and allow-listed already.
    pub fn is_enabled(&self, project_id: &str) -> Result<bool, MemoryError> {
        let conn = lock(&self.conn);
        let enabled = conn
            .query_row(
                "SELECT enabled FROM memory_projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(1);
        Ok(enabled != 0)
    }

    pub fn set_enabled(&self, project_id: &str, enabled: bool) -> Result<(), MemoryError> {
        let now = now_ms();
        let conn = lock(&self.write);
        conn.execute(
            "INSERT INTO memory_projects(project_id, enabled, created_at_ms, updated_at_ms)
             VALUES(?1, ?2, ?3, ?3)
             ON CONFLICT(project_id) DO UPDATE SET
               enabled = excluded.enabled, updated_at_ms = excluded.updated_at_ms",
            params![project_id, enabled as i64, now],
        )?;
        Ok(())
    }

    /// User-configured extra path exclusions (globs/prefixes) for a project.
    pub fn exclusions(&self, project_id: &str) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let raw = conn
            .query_row(
                "SELECT exclusions FROM memory_projects WHERE project_id = ?1",
                params![project_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| "[]".into());
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    pub fn set_exclusions(
        &self,
        project_id: &str,
        exclusions: &[String],
    ) -> Result<(), MemoryError> {
        let now = now_ms();
        let conn = lock(&self.write);
        conn.execute(
            "INSERT INTO memory_projects(project_id, enabled, exclusions, created_at_ms, updated_at_ms)
             VALUES(?1, 1, ?2, ?3, ?3)
             ON CONFLICT(project_id) DO UPDATE SET
               exclusions = excluded.exclusions, updated_at_ms = excluded.updated_at_ms",
            params![project_id, serde_json::to_string(exclusions)?, now],
        )?;
        Ok(())
    }

    // ── queries ───────────────────────────────────────────────────────────

    /// FTS + recency search over allow-listed metadata. Empty/whitespace
    /// queries return an empty vec (callers wanting "recent" use
    /// [`Self::recent`]).
    pub fn search(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEvent>, MemoryError> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim_matches('"').to_string())
            .filter(|t| !t.is_empty())
            .take(8)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let conn = lock(&self.conn);
        let limit = limit.clamp(1, 200) as i64;
        let events = if self.fts {
            let match_expr = terms
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let mut stmt = conn.prepare(
                "SELECT e.id, e.project_id, e.chat_id, e.source_seq, e.agent, e.model,
                        e.kind, e.occurred_at_ms, e.tool_name, e.relative_path, e.outcome,
                        e.input_tokens, e.cached_input_tokens, e.output_tokens,
                        e.reasoning_tokens, e.duration_ms
                 FROM memory_events e
                 JOIN memory_fts f ON f.rowid = e.id
                 WHERE memory_fts MATCH ?1 AND e.project_id = ?2
                 ORDER BY e.occurred_at_ms DESC
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![match_expr, project_id, limit], row_to_event)?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            // LIKE fallback when FTS5 is unavailable: AND of substring matches
            // over the stored search_text.
            let mut sql = String::from(
                "SELECT id, project_id, chat_id, source_seq, agent, model, kind,
                        occurred_at_ms, tool_name, relative_path, outcome,
                        input_tokens, cached_input_tokens, output_tokens,
                        reasoning_tokens, duration_ms
                 FROM memory_events
                 WHERE project_id = ?1",
            );
            for i in 0..terms.len() {
                sql.push_str(&format!(" AND search_text LIKE ?{} ESCAPE '\\'", i + 2));
            }
            sql.push_str(" ORDER BY occurred_at_ms DESC LIMIT ?");
            let mut stmt = conn.prepare(&sql)?;
            // Positional binding in SQL order: project, one LIKE per term, limit.
            let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(terms.len() + 2);
            values.push(project_id.to_string().into());
            for t in &terms {
                values.push(format!("%{}%", escape_like(t)).into());
            }
            values.push(limit.into());
            let rows = stmt.query_map(params_from_iter(values.iter()), row_to_event)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        Ok(events)
    }

    /// Most recent events for a project (inspection + briefing source).
    pub fn recent(
        &self,
        project_id: &str,
        limit: usize,
        since_ms: Option<i64>,
    ) -> Result<Vec<MemoryEvent>, MemoryError> {
        let conn = lock(&self.conn);
        let limit = limit.clamp(1, 1000) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, chat_id, source_seq, agent, model, kind,
                    occurred_at_ms, tool_name, relative_path, outcome,
                    input_tokens, cached_input_tokens, output_tokens,
                    reasoning_tokens, duration_ms
             FROM memory_events
             WHERE project_id = ?1 AND (?2 IS NULL OR occurred_at_ms >= ?2)
             ORDER BY occurred_at_ms DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![project_id, since_ms, limit], row_to_event)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Deterministic 300–500-token handoff briefing, or `None` when the
    /// project has no useful memory (disabled, empty, or all stale).
    /// `exclude_chat_id` drops the receiving chat's own activity — the agent
    /// already knows its own work.
    pub fn briefing(
        &self,
        project_id: &str,
        exclude_chat_id: Option<&str>,
    ) -> Result<Option<String>, MemoryError> {
        if !self.is_enabled(project_id)? {
            return Ok(None);
        }
        let since = now_ms().saturating_sub(BRIEFING_WINDOW_MS);
        let digest = self.briefing_digest(project_id, exclude_chat_id, since)?;
        Ok(crate::briefing::build(&digest, now_ms()))
    }

    /// SQL-aggregated briefing input: per-agent rollups, top modified paths
    /// and recent failures. Grouped queries mean one high-volume agent
    /// cannot consume the whole row budget.
    pub fn briefing_digest(
        &self,
        project_id: &str,
        exclude_chat_id: Option<&str>,
        since_ms: i64,
    ) -> Result<BriefingDigest, MemoryError> {
        let conn = lock(&self.conn);
        let chats: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT chat_id) FROM memory_events
             WHERE project_id = ?1 AND (?2 IS NULL OR chat_id != ?2)
               AND occurred_at_ms >= ?3",
            params![project_id, exclude_chat_id, since_ms],
            |r| r.get(0),
        )?;

        // Per-agent volume + recency.
        let mut stmt = conn.prepare(
            "SELECT agent,
                    SUM(kind = 'session_start') AS sessions,
                    SUM(kind = 'tool_call') AS tool_calls,
                    SUM(kind = 'tool_call' AND tool_name = 'read_file') AS file_reads,
                    MAX(occurred_at_ms) AS last_ms
             FROM memory_events
             WHERE project_id = ?1 AND (?2 IS NULL OR chat_id != ?2)
               AND occurred_at_ms >= ?3
             GROUP BY agent",
        )?;
        let mut agents: Vec<AgentDigest> = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, i64>(2)? as u32,
                    r.get::<_, i64>(3)? as u32,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(
                |(agent, sessions, tool_calls, file_reads, last_ms)| AgentDigest {
                    agent,
                    sessions,
                    tool_calls,
                    file_reads,
                    last_status: None,
                    last_activity_ms: last_ms,
                    modified_paths: Vec::new(),
                    failures: Vec::new(),
                },
            )
            .collect();
        drop(stmt);

        // Latest session_end outcome per agent.
        let mut stmt = conn.prepare(
            "SELECT agent, outcome FROM memory_events e
             WHERE project_id = ?1 AND kind = 'session_end'
               AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
               AND occurred_at_ms = (
                 SELECT MAX(occurred_at_ms) FROM memory_events
                 WHERE project_id = ?1 AND agent = e.agent AND kind = 'session_end'
                   AND (?2 IS NULL OR chat_id != ?2)
               )",
        )?;
        let statuses: Vec<(String, Option<String>)> = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (agent, status) in statuses {
            if let Some(d) = agents.iter_mut().find(|a| a.agent == agent) {
                d.last_status = status;
            }
        }

        // Top modified paths per agent (write-ish file ops only — reads are
        // counted, not listed; `tool_result` rows never carry paths here).
        let mut stmt = conn.prepare(
            "SELECT agent, relative_path, MAX(occurred_at_ms) AS last_ms
             FROM memory_events
             WHERE project_id = ?1 AND kind = 'tool_call'
               AND relative_path IS NOT NULL
               AND tool_name IN ('write_file', 'edit_file', 'apply_patch')
               AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
             GROUP BY agent, relative_path
             ORDER BY last_ms DESC
             LIMIT 200",
        )?;
        let touches: Vec<(String, String, i64)> = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (agent, path, last_ms) in touches {
            if let Some(d) = agents.iter_mut().find(|a| a.agent == agent)
                && d.modified_paths.len() < 8
            {
                d.modified_paths.push(FileTouch { path, last_ms });
            }
        }
        // Keep each agent's paths newest-first.
        for d in &mut agents {
            d.modified_paths
                .sort_by_key(|t| std::cmp::Reverse(t.last_ms));
        }

        // Recent failures, attributed per agent.
        let mut stmt = conn.prepare(
            "SELECT agent, kind, tool_name, relative_path, outcome, occurred_at_ms
             FROM memory_events
             WHERE project_id = ?1
               AND ((kind = 'tool_result' AND outcome = 'error')
                 OR (kind = 'session_end' AND outcome IN ('errored', 'interrupted')))
               AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
             ORDER BY occurred_at_ms DESC
             LIMIT 30",
        )?;
        type FailureRow = (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        );
        let failures: Vec<FailureRow> = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (agent, kind, tool_name, rel_path, outcome, ms) in failures {
            let label = if kind == "tool_result" {
                format!(
                    "{} on {}",
                    tool_name.as_deref().unwrap_or("tool"),
                    rel_path.as_deref().unwrap_or("?")
                )
            } else {
                format!("run {}", outcome.as_deref().unwrap_or("failed"))
            };
            if let Some(d) = agents.iter_mut().find(|a| a.agent == agent)
                && d.failures.len() < 5
            {
                d.failures.push(FailureDigest { label, last_ms: ms });
            }
        }

        Ok(BriefingDigest {
            chats: chats.max(0) as u64,
            agents,
        })
    }

    /// "Who touched this path?" — the highest-value query for agent
    /// handoff: every recorded op under `path_prefix` (project-relative),
    /// newest first.
    pub fn touched(
        &self,
        project_id: &str,
        path_prefix: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEvent>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT id, project_id, chat_id, source_seq, agent, model, kind,
                    occurred_at_ms, tool_name, relative_path, outcome,
                    input_tokens, cached_input_tokens, output_tokens,
                    reasoning_tokens, duration_ms
             FROM memory_events
             WHERE project_id = ?1 AND relative_path IS NOT NULL
               AND (relative_path = ?2 OR relative_path LIKE ?3 ESCAPE '\\')
             ORDER BY occurred_at_ms DESC
             LIMIT ?4",
        )?;
        let prefix = format!("{}%", escape_like(path_prefix));
        let rows = stmt.query_map(
            params![project_id, path_prefix, prefix, limit.clamp(1, 200) as i64],
            row_to_event,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Chats with a persisted enqueue gap — the periodic catch-up tick only
    /// runs a journal scan when this is non-empty.
    pub fn chats_with_gaps(&self) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt =
            conn.prepare("SELECT chat_id FROM memory_chat_state WHERE gap_seq IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── forget ────────────────────────────────────────────────────────────

    /// Delete every retained record for one chat (events + FTS rows + cursor).
    pub fn forget_chat(&self, chat_id: &str) -> Result<usize, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM memory_events WHERE chat_id = ?1")?;
            stmt.query_map(params![chat_id], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if self.fts {
            delete_fts_rows(&tx, &ids)?;
        }
        let removed = tx.execute(
            "DELETE FROM memory_events WHERE chat_id = ?1",
            params![chat_id],
        )?;
        tx.execute(
            "DELETE FROM memory_chat_state WHERE chat_id = ?1",
            params![chat_id],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Delete every retained record for a whole project (all derived data).
    pub fn forget_project(&self, project_id: &str) -> Result<usize, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM memory_events WHERE project_id = ?1")?;
            stmt.query_map(params![project_id], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if self.fts {
            delete_fts_rows(&tx, &ids)?;
        }
        let removed = tx.execute(
            "DELETE FROM memory_events WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM memory_chat_state WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM memory_projects WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Delete one retained record by id (per-fact/session forget granularity
    /// grows in later phases; events are the Phase-1 unit).
    pub fn forget_event(&self, id: i64) -> Result<bool, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        if self.fts {
            delete_fts_rows(&tx, &[id])?;
        }
        let removed = tx.execute("DELETE FROM memory_events WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Storage footprint for the resource panel (Phase 5 consumes this).
    pub fn stats(&self, project_id: &str) -> Result<MemoryStats, MemoryError> {
        let conn = lock(&self.conn);
        let events: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_events WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        let chats: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT chat_id) FROM memory_events WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        let bytes = std::fs::metadata(self.dir.join(DB_FILE))
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(MemoryStats {
            events,
            chats,
            bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryStats {
    pub events: i64,
    pub chats: i64,
    pub bytes: u64,
}

/// Escape `LIKE` metacharacters (`%`, `_`, `\`) for `ESCAPE '\'` clauses.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEvent> {
    Ok(MemoryEvent {
        id: r.get(0)?,
        project_id: r.get(1)?,
        chat_id: r.get(2)?,
        source_seq: r.get::<_, i64>(3)?.max(0) as u64,
        agent: r.get(4)?,
        model: r.get(5)?,
        kind: r.get(6)?,
        occurred_at_ms: r.get(7)?,
        tool_name: r.get(8)?,
        relative_path: r.get(9)?,
        outcome: r.get(10)?,
        input_tokens: r.get(11)?,
        cached_input_tokens: r.get(12)?,
        output_tokens: r.get(13)?,
        reasoning_tokens: r.get(14)?,
        duration_ms: r.get(15)?,
    })
}

fn delete_fts_rows(conn: &Connection, ids: &[i64]) -> Result<(), MemoryError> {
    if ids.is_empty() {
        return Ok(());
    }
    // External-content FTS5 delete: the 'delete' command needs the rowid and
    // the indexed text. We stored search_text on the row, so fetch it first.
    let mut stmt =
        conn.prepare("SELECT e.id, e.search_text FROM memory_events e WHERE e.id = ?1")?;
    let mut del = conn.prepare(
        "INSERT INTO memory_fts(memory_fts, rowid, search_text) VALUES('delete', ?1, ?2)",
    )?;
    for id in ids {
        if let Some(text) = stmt
            .query_row(params![id], |r| r.get::<_, String>(1))
            .optional()?
        {
            del.execute(params![id, text])?;
        }
    }
    Ok(())
}

// ── schema ────────────────────────────────────────────────────────────────

fn migrate(conn: &mut Connection) -> Result<(), MemoryError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE memory_events (
               id INTEGER PRIMARY KEY,
               project_id TEXT NOT NULL,
               chat_id TEXT NOT NULL,
               source_seq INTEGER NOT NULL,
               agent TEXT NOT NULL,
               model TEXT,
               kind TEXT NOT NULL,
               occurred_at_ms INTEGER NOT NULL,
               tool_name TEXT,
               relative_path TEXT,
               outcome TEXT,
               input_tokens INTEGER,
               cached_input_tokens INTEGER,
               output_tokens INTEGER,
               reasoning_tokens INTEGER,
               duration_ms INTEGER,
               search_text TEXT NOT NULL DEFAULT '',
               UNIQUE(project_id, chat_id, source_seq)
             );
             CREATE INDEX idx_memory_events_project_time
               ON memory_events(project_id, occurred_at_ms);
             CREATE INDEX idx_memory_events_project_path
               ON memory_events(project_id, relative_path);
             CREATE INDEX idx_memory_events_chat_seq
               ON memory_events(chat_id, source_seq);

             CREATE TABLE memory_projects (
               project_id TEXT PRIMARY KEY,
               enabled INTEGER NOT NULL DEFAULT 1,
               exclusions TEXT NOT NULL DEFAULT '[]',
               created_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL
             );

             CREATE TABLE memory_chat_state (
               chat_id TEXT PRIMARY KEY,
               project_id TEXT,
               last_seq INTEGER NOT NULL DEFAULT 0,
               gap_seq INTEGER
             );
             PRAGMA user_version = 1;",
        )?;
    }
    Ok(())
}

/// FTS5 over the metadata `search_text` column (external content). Returns
/// false when the bundled SQLite lacks FTS5 — callers fall back to LIKE.
fn ensure_fts(conn: &Connection) -> Result<bool, MemoryError> {
    let probe = conn.execute_batch(
        // prefix='2 3' indexes 2-3 char token prefixes: path-ish queries
        // ("src/mai") still hit without a trigram tokenizer.
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
           search_text, content='memory_events', content_rowid='id',
           prefix='2 3'
         );",
    );
    match probe {
        Ok(()) => Ok(true),
        Err(err) => {
            tracing::warn!(error = %err, "memory: FTS5 unavailable, using LIKE search");
            Ok(false)
        }
    }
}

// ── writer ────────────────────────────────────────────────────────────────

fn writer_loop(conn: Connection, rx: Receiver<MemoryItem>) {
    let mut batch: Vec<MemoryItem> = Vec::with_capacity(WRITE_BATCH);
    // Block for the next item; sender loss ends the loop after a final
    // drain + flush.
    while let Ok(item) = rx.recv() {
        batch.push(item);
        // Collect until the batch cap or a quiet window elapses, so a burst
        // lands in one transaction and a trickle still flushes ~100ms out.
        let deadline = Instant::now() + WRITE_FLUSH;
        loop {
            if batch.len() >= WRITE_BATCH {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(item) => batch.push(item),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    while let Ok(item) = rx.try_recv() {
                        batch.push(item);
                    }
                    flush(&conn, &mut batch);
                    return;
                }
            }
        }
        flush(&conn, &mut batch);
    }
    while let Ok(item) = rx.try_recv() {
        batch.push(item);
    }
    flush(&conn, &mut batch);
}

fn flush(conn: &Connection, batch: &mut Vec<MemoryItem>) {
    if batch.is_empty() {
        return;
    }
    let items = std::mem::take(batch);
    if let Err(err) = apply_batch(conn, &items) {
        // Never lose silently: log and DROP is not acceptable for indexed
        // events — but the journal remains authoritative, so the honest move
        // is to mark every affected chat gapped and let catch-up replay.
        // We can't reach the engine from here; the gap markers below do it.
        tracing::error!(error = %err, items = items.len(), "memory: writer batch failed");
        for item in &items {
            if let Some((chat_id, seq)) = item_chat_seq(item) {
                let _ = note_gap_on(conn, chat_id, seq);
            }
        }
    }
    batch.clear();
}

fn item_chat_seq(item: &MemoryItem) -> Option<(&str, u64)> {
    match item {
        MemoryItem::Record(r) => Some((r.chat_id.as_str(), r.source_seq)),
        MemoryItem::Checkpoint { chat_id, seq } | MemoryItem::Resume { chat_id, seq } => {
            Some((chat_id.as_str(), *seq))
        }
    }
}

fn note_gap_on(conn: &Connection, chat_id: &str, seq: u64) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO memory_chat_state(chat_id, gap_seq) VALUES(?1, ?2)
         ON CONFLICT(chat_id) DO UPDATE SET
           gap_seq = MIN(COALESCE(memory_chat_state.gap_seq, ?2), ?2)",
        params![chat_id, seq as i64],
    )?;
    Ok(())
}

fn apply_batch(conn: &Connection, items: &[MemoryItem]) -> Result<(), MemoryError> {
    let tx = conn.unchecked_transaction()?;
    // Per-batch settings cache: project enabled flag + exclusions.
    let mut settings: HashMap<String, (bool, Vec<String>)> = HashMap::new();
    let mut insert = tx.prepare(
        "INSERT OR IGNORE INTO memory_events(
           project_id, chat_id, source_seq, agent, model, kind, occurred_at_ms,
           tool_name, relative_path, outcome, input_tokens, cached_input_tokens,
           output_tokens, reasoning_tokens, duration_ms, search_text
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         RETURNING id",
    )?;
    let mut fts_insert = if fts_table_exists(&tx)? {
        Some(tx.prepare("INSERT INTO memory_fts(rowid, search_text) VALUES(?1, ?2)")?)
    } else {
        None
    };
    let mut cursor = tx.prepare(
        "INSERT INTO memory_chat_state(chat_id, project_id, last_seq)
         VALUES(?1, ?2, ?3)
         ON CONFLICT(chat_id) DO UPDATE SET
           project_id = COALESCE(excluded.project_id, memory_chat_state.project_id),
           last_seq = MAX(memory_chat_state.last_seq, excluded.last_seq)",
    )?;
    let mut gap_clear = tx.prepare(
        "UPDATE memory_chat_state SET gap_seq = NULL
         WHERE chat_id = ?1 AND gap_seq IS NOT NULL AND gap_seq <= ?2",
    )?;

    for item in items {
        match item {
            MemoryItem::Record(rec) => {
                let (enabled, exclusions) = project_settings(&tx, &mut settings, &rec.project_id)?;
                if enabled {
                    let rel_path = rec
                        .relative_path
                        .as_deref()
                        .filter(|p| !policy::user_excluded(p, &exclusions));
                    let search_text = search_text(rec, rel_path);
                    let id: Option<i64> = insert
                        .query_row(
                            params![
                                rec.project_id,
                                rec.chat_id,
                                rec.source_seq as i64,
                                rec.agent,
                                rec.model,
                                rec.kind,
                                rec.occurred_at_ms,
                                rec.tool_name,
                                rel_path,
                                rec.outcome,
                                rec.input_tokens,
                                rec.cached_input_tokens,
                                rec.output_tokens,
                                rec.reasoning_tokens,
                                rec.duration_ms,
                                search_text,
                            ],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if let (Some(id), Some(fts)) = (id, fts_insert.as_mut()) {
                        fts.execute(params![id, search_text])?;
                    }
                }
                // Disabled projects still advance the cursor: "disable" means
                // stop recording from this point, not "backfill on re-enable".
                cursor.execute(params![rec.chat_id, rec.project_id, rec.source_seq as i64])?;
            }
            MemoryItem::Checkpoint { chat_id, seq } => {
                cursor.execute(params![chat_id, Option::<String>::None, *seq as i64])?;
            }
            MemoryItem::Resume { chat_id, seq } => {
                cursor.execute(params![chat_id, Option::<String>::None, *seq as i64])?;
                gap_clear.execute(params![chat_id, *seq as i64])?;
            }
        }
    }
    drop(insert);
    drop(fts_insert);
    drop(cursor);
    drop(gap_clear);
    tx.commit()?;
    Ok(())
}

fn project_settings(
    conn: &Connection,
    cache: &mut HashMap<String, (bool, Vec<String>)>,
    project_id: &str,
) -> Result<(bool, Vec<String>), MemoryError> {
    if let Some(hit) = cache.get(project_id) {
        return Ok(hit.clone());
    }
    let row = conn
        .query_row(
            "SELECT enabled, exclusions FROM memory_projects WHERE project_id = ?1",
            params![project_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let (enabled, exclusions) = match row {
        Some((e, raw)) => (e != 0, serde_json::from_str(&raw).unwrap_or_default()),
        None => (true, Vec::new()),
    };
    cache.insert(project_id.to_string(), (enabled, exclusions.clone()));
    Ok((enabled, exclusions))
}

fn fts_table_exists(conn: &Connection) -> Result<bool, MemoryError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_fts'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Metadata string indexed for search: kind + tool + path + agent + model +
/// outcome. Never contains prompts, args, or content.
fn search_text(rec: &MemoryRecord, rel_path: Option<&str>) -> String {
    let mut parts: Vec<&str> = vec![rec.kind, &rec.agent];
    if let Some(t) = rec.tool_name.as_deref() {
        parts.push(t);
    }
    if let Some(p) = rel_path {
        parts.push(p);
    }
    if let Some(m) = rec.model.as_deref() {
        parts.push(m);
    }
    if let Some(o) = rec.outcome.as_deref() {
        parts.push(o);
    }
    parts.join(" ")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MemoryRecord;

    fn svc() -> (MemoryService, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let svc = MemoryService::open(dir.path().join("memory")).unwrap();
        (svc, dir)
    }

    fn rec(seq: u64, kind: &'static str) -> MemoryRecord {
        MemoryRecord {
            project_id: "p_test".into(),
            chat_id: "chat-1".into(),
            source_seq: seq,
            agent: "claude-code".into(),
            model: Some("opus".into()),
            kind,
            occurred_at_ms: now_ms(),
            tool_name: None,
            relative_path: None,
            outcome: None,
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            duration_ms: None,
        }
    }

    /// The writer flushes on a ~100ms cadence; poll until `cond` holds.
    fn wait_for(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for memory writer");
    }

    #[test]
    fn records_are_searchable_and_idempotent() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        let mut tool = rec(2, "tool_call");
        tool.tool_name = Some("edit_file".into());
        tool.relative_path = Some("src/main.rs".into());
        svc.enqueue(MemoryItem::Record(Box::new(tool))).unwrap();
        svc.enqueue(MemoryItem::Checkpoint {
            chat_id: "chat-1".into(),
            seq: 2,
        })
        .unwrap();
        // Replay the same seqs: INSERT OR IGNORE must not duplicate.
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(2, "tool_call"))))
            .unwrap();

        wait_for(|| svc.stats("p_test").unwrap().events == 2);
        let hits = svc.search("p_test", "edit_file main.rs", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].relative_path.as_deref(), Some("src/main.rs"));
        assert!(
            svc.search("p_test", "nonexistent-term", 10)
                .unwrap()
                .is_empty()
        );

        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.last_seq, 2);
        assert_eq!(cursor.gap_seq, None);
    }

    #[test]
    fn disabled_project_records_nothing_but_advances_cursor() {
        let (svc, _dir) = svc();
        svc.set_enabled("p_test", false).unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.chat_cursor("chat-1").unwrap().last_seq == 1);
        assert_eq!(svc.stats("p_test").unwrap().events, 0);
        assert!(svc.briefing("p_test", None).unwrap().is_none());
    }

    #[test]
    fn user_exclusions_drop_matching_paths() {
        let (svc, _dir) = svc();
        svc.set_exclusions("p_test", &["secrets/**".to_string(), "gen".to_string()])
            .unwrap();
        let mut a = rec(1, "tool_call");
        a.tool_name = Some("read_file".into());
        a.relative_path = Some("secrets/key.txt".into());
        let mut b = rec(2, "tool_call");
        b.tool_name = Some("read_file".into());
        b.relative_path = Some("gen/out.rs".into());
        let mut c = rec(3, "tool_call");
        c.tool_name = Some("read_file".into());
        c.relative_path = Some("src/keep.rs".into());
        for item in [a, b, c] {
            svc.enqueue(MemoryItem::Record(Box::new(item))).unwrap();
        }
        wait_for(|| svc.stats("p_test").unwrap().events == 3);
        let events = svc.recent("p_test", 10, None).unwrap();
        let paths: Vec<_> = events
            .iter()
            .filter_map(|e| e.relative_path.as_deref())
            .collect();
        assert_eq!(paths, vec!["src/keep.rs"]);
    }

    #[test]
    fn forget_removes_events_fts_and_cursor() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        assert_eq!(svc.forget_chat("chat-1").unwrap(), 1);
        assert_eq!(svc.stats("p_test").unwrap().events, 0);
        // Cursor row is gone: a fresh replay starts from zero again.
        assert_eq!(svc.chat_cursor("chat-1").unwrap().last_seq, 0);
        // FTS row must be gone too (external-content delete path).
        assert!(
            svc.search("p_test", "session_start", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forget_project_clears_settings_too() {
        let (svc, _dir) = svc();
        svc.set_enabled("p_test", false).unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.chat_cursor("chat-1").unwrap().last_seq == 1);
        svc.forget_project("p_test").unwrap();
        assert!(svc.is_enabled("p_test").unwrap(), "default is enabled");
        assert_eq!(svc.chat_cursor("chat-1").unwrap().last_seq, 0);
    }

    #[test]
    fn briefing_is_none_for_empty_project() {
        let (svc, _dir) = svc();
        assert!(svc.briefing("p_empty", None).unwrap().is_none());
    }

    #[test]
    fn gap_marker_survives_and_resume_clears_it() {
        let (svc, _dir) = svc();
        svc.note_gap("chat-9", 41).unwrap();
        let cursor = svc.chat_cursor("chat-9").unwrap();
        assert_eq!(cursor.gap_seq, Some(41));
        svc.enqueue(MemoryItem::Resume {
            chat_id: "chat-9".into(),
            seq: 50,
        })
        .unwrap();
        wait_for(|| svc.chat_cursor("chat-9").unwrap().gap_seq.is_none());
        assert_eq!(svc.chat_cursor("chat-9").unwrap().last_seq, 50);
    }
}
