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
use serde::Serialize;

use crate::facts::{FactKind, FactScope, FactSource, MemoryFact, NewMemoryFact};
use crate::graph::{GraphNeighbor, MemorySymbol};
use crate::policy;
use crate::record::{
    AgentDigest, BriefingDigest, ChatCursor, FactDigest, FailureDigest, FileTouch, MemoryEvent,
    MemoryItem, MemoryRecord,
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
/// Raw-event retention. Older rows are pruned by the engine's maintenance
/// tick; replay cursors only move forward, so catch-up cannot re-create
/// them.
pub const RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;

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

/// One-way export into an explicitly chosen new or previously-owned
/// directory. A marker prevents overwriting an unrelated existing vault.
pub fn export_obsidian(
    service: &MemoryService,
    project_id: &str,
    destination: &Path,
) -> Result<ObsidianExportReport, MemoryError> {
    const MARKER: &str = ".komet-memory-export";
    let destination = destination.to_path_buf();
    if destination.exists() {
        let metadata = std::fs::symlink_metadata(&destination)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || !destination.join(MARKER).is_file()
        {
            return Err(MemoryError::Other(
                "export destination exists and is not a Komet-owned export".into(),
            ));
        }
    } else {
        std::fs::create_dir_all(&destination)?;
        std::fs::write(destination.join(MARKER), "Komet one-way memory export\n")?;
    }

    let facts = service.facts(project_id, None, true, 10_000)?;
    let events = service.recent(project_id, 10_000, None)?;
    let facts_dir = destination.join("Facts");
    let sessions_dir = destination.join("Sessions");
    ensure_export_directory(&facts_dir)?;
    ensure_export_directory(&sessions_dir)?;

    for fact in &facts {
        let path_link = fact
            .source_path
            .as_deref()
            .map(|path| format!("\nsourcePath: {}\n", yaml_scalar(path)))
            .unwrap_or_default();
        let content = format!(
            "---\nid: {}\nprojectId: {}\nkind: {}\nscope: {}\nsource: {}\nconfidence: {}\nuserConfirmed: {}\nvalidFromMs: {}\nstaleAtMs: {}\nchatId: {}\nsourceEventSeq: {}\n{}---\n# {}\n\n{}\n\n## Provenance\n- agent: {}\n- model: {}\n- replacedBy: {}\n",
            fact.id,
            yaml_scalar(&fact.project_id),
            fact.kind.as_str(),
            fact.scope.as_str(),
            fact.source.as_str(),
            fact.confidence,
            fact.user_confirmed,
            fact.valid_from_ms,
            fact.stale_at_ms.map_or("null".into(), |ms| ms.to_string()),
            yaml_optional(fact.chat_id.as_deref()),
            fact.source_event_seq
                .map_or("null".into(), |seq| seq.to_string()),
            path_link,
            markdown_text(&fact.content),
            markdown_text(&fact.content),
            yaml_optional(fact.source_agent.as_deref()),
            yaml_optional(fact.source_model.as_deref()),
            fact.replaced_by.map_or("null".into(), |id| id.to_string()),
        );
        write_export_file(
            &facts_dir.join(format!("{}-{}.md", fact.kind.as_str(), fact.id)),
            &content,
        )?;
    }

    let mut chats: HashMap<String, Vec<MemoryEvent>> = HashMap::new();
    for event in events {
        chats.entry(event.chat_id.clone()).or_default().push(event);
    }
    for (chat_id, chat_events) in &mut chats {
        chat_events.sort_by_key(|event| (event.occurred_at_ms, event.source_seq));
        let linked_facts: Vec<_> = facts
            .iter()
            .filter(|fact| fact.chat_id.as_deref() == Some(chat_id.as_str()))
            .map(|fact| format!("- [[../Facts/{}-{}]]", fact.kind.as_str(), fact.id))
            .collect();
        let mut markdown = format!(
            "---\nchatId: {}\nprojectId: {}\nevents: {}\n---\n# Session {}\n\n## Related facts\n{}\n\n## Activity\n",
            yaml_scalar(chat_id),
            yaml_scalar(project_id),
            chat_events.len(),
            markdown_text(chat_id),
            if linked_facts.is_empty() {
                "- None linked".to_string()
            } else {
                linked_facts.join("\n")
            }
        );
        for event in chat_events.iter() {
            let path_link = event
                .relative_path
                .as_deref()
                .map(|path| format!(" · [[{}]]", markdown_text(path)))
                .unwrap_or_default();
            markdown.push_str(&format!(
                "- {} · {} · {}{}{}\n",
                event.occurred_at_ms,
                markdown_text(&event.agent),
                markdown_text(&event.kind),
                event
                    .tool_name
                    .as_deref()
                    .map(|tool| format!(" · {}", markdown_text(tool)))
                    .unwrap_or_default(),
                path_link
            ));
        }
        write_export_file(
            &sessions_dir.join(format!("{}.md", stable_export_slug(chat_id))),
            &markdown,
        )?;
    }
    Ok(ObsidianExportReport {
        facts: facts.len(),
        sessions: chats.len(),
    })
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

fn yaml_optional(value: Option<&str>) -> String {
    value.map_or_else(|| "null".into(), yaml_scalar)
}

fn markdown_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ")
}

fn stable_export_slug(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    crate::hex(&digest[..12])
}

fn write_export_file(path: &Path, content: &str) -> Result<(), MemoryError> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(MemoryError::Other(
            "refusing to write through an export symlink".into(),
        ));
    }
    let temporary = path.with_extension("md.tmp");
    std::fs::write(&temporary, content)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn ensure_export_directory(path: &Path) -> Result<(), MemoryError> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(MemoryError::Other(
                "export subdirectory is not a regular directory".into(),
            ));
        }
    } else {
        std::fs::create_dir(path)?;
    }
    Ok(())
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
    facts_fts: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsidianExportReport {
    pub facts: usize,
    pub sessions: usize,
}

impl MemoryService {
    pub fn export_obsidian(
        &self,
        project_id: &str,
        destination: &Path,
    ) -> Result<ObsidianExportReport, MemoryError> {
        export_obsidian(self, project_id, destination)
    }

    /// Open (or create) the store under `dir` — typically
    /// `<data_dir>/memory`. Creates the schema, enables WAL, and spawns the
    /// single writer thread.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(DB_FILE);
        let open_conn = |path: &Path| -> Result<Connection, MemoryError> {
            let conn = Connection::open(path)?;
            // auto_vacuum must be set before any table exists to take
            // effect; it is a no-op on databases that already have tables.
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.pragma_update(None, "cache_size", -2000)?; // ~2 MB
            conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            Ok(conn)
        };
        let mut conn = open_conn(&path)?;
        migrate(&mut conn)?;
        // One-shot planner stats after migrations (cheap on a fresh/small db).
        conn.execute_batch("PRAGMA optimize;")?;
        let fts = ensure_fts(&conn)?;
        let facts_fts = ensure_facts_fts(&conn)?;
        let (tx, rx) = sync_channel::<MemoryItem>(QUEUE_CAPACITY);
        let writer_conn = open_conn(&path)?;
        let write_conn = open_conn(&path)?;
        let writer = std::thread::Builder::new()
            .name("komet-memory-writer".into())
            .spawn(move || writer_loop(writer_conn, rx, fts))
            .map_err(|e| MemoryError::Other(format!("spawn writer: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
            write: Mutex::new(write_conn),
            tx,
            writer,
            dir,
            fts,
            facts_fts,
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
    /// Each dropped seq is its own row: a drop during an active replay must
    /// survive the `Resume` that heals the older gap.
    pub fn note_gap(&self, chat_id: &str, seq: u64) -> Result<(), MemoryError> {
        let conn = lock(&self.write);
        conn.execute(
            "INSERT OR IGNORE INTO memory_chat_gaps(chat_id, seq) VALUES(?1, ?2)",
            params![chat_id, seq as i64],
        )?;
        Ok(())
    }

    /// Replay cursor for one chat (`last_seq` = highest processed seq,
    /// `gap_seq` = lowest known-dropped seq still unhealed).
    pub fn chat_cursor(&self, chat_id: &str) -> Result<ChatCursor, MemoryError> {
        let conn = lock(&self.conn);
        let row = conn
            .query_row(
                "SELECT last_seq,
                        (SELECT MIN(seq) FROM memory_chat_gaps WHERE chat_id = ?1)
                 FROM memory_chat_state WHERE chat_id = ?1",
                params![chat_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        // A chat known ONLY through gap rows (every record dropped before
        // the first flush) has no chat_state row — still report its gap.
        let row = match row {
            Some(row) => Some(row),
            None => conn
                .query_row(
                    "SELECT 0, MIN(seq) FROM memory_chat_gaps WHERE chat_id = ?1",
                    params![chat_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .optional()?,
        };
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

    /// All chat ids the indexer has seen (for catch-up scans) — including
    /// chats known only through gap rows.
    pub fn known_chats(&self) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT chat_id FROM memory_chat_state
             UNION
             SELECT chat_id FROM memory_chat_gaps",
        )?;
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
        drop(conn);
        self.drop_excluded_file_indexes(project_id, exclusions)?;
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
                .enumerate()
                .map(|(i, t)| {
                    // Last term becomes a prefix query so the FTS
                    // `prefix='2 3'` index is actually exercised
                    // ("src/mai"* → src/main.rs).
                    let quoted = format!("\"{}\"", t.replace('"', ""));
                    if i + 1 == terms.len() {
                        format!("{quoted}*")
                    } else {
                        quoted
                    }
                })
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
                 ORDER BY e.occurred_at_ms DESC, e.id DESC
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
            sql.push_str(" ORDER BY occurred_at_ms DESC, id DESC LIMIT ?");
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
             ORDER BY occurred_at_ms DESC, id DESC
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
                    COALESCE(SUM(kind = 'session_start'), 0) AS sessions,
                    COALESCE(SUM(kind = 'tool_call'), 0) AS tool_calls,
                    COALESCE(SUM(kind = 'tool_call' AND tool_name = 'read_file'), 0) AS file_reads,
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

        // Latest session_end outcome per agent — window function, not a
        // correlated subquery (O(events) once, and `id` breaks mtime ties:
        // replayed events share one timestamp).
        let mut stmt = conn.prepare(
            "SELECT agent, outcome FROM (
               SELECT agent, outcome,
                      ROW_NUMBER() OVER (
                        PARTITION BY agent
                        ORDER BY occurred_at_ms DESC, id DESC
                      ) AS rn
               FROM memory_events
               WHERE project_id = ?1 AND kind = 'session_end'
                 AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
             ) WHERE rn = 1",
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

        // Top modified paths PER AGENT (write-ish file ops only — reads are
        // counted, not listed; `tool_result` rows never carry paths here).
        // ROW_NUMBER partition keeps a high-volume agent from crowding the
        // others out of the briefing.
        let mut stmt = conn.prepare(
            "SELECT agent, relative_path, last_ms FROM (
               SELECT agent, relative_path, MAX(occurred_at_ms) AS last_ms,
                      ROW_NUMBER() OVER (
                        PARTITION BY agent
                        ORDER BY MAX(occurred_at_ms) DESC, relative_path
                      ) AS rn
               FROM memory_events
               WHERE project_id = ?1 AND kind = 'tool_call'
                 AND relative_path IS NOT NULL
                 AND tool_name IN ('write_file', 'edit_file', 'apply_patch')
                 AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
               GROUP BY agent, relative_path
             ) WHERE rn <= 8
             ORDER BY last_ms DESC, agent, relative_path",
        )?;
        let touches: Vec<(String, String, i64)> = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (agent, path, last_ms) in touches {
            if let Some(d) = agents.iter_mut().find(|a| a.agent == agent) {
                d.modified_paths.push(FileTouch { path, last_ms });
            }
        }

        // Recent failures PER AGENT — same window-function fairness as the
        // path query.
        let mut stmt = conn.prepare(
            "SELECT agent, kind, tool_name, relative_path, outcome, occurred_at_ms FROM (
               SELECT id, agent, kind, tool_name, relative_path, outcome, occurred_at_ms,
                      ROW_NUMBER() OVER (
                        PARTITION BY agent
                        ORDER BY occurred_at_ms DESC, id DESC
                      ) AS rn
               FROM memory_events
               WHERE project_id = ?1
                 AND ((kind = 'tool_result' AND outcome = 'error')
                   OR (kind = 'session_end' AND outcome IN ('errored', 'interrupted')))
                 AND (?2 IS NULL OR chat_id != ?2) AND occurred_at_ms >= ?3
             ) WHERE rn <= 5
             ORDER BY occurred_at_ms DESC, id DESC",
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
            if let Some(d) = agents.iter_mut().find(|a| a.agent == agent) {
                d.failures.push(FailureDigest { label, last_ms: ms });
            }
        }

        let mut stmt = conn.prepare(
            "SELECT kind, content, source, source_agent, source_path, confidence, stale_at_ms
             FROM memory_facts
             WHERE (project_id = ?1 OR scope = 'global')
               AND (scope != 'session' OR ?2 IS NULL OR chat_id != ?2)
               AND (stale_at_ms IS NULL OR stale_at_ms >= ?3)
             ORDER BY stale_at_ms IS NOT NULL, confidence DESC, valid_from_ms DESC, id DESC
             LIMIT 8",
        )?;
        let facts = stmt
            .query_map(params![project_id, exclude_chat_id, since_ms], |r| {
                Ok(FactDigest {
                    kind: r.get(0)?,
                    content: r.get(1)?,
                    source: r.get(2)?,
                    source_agent: r.get(3)?,
                    source_path: r.get(4)?,
                    confidence: r.get(5)?,
                    stale: r.get::<_, Option<i64>>(6)?.is_some(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(BriefingDigest {
            chats: chats.max(0) as u64,
            agents,
            facts,
        })
    }

    /// "Who touched this path?" — every recorded `tool_call` under
    /// `path_prefix` (project-relative), newest first. `tool_call` only:
    /// result rows would double-count each op and carry no extra path info.
    /// The range comparison (`>= prefix`, `< prefix || U+10FFFF`) is a real
    /// prefix match that can use `idx_memory_events_project_path` — LIKE
    /// can't (case-insensitive by default → full project scan).
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
             WHERE project_id = ?1 AND kind = 'tool_call'
               AND relative_path >= ?2
               AND relative_path < ?2 || char(1114111)
             ORDER BY occurred_at_ms DESC, id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![project_id, path_prefix, limit.clamp(1, 200) as i64],
            row_to_event,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Replace the parsed graph for one file when its content hash changes.
    /// The caller reads and parses files off the agent event path.
    pub fn index_file_symbols(
        &self,
        project_id: &str,
        path: &str,
        content_hash: &str,
        symbols: &[MemorySymbol],
    ) -> Result<bool, MemoryError> {
        validate_graph_path(path)?;
        if project_id.trim().is_empty()
            || content_hash.len() != 64
            || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(MemoryError::Other("invalid file index identity".into()));
        }
        let exclusions = self.exclusions(project_id)?;
        if policy::is_default_excluded(path) || policy::user_excluded(path, &exclusions) {
            return Err(MemoryError::Other(
                "file path is excluded from memory".into(),
            ));
        }
        let symbols: Vec<_> = symbols
            .iter()
            .filter(|symbol| {
                !symbol.name.trim().is_empty()
                    && symbol.name.chars().count() <= 256
                    && symbol.kind.chars().count() <= 100
                    && !policy::contains_secret_value(&symbol.name)
            })
            .take(1000)
            .collect();
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let old_hash: Option<String> = tx
            .query_row(
                "SELECT content_hash FROM memory_files
                 WHERE project_id = ?1 AND path = ?2",
                params![project_id, path],
                |row| row.get(0),
            )
            .optional()?;
        let changed = old_hash.as_deref() != Some(content_hash);
        let file_node = upsert_graph_node(&tx, project_id, "file", path, path)?;
        if changed {
            let prefix = format!("{path}::");
            let old_symbols: Vec<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM memory_graph_nodes
                     WHERE project_id = ?1 AND kind = 'symbol'
                       AND substr(node_key, 1, length(?2)) = ?2",
                )?;
                stmt.query_map(params![project_id, prefix], |row| row.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for id in old_symbols {
                tx.execute("DELETE FROM memory_graph_nodes WHERE id = ?1", params![id])?;
            }
            tx.execute(
                "INSERT INTO memory_files(project_id, path, content_hash, indexed_at_ms, node_id)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(project_id, path) DO UPDATE SET
                   content_hash = excluded.content_hash,
                   indexed_at_ms = excluded.indexed_at_ms,
                   node_id = excluded.node_id",
                params![project_id, path, content_hash, now_ms(), file_node],
            )?;
            for symbol in symbols {
                let key = format!(
                    "{path}::{}::{}::{}",
                    symbol.kind, symbol.name, symbol.start_byte
                );
                let symbol_node = upsert_graph_node(&tx, project_id, "symbol", &key, &symbol.name)?;
                insert_graph_edge(&tx, project_id, file_node, symbol_node, "contains", "", 0)?;
            }
            if old_hash.is_some() {
                tx.execute(
                    "UPDATE memory_facts
                     SET stale_at_ms = ?3
                     WHERE project_id = ?1 AND source_path = ?2 AND stale_at_ms IS NULL",
                    params![project_id, path, now_ms()],
                )?;
            }
        }
        connect_file_history(&tx, project_id, path, file_node)?;
        tx.execute(
            "DELETE FROM memory_graph_edges
             WHERE project_id = ?1 AND target_node = ?2
               AND source_node IN (
                 SELECT id FROM memory_graph_nodes
                 WHERE project_id = ?1 AND kind = 'fact'
               )",
            params![project_id, file_node],
        )?;
        connect_file_facts(&tx, project_id, path, file_node)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn graph_neighbors(
        &self,
        project_id: &str,
        path: &str,
        limit: usize,
    ) -> Result<Vec<GraphNeighbor>, MemoryError> {
        validate_graph_path(path)?;
        let exclusions = self.exclusions(project_id)?;
        if policy::is_default_excluded(path) || policy::user_excluded(path, &exclusions) {
            return Err(MemoryError::Other(
                "file path is excluded from memory".into(),
            ));
        }
        let conn = lock(&self.conn);
        let file_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM memory_graph_nodes
                 WHERE project_id = ?1 AND kind = 'file' AND node_key = ?2",
                params![project_id, path],
                |row| row.get(0),
            )
            .optional()?;
        let Some(file_id) = file_id else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT node.kind, node.node_key, node.label, edge.relation
             FROM memory_graph_edges edge
             JOIN memory_graph_nodes node
               ON node.id = CASE WHEN edge.source_node = ?3
                                 THEN edge.target_node ELSE edge.source_node END
             WHERE edge.project_id = ?1
               AND (edge.source_node = ?3 OR edge.target_node = ?3)
             ORDER BY edge.relation, node.kind, node.label
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![project_id, limit.clamp(1, 100) as i64, file_id],
            |row| {
                Ok(GraphNeighbor {
                    kind: row.get(0)?,
                    key: row.get(1)?,
                    label: row.get(2)?,
                    relation: row.get(3)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn file_indexes(&self, project_id: &str) -> Result<Vec<(String, String)>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT path, content_hash FROM memory_files
             WHERE project_id = ?1 ORDER BY path LIMIT 10000",
        )?;
        let rows = stmt.query_map(params![project_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn drop_excluded_file_indexes(
        &self,
        project_id: &str,
        exclusions: &[String],
    ) -> Result<usize, MemoryError> {
        let indexed = self.file_indexes(project_id)?;
        let excluded: Vec<_> = indexed
            .into_iter()
            .filter(|(path, _)| {
                policy::is_default_excluded(path) || policy::user_excluded(path, exclusions)
            })
            .map(|(path, _)| path)
            .collect();
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let mut removed = 0;
        for path in excluded {
            let file_id: Option<i64> = tx
                .query_row(
                    "SELECT id FROM memory_graph_nodes
                     WHERE project_id = ?1 AND kind = 'file' AND node_key = ?2",
                    params![project_id, path],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(file_id) = file_id {
                let prefix = format!("{path}::");
                tx.execute(
                    "DELETE FROM memory_graph_nodes
                     WHERE project_id = ?1 AND kind = 'symbol'
                       AND substr(node_key, 1, length(?2)) = ?2",
                    params![project_id, prefix],
                )?;
                tx.execute(
                    "DELETE FROM memory_graph_nodes
                     WHERE id = ?1 AND project_id = ?2",
                    params![file_id, project_id],
                )?;
                tx.execute(
                    "UPDATE memory_facts SET stale_at_ms = ?3
                     WHERE project_id = ?1 AND source_path = ?2
                       AND stale_at_ms IS NULL",
                    params![project_id, path, now_ms()],
                )?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn remove_file_index(&self, project_id: &str, path: &str) -> Result<bool, MemoryError> {
        validate_graph_path(path)?;
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let file_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM memory_graph_nodes
                 WHERE project_id = ?1 AND kind = 'file' AND node_key = ?2",
                params![project_id, path],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = file_id {
            let prefix = format!("{path}::");
            tx.execute(
                "DELETE FROM memory_graph_nodes
                 WHERE project_id = ?1 AND kind = 'symbol'
                   AND substr(node_key, 1, length(?2)) = ?2",
                params![project_id, prefix],
            )?;
            tx.execute("DELETE FROM memory_graph_nodes WHERE id = ?1", params![id])?;
            tx.execute(
                "UPDATE memory_facts SET stale_at_ms = ?3
                 WHERE project_id = ?1 AND source_path = ?2 AND stale_at_ms IS NULL",
                params![project_id, path, now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(file_id.is_some())
    }

    /// Store one explicit, typed fact. This API never distills transcript
    /// text; callers must provide a reviewed fact and provenance.
    pub fn create_fact(&self, fact: &NewMemoryFact) -> Result<i64, MemoryError> {
        self.validate_fact(fact)?;
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        insert_fact(&tx, fact)?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Return valid facts relevant to a project/session, including explicitly
    /// promoted global facts. Result counts are capped to keep MCP bounded.
    pub fn facts(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        include_stale: bool,
        limit: usize,
    ) -> Result<Vec<MemoryFact>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT id, project_id, chat_id, kind, scope, content, source,
                    source_agent, source_model, source_event_seq, source_path,
                    confidence, user_confirmed, valid_from_ms, stale_at_ms, replaced_by
             FROM memory_facts
             WHERE (project_id = ?1 OR scope = 'global')
               AND (?2 IS NULL OR scope != 'session' OR chat_id = ?2)
               AND (?3 OR stale_at_ms IS NULL)
             ORDER BY stale_at_ms IS NOT NULL, valid_from_ms DESC, id DESC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                project_id,
                chat_id,
                include_stale,
                limit.clamp(1, 200) as i64
            ],
            row_to_fact,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Bounded substring search over the project's valid facts.
    pub fn search_facts(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryFact>, MemoryError> {
        let needle = query.trim();
        if needle.is_empty() || needle.len() > 256 {
            return Ok(Vec::new());
        }
        let conn = lock(&self.conn);
        let mut sql = String::from(
            "SELECT id, project_id, chat_id, kind, scope, content, source,
                    source_agent, source_model, source_event_seq, source_path,
                    confidence, user_confirmed, valid_from_ms, stale_at_ms, replaced_by
             FROM memory_facts
             WHERE (project_id = ?1 OR scope = 'global')
               AND (?2 IS NULL OR scope != 'session' OR chat_id = ?2)
               AND stale_at_ms IS NULL",
        );
        if self.facts_fts {
            sql.push_str(
                " AND id IN (
                   SELECT rowid FROM memory_facts_fts WHERE memory_facts_fts MATCH ?3
                 )",
            );
        } else {
            sql.push_str(" AND instr(lower(content), lower(?3)) > 0");
        }
        sql.push_str(" ORDER BY valid_from_ms DESC, id DESC LIMIT ?4");
        let mut stmt = conn.prepare(&sql)?;
        let search_query = if self.facts_fts {
            needle
                .split_whitespace()
                .take(8)
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ")
        } else {
            needle.to_string()
        };
        let rows = stmt.query_map(
            params![
                project_id,
                chat_id,
                search_query,
                limit.clamp(1, 100) as i64
            ],
            row_to_fact,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Supersede one fact and insert its replacement in one transaction.
    pub fn replace_fact(
        &self,
        old_id: i64,
        replacement: &NewMemoryFact,
    ) -> Result<i64, MemoryError> {
        self.validate_fact(replacement)?;
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE memory_facts
             SET stale_at_ms = ?2
             WHERE id = ?1 AND stale_at_ms IS NULL",
            params![old_id, replacement.valid_from_ms],
        )?;
        if changed != 1 {
            return Err(MemoryError::Other("fact not found or already stale".into()));
        }
        insert_fact(&tx, replacement)?;
        let new_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE memory_facts SET replaced_by = ?2 WHERE id = ?1",
            params![old_id, new_id],
        )?;
        tx.commit()?;
        Ok(new_id)
    }

    /// Mark a fact as potentially stale without asserting a contradiction.
    pub fn mark_fact_stale(&self, id: i64, at_ms: i64) -> Result<bool, MemoryError> {
        let conn = lock(&self.write);
        let changed = conn.execute(
            "UPDATE memory_facts SET stale_at_ms = ?2
             WHERE id = ?1 AND stale_at_ms IS NULL",
            params![id, at_ms],
        )?;
        Ok(changed == 1)
    }

    /// Promote a fact only through a caller-controlled, explicit confirmation
    /// action. This does not make the original assertion more certain.
    pub fn promote_fact_to_global(&self, id: i64, confirmed: bool) -> Result<bool, MemoryError> {
        if !confirmed {
            return Err(MemoryError::Other(
                "global promotion requires explicit user confirmation".into(),
            ));
        }
        let conn = lock(&self.write);
        let changed = conn.execute(
            "UPDATE memory_facts SET scope = 'global', user_confirmed = 1
             WHERE id = ?1 AND stale_at_ms IS NULL",
            params![id],
        )?;
        Ok(changed == 1)
    }

    pub fn forget_fact(&self, id: i64) -> Result<bool, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute("DELETE FROM memory_facts WHERE id = ?1", params![id])?;
        if changed == 1 {
            tx.execute(
                "DELETE FROM memory_graph_nodes
                 WHERE kind = 'fact' AND node_key = ?1",
                params![id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    fn validate_fact(&self, fact: &NewMemoryFact) -> Result<(), MemoryError> {
        validate_fact_input(fact)?;
        if let Some(path) = fact.source_path.as_deref() {
            let exclusions = self.exclusions(&fact.project_id)?;
            if policy::is_default_excluded(path) || policy::user_excluded(path, &exclusions) {
                return Err(MemoryError::Other(
                    "fact source_path is excluded from memory".into(),
                ));
            }
        }
        Ok(())
    }

    /// Record/refresh a project's display root (first writer wins per row;
    /// later touches only fill a still-NULL label). Called once per chat by
    /// the indexer — never on the hot path.
    pub fn touch_project(&self, project_id: &str, root_path: &str) -> Result<(), MemoryError> {
        if project_id.is_empty() || root_path.is_empty() {
            return Ok(());
        }
        let conn = lock(&self.write);
        let now = now_ms();
        conn.execute(
            "INSERT INTO memory_projects(project_id, root_path, created_at_ms, updated_at_ms)
             VALUES(?1, ?2, ?3, ?3)
             ON CONFLICT(project_id) DO UPDATE SET
               root_path = COALESCE(memory_projects.root_path, excluded.root_path),
               updated_at_ms = excluded.updated_at_ms",
            params![project_id, root_path, now],
        )?;
        Ok(())
    }

    /// Every project the index knows, with its settings and footprint —
    /// the Settings → Memory project list. Projects seen only through
    /// events (no settings row yet) are included with defaults.
    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>, MemoryError> {
        let conn = lock(&self.conn);
        let mut projects: HashMap<String, ProjectInfo> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT project_id, root_path, enabled, exclusions
                 FROM memory_projects",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (id, root, enabled, raw) = row?;
                projects.insert(
                    id.clone(),
                    ProjectInfo {
                        project_id: id,
                        root_path: root,
                        enabled,
                        exclusions: serde_json::from_str(&raw).unwrap_or_default(),
                        events: 0,
                        last_activity_ms: None,
                    },
                );
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT project_id, COUNT(*), MAX(occurred_at_ms)
                 FROM memory_events GROUP BY project_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })?;
            for row in rows {
                let (id, events, last_ms) = row?;
                let entry = projects.entry(id.clone()).or_insert_with(|| ProjectInfo {
                    project_id: id,
                    root_path: None,
                    enabled: true,
                    exclusions: Vec::new(),
                    events: 0,
                    last_activity_ms: None,
                });
                entry.events = events;
                entry.last_activity_ms = last_ms;
            }
        }
        let mut out: Vec<ProjectInfo> = projects.into_values().collect();
        out.sort_by(|a, b| {
            b.last_activity_ms
                .unwrap_or(0)
                .cmp(&a.last_activity_ms.unwrap_or(0))
                .then_with(|| a.project_id.cmp(&b.project_id))
        });
        Ok(out)
    }

    /// Store-wide footprint for the settings header (events + chats across
    /// all projects; bytes come from the caller's dir metadata).
    pub fn totals(&self) -> Result<(i64, i64), MemoryError> {
        let conn = lock(&self.conn);
        let events: i64 = conn.query_row("SELECT COUNT(*) FROM memory_events", [], |r| r.get(0))?;
        let chats: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT chat_id) FROM memory_events",
            [],
            |r| r.get(0),
        )?;
        Ok((events, chats))
    }

    /// Chats with a persisted enqueue gap — the periodic catch-up tick only
    /// runs a journal scan when this is non-empty.
    pub fn chats_with_gaps(&self) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare("SELECT DISTINCT chat_id FROM memory_chat_gaps")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Chats known to belong to a project — used by `forget_project` to
    /// build per-chat replay horizons so catch-up cannot resurrect rows.
    pub fn project_chats(&self, project_id: &str) -> Result<Vec<String>, MemoryError> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT chat_id FROM memory_chat_state WHERE project_id = ?1
             UNION
             SELECT chat_id FROM memory_events WHERE project_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![project_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Clear persisted gap markers without touching the cursor — used when
    /// the journal backing the gap no longer exists (nothing to replay).
    pub fn clear_gap(&self, chat_id: &str) -> Result<(), MemoryError> {
        let conn = lock(&self.write);
        conn.execute(
            "DELETE FROM memory_chat_gaps WHERE chat_id = ?1",
            params![chat_id],
        )?;
        Ok(())
    }

    // ── forget ────────────────────────────────────────────────────────────

    /// Delete every retained record for one chat, leaving TWO durable
    /// markers so nothing can resurrect the deleted range:
    /// - a `memory_forgotten` tombstone at `through_seq` (the journal tail at
    ///   forget time, or "forever" when the journal is already gone): the
    ///   writer skips any record at or below it, queued or replayed;
    /// - the replay cursor advanced to `through_seq`, so catch-up resumes
    ///   AFTER the forget point instead of rebuilding the deleted range.
    ///
    /// `None` = journal unreadable/gone → the chat is being destroyed, so
    /// the tombstone covers every seq (no legitimate future event can
    /// arrive for a chat whose journal was discarded).
    pub fn forget_chat(
        &self,
        chat_id: &str,
        through_seq: Option<u64>,
    ) -> Result<usize, MemoryError> {
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
            "DELETE FROM memory_facts WHERE chat_id = ?1",
            params![chat_id],
        )?;
        tx.execute(
            "DELETE FROM memory_graph_edges WHERE source_chat_id = ?1",
            params![chat_id],
        )?;
        delete_orphan_fact_nodes(&tx)?;
        remove_orphan_activity_nodes(&tx, "")?;
        let through = through_seq.unwrap_or(u64::MAX).min(i64::MAX as u64) as i64;
        // Wildcard project_id '' = every project this chat ever touched.
        tx.execute(
            "INSERT INTO memory_forgotten(chat_id, project_id, through_seq)
             VALUES(?1, '', ?2)
             ON CONFLICT(chat_id, project_id) DO UPDATE SET
               through_seq = MAX(memory_forgotten.through_seq, excluded.through_seq)",
            params![chat_id, through],
        )?;
        if through_seq.is_some() {
            // Real journal tail: advance the replay cursor so catch-up
            // resumes after the forget point. `None` (journal already
            // gone) keeps the existing cursor — and never creates a state
            // row for a chat the index never saw; the tombstone alone
            // still blocks any queued/replayed record.
            tx.execute(
                "INSERT INTO memory_chat_state(chat_id, last_seq) VALUES(?1, ?2)
                 ON CONFLICT(chat_id) DO UPDATE SET
                   last_seq = MAX(memory_chat_state.last_seq, excluded.last_seq)",
                params![chat_id, through],
            )?;
        }
        tx.execute(
            "DELETE FROM memory_chat_gaps WHERE chat_id = ?1",
            params![chat_id],
        )?;
        tx.commit()?;
        // Sweep stragglers a straddling writer batch may have committed
        // after our delete (queue-ordered; see MemoryItem::PurgeChat).
        let _ = self.tx.try_send(MemoryItem::PurgeChat {
            chat_id: chat_id.to_string(),
        });
        Ok(removed)
    }

    /// Delete every retained record for a whole project. `horizons` maps
    /// each known chat to its journal tail (`u64::MAX` = journal gone, chat
    /// dead) so per-(chat, project) tombstones prevent catch-up AND queued
    /// records from resurrecting rows; project settings (enable flag,
    /// exclusions) are kept — forgetting activity is not a settings reset.
    pub fn forget_project(
        &self,
        project_id: &str,
        horizons: &[(String, u64)],
    ) -> Result<usize, MemoryError> {
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
            "DELETE FROM memory_facts WHERE project_id = ?1",
            params![project_id],
        )?;
        tx.execute(
            "DELETE FROM memory_graph_nodes WHERE project_id = ?1",
            params![project_id],
        )?;
        // Tombstones: jump every known chat's cursor past its journal tail
        // and record the forget horizon so the writer refuses any record at
        // or below it — queued, replayed, or straddling this transaction.
        for (chat_id, seq) in horizons {
            let through = (*seq).min(i64::MAX as u64) as i64;
            tx.execute(
                "INSERT INTO memory_forgotten(chat_id, project_id, through_seq)
                 VALUES(?1, ?2, ?3)
                 ON CONFLICT(chat_id, project_id) DO UPDATE SET
                   through_seq = MAX(memory_forgotten.through_seq, excluded.through_seq)",
                params![chat_id, project_id, through],
            )?;
            tx.execute(
                "INSERT INTO memory_chat_state(chat_id, project_id, last_seq)
                 VALUES(?1, ?2, ?3)
                 ON CONFLICT(chat_id) DO UPDATE SET
                   last_seq = MAX(memory_chat_state.last_seq, excluded.last_seq)",
                params![chat_id, project_id, through],
            )?;
            // Gap rows are NOT cleared here: they belong to the chat's whole
            // journal, not to this project. The project tombstone above
            // already suppresses this project's replayed rows; keeping the
            // gaps lets catch-up still recover events another project of the
            // same chat dropped. Only forget_chat / forget_all (whole-chat
            // wipes) clear them.
        }
        tx.commit()?;
        // Return freed pages to the OS incrementally (auto_vacuum=INCREMENTAL).
        let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
        let _ = self.tx.try_send(MemoryItem::PurgeProject {
            project_id: project_id.to_string(),
        });
        Ok(removed)
    }

    /// "Forget all": delete every retained record across every project and
    /// tombstone every chat in `horizons` at its journal tail so neither
    /// catch-up nor queued writes can rebuild them. Project settings rows
    /// are kept (enable flags, exclusions, roots are preferences, not
    /// activity data).
    pub fn forget_all(&self, horizons: &[(String, u64)]) -> Result<usize, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM memory_events")?;
            stmt.query_map([], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if self.fts {
            delete_fts_rows(&tx, &ids)?;
        }
        let removed = tx.execute("DELETE FROM memory_events", [])?;
        tx.execute("DELETE FROM memory_facts", [])?;
        tx.execute("DELETE FROM memory_graph_nodes", [])?;
        for (chat_id, seq) in horizons {
            let through = (*seq).min(i64::MAX as u64) as i64;
            tx.execute(
                "INSERT INTO memory_forgotten(chat_id, project_id, through_seq)
                 VALUES(?1, '', ?2)
                 ON CONFLICT(chat_id, project_id) DO UPDATE SET
                   through_seq = MAX(memory_forgotten.through_seq, excluded.through_seq)",
                params![chat_id, through],
            )?;
            tx.execute(
                "INSERT INTO memory_chat_state(chat_id, last_seq) VALUES(?1, ?2)
                 ON CONFLICT(chat_id) DO UPDATE SET
                   last_seq = MAX(memory_chat_state.last_seq, excluded.last_seq)",
                params![chat_id, through],
            )?;
        }
        tx.execute("DELETE FROM memory_chat_gaps", [])?;
        tx.commit()?;
        let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
        let _ = self.tx.try_send(MemoryItem::PurgeAll);
        Ok(removed)
    }

    /// Delete one retained activity event and any facts explicitly sourced
    /// from it, along with its derived graph edges.
    pub fn forget_event(&self, id: i64) -> Result<bool, MemoryError> {
        let conn = lock(&self.write);
        let tx = conn.unchecked_transaction()?;
        if self.fts {
            delete_fts_rows(&tx, &[id])?;
        }
        let source = tx
            .query_row(
                "SELECT project_id, chat_id, source_seq FROM memory_events WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let removed = tx.execute("DELETE FROM memory_events WHERE id = ?1", params![id])?;
        if let Some((project_id, chat_id, seq)) = source {
            tx.execute(
                "DELETE FROM memory_facts
                 WHERE project_id = ?1 AND chat_id = ?2 AND source_event_seq = ?3",
                params![project_id, chat_id, seq],
            )?;
            tx.execute(
                "DELETE FROM memory_graph_edges
                 WHERE project_id = ?1 AND source_chat_id = ?2 AND source_event_seq = ?3",
                params![project_id, chat_id, seq],
            )?;
            delete_orphan_fact_nodes(&tx)?;
            remove_orphan_activity_nodes(&tx, &project_id)?;
        }
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Retention: delete events older than `cutoff_ms` in bounded batches,
    /// releasing the write lock between them so live indexing is never
    /// starved for long. Returns the number of rows removed.
    pub fn prune_older_than(&self, cutoff_ms: i64) -> Result<usize, MemoryError> {
        const BATCH: i64 = 2000;
        let mut total = 0usize;
        loop {
            let conn = lock(&self.write);
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<(i64, String, String, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, project_id, chat_id, source_seq
                     FROM memory_events
                     WHERE occurred_at_ms < ?1
                     ORDER BY id LIMIT ?2",
                )?;
                stmt.query_map(params![cutoff_ms, BATCH], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
            };
            if rows.is_empty() {
                break;
            }
            if self.fts {
                let ids: Vec<_> = rows.iter().map(|(id, _, _, _)| *id).collect();
                delete_fts_rows(&tx, &ids)?;
            }
            {
                let mut del = tx.prepare("DELETE FROM memory_events WHERE id = ?1")?;
                for (id, project_id, chat_id, seq) in &rows {
                    del.execute(params![id])?;
                    tx.execute(
                        "DELETE FROM memory_graph_edges
                         WHERE project_id = ?1 AND source_chat_id = ?2
                           AND source_event_seq = ?3",
                        params![project_id, chat_id, seq],
                    )?;
                }
            }
            delete_orphan_fact_nodes(&tx)?;
            remove_orphan_activity_nodes(&tx, "")?;
            tx.commit()?;
            total += rows.len();
        }
        if total > 0 {
            let conn = lock(&self.write);
            let _ = conn.execute_batch("PRAGMA incremental_vacuum;");
        }
        Ok(total)
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

/// One row of the Settings → Memory project list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub project_id: String,
    /// Human label (worktree/repo root or cwd) — `None` until the indexer
    /// records it via [`MemoryService::touch_project`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
    pub enabled: bool,
    pub exclusions: Vec<String>,
    pub events: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
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

fn row_to_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryFact> {
    let kind: String = r.get(3)?;
    let scope: String = r.get(4)?;
    let source: String = r.get(6)?;
    Ok(MemoryFact {
        id: r.get(0)?,
        project_id: r.get(1)?,
        chat_id: r.get(2)?,
        kind: parse_fact_enum(&kind, 3, "fact kind", FactKind::parse(&kind))?,
        scope: parse_fact_enum(&scope, 4, "fact scope", FactScope::parse(&scope))?,
        content: r.get(5)?,
        source: parse_fact_enum(&source, 6, "fact source", FactSource::parse(&source))?,
        source_agent: r.get(7)?,
        source_model: r.get(8)?,
        source_event_seq: r.get::<_, Option<i64>>(9)?.map(|seq| seq.max(0) as u64),
        source_path: r.get(10)?,
        confidence: r.get(11)?,
        user_confirmed: r.get(12)?,
        valid_from_ms: r.get(13)?,
        stale_at_ms: r.get(14)?,
        replaced_by: r.get(15)?,
    })
}

fn parse_fact_enum<T>(
    value: &str,
    index: usize,
    name: &str,
    parsed: Option<T>,
) -> rusqlite::Result<T> {
    parsed.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!(
                "unknown {name} value: {value}"
            ))),
        )
    })
}

fn validate_fact_input(fact: &NewMemoryFact) -> Result<(), MemoryError> {
    fact.validate().map_err(|e| MemoryError::Other(e.into()))?;
    if policy::contains_secret_value(&fact.content)
        || fact
            .source_path
            .as_deref()
            .is_some_and(policy::contains_secret_value)
        || fact
            .source_agent
            .as_deref()
            .is_some_and(policy::contains_secret_value)
        || fact
            .source_model
            .as_deref()
            .is_some_and(policy::contains_secret_value)
    {
        return Err(MemoryError::Other(
            "fact contains credential-shaped content".into(),
        ));
    }
    if fact
        .source_path
        .as_deref()
        .is_some_and(|path| Path::new(path).is_absolute())
    {
        return Err(MemoryError::Other(
            "fact source_path must be project-relative".into(),
        ));
    }
    Ok(())
}

fn insert_fact(conn: &Connection, fact: &NewMemoryFact) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO memory_facts(
           project_id, chat_id, kind, scope, content, source, source_agent,
           source_model, source_event_seq, source_path, confidence,
           user_confirmed, valid_from_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            fact.project_id,
            fact.chat_id,
            fact.kind.as_str(),
            fact.scope.as_str(),
            fact.content.trim(),
            fact.source.as_str(),
            fact.source_agent,
            fact.source_model,
            fact.source_event_seq.map(|seq| seq as i64),
            fact.source_path,
            fact.confidence,
            fact.user_confirmed,
            fact.valid_from_ms
        ],
    )?;
    Ok(())
}

fn validate_graph_path(path: &str) -> Result<(), MemoryError> {
    let path_buf = Path::new(path);
    if path.trim().is_empty()
        || path_buf.is_absolute()
        || path_buf.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
        || policy::contains_secret_value(path)
    {
        return Err(MemoryError::Other(
            "graph path must be a safe project-relative path".into(),
        ));
    }
    Ok(())
}

fn upsert_graph_node(
    conn: &Connection,
    project_id: &str,
    kind: &str,
    key: &str,
    label: &str,
) -> Result<i64, MemoryError> {
    Ok(conn.query_row(
        "INSERT INTO memory_graph_nodes(project_id, kind, node_key, label)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(project_id, kind, node_key) DO UPDATE SET label = excluded.label
         RETURNING id",
        params![project_id, kind, key, label],
        |row| row.get(0),
    )?)
}

fn insert_graph_edge(
    conn: &Connection,
    project_id: &str,
    source_node: i64,
    target_node: i64,
    relation: &str,
    source_chat_id: &str,
    source_event_seq: u64,
) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_graph_edges(
           project_id, source_node, target_node, relation,
           source_chat_id, source_event_seq
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            project_id,
            source_node,
            target_node,
            relation,
            source_chat_id,
            source_event_seq.min(i64::MAX as u64) as i64
        ],
    )?;
    Ok(())
}

fn connect_file_history(
    conn: &Connection,
    project_id: &str,
    path: &str,
    file_node: i64,
) -> Result<(), MemoryError> {
    let events: Vec<(String, String, u64)> = {
        let mut stmt = conn.prepare(
            "SELECT chat_id, agent, source_seq
             FROM memory_events
             WHERE project_id = ?1 AND relative_path = ?2 AND kind = 'tool_call'
               AND tool_name IN ('write_file', 'edit_file', 'apply_patch')
             ORDER BY occurred_at_ms DESC, id DESC
             LIMIT 1000",
        )?;
        stmt.query_map(params![project_id, path], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, i64>(2)?.max(0) as u64,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    for (chat_id, agent, seq) in events {
        let session_node = upsert_graph_node(conn, project_id, "session", &chat_id, &chat_id)?;
        let agent_node = upsert_graph_node(conn, project_id, "agent", &agent, &agent)?;
        insert_graph_edge(
            conn,
            project_id,
            session_node,
            file_node,
            "touched",
            &chat_id,
            seq,
        )?;
        insert_graph_edge(
            conn, project_id, agent_node, file_node, "modifies", &chat_id, seq,
        )?;
    }
    Ok(())
}

fn connect_file_facts(
    conn: &Connection,
    project_id: &str,
    path: &str,
    file_node: i64,
) -> Result<(), MemoryError> {
    let facts: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, kind FROM memory_facts
             WHERE project_id = ?1 AND source_path = ?2 AND stale_at_ms IS NULL",
        )?;
        stmt.query_map(params![project_id, path], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    for (id, kind) in facts {
        let label = format!("{kind} #{id}");
        let fact_node = upsert_graph_node(conn, project_id, "fact", &id.to_string(), &label)?;
        let relation = match kind.as_str() {
            "decision" => "decided",
            "bug" | "failed_attempt" => "explains",
            "convention" | "state" | "todo" => "related_to",
            _ => "related_to",
        };
        insert_graph_edge(conn, project_id, fact_node, file_node, relation, "", 0)?;
    }
    Ok(())
}

fn remove_orphan_activity_nodes(conn: &Connection, project_id: &str) -> Result<(), MemoryError> {
    conn.execute(
        "DELETE FROM memory_graph_nodes
         WHERE (?1 = '' OR project_id = ?1) AND kind IN ('session', 'agent')
           AND NOT EXISTS (
             SELECT 1 FROM memory_graph_edges edge
             WHERE edge.source_node = memory_graph_nodes.id
                OR edge.target_node = memory_graph_nodes.id
           )",
        params![project_id],
    )?;
    Ok(())
}

fn delete_orphan_fact_nodes(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute(
        "DELETE FROM memory_graph_nodes
         WHERE kind = 'fact'
           AND NOT EXISTS (
             SELECT 1 FROM memory_facts fact
             WHERE fact.project_id = memory_graph_nodes.project_id
               AND CAST(fact.id AS TEXT) = memory_graph_nodes.node_key
           )",
        [],
    )?;
    Ok(())
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
    if version < 2 {
        // v1 → v2: project identity moved from canonical cwd to the git
        // common dir, so every v1 `project_id` is an unreachable orphan.
        // Phase 1 has not shipped — wipe rather than migrate.
        conn.execute_batch(
            "DELETE FROM memory_events;
             DELETE FROM memory_chat_state;
             DELETE FROM memory_projects;",
        )?;
        if fts_table_exists(conn)? {
            // External-content FTS: 'rebuild' re-indexes from the (now
            // empty) content table, clearing stale rows.
            conn.execute_batch("INSERT INTO memory_fts(memory_fts) VALUES('rebuild');")?;
        }
        conn.execute_batch("PRAGMA user_version = 2;")?;
    }
    if version < 3 {
        // v3: human-readable project root for the settings UI. The project
        // id is a hash; without this the per-project toggles would be
        // unlabelled. Guarded so a hand-built v1/v2 schema still migrates.
        let has_root: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memory_projects')
             WHERE name = 'root_path'",
            [],
            |r| r.get(0),
        )?;
        if has_root == 0 {
            conn.execute_batch("ALTER TABLE memory_projects ADD COLUMN root_path TEXT")?;
        }
        conn.execute_batch("PRAGMA user_version = 3;")?;
    }
    if version < 4 {
        // v4: two durability fixes for gap/forget handling.
        //
        // `memory_chat_gaps` replaces the single `gap_seq` column: every
        // dropped seq is its own row, so a drop that lands DURING a catch-up
        // replay is never collapsed into (and then cleared with) the gap the
        // replay was healing. `Resume` deletes the rows it covered.
        //
        // `memory_forgotten` is the anti-resurrection guard: forget_* writes
        // a per-(chat, project) horizon and the writer skips any record at
        // or below it — including records still sitting in the queue or
        // replayed by an in-flight catch-up.
        conn.execute_batch(
            "CREATE TABLE memory_chat_gaps (
               chat_id TEXT NOT NULL,
               seq INTEGER NOT NULL,
               PRIMARY KEY(chat_id, seq)
             );
             INSERT INTO memory_chat_gaps(chat_id, seq)
               SELECT chat_id, gap_seq FROM memory_chat_state
               WHERE gap_seq IS NOT NULL;

             CREATE TABLE memory_forgotten (
               chat_id TEXT NOT NULL,
               project_id TEXT NOT NULL DEFAULT '',
               through_seq INTEGER NOT NULL,
               PRIMARY KEY(chat_id, project_id)
             );
             PRAGMA user_version = 4;",
        )?;
    }
    if version < 5 {
        conn.execute_batch(
            "CREATE TABLE memory_facts (
               id INTEGER PRIMARY KEY,
               project_id TEXT NOT NULL,
               chat_id TEXT,
               kind TEXT NOT NULL CHECK(kind IN (
                 'decision', 'bug', 'convention', 'state', 'todo', 'failed_attempt'
               )),
               scope TEXT NOT NULL CHECK(scope IN ('session', 'project', 'global')),
               content TEXT NOT NULL,
               source TEXT NOT NULL CHECK(source IN (
                 'agent_assertion', 'observed', 'test', 'user_confirmed'
               )),
               source_agent TEXT,
               source_model TEXT,
               source_event_seq INTEGER,
               source_path TEXT,
               confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
               user_confirmed INTEGER NOT NULL DEFAULT 0,
               valid_from_ms INTEGER NOT NULL,
               stale_at_ms INTEGER,
               replaced_by INTEGER REFERENCES memory_facts(id) ON DELETE SET NULL,
               CHECK(scope != 'session' OR chat_id IS NOT NULL),
               CHECK(scope != 'global' OR user_confirmed = 1)
             );
             CREATE INDEX idx_memory_facts_project_scope_validity
               ON memory_facts(project_id, scope, stale_at_ms, valid_from_ms);
             CREATE INDEX idx_memory_facts_chat_validity
               ON memory_facts(chat_id, stale_at_ms, valid_from_ms);
             CREATE INDEX idx_memory_facts_source
               ON memory_facts(project_id, chat_id, source_event_seq);
             PRAGMA user_version = 5;",
        )?;
    }
    if version < 6 {
        conn.execute_batch(
            "CREATE TABLE memory_graph_nodes (
               id INTEGER PRIMARY KEY,
               project_id TEXT NOT NULL,
               kind TEXT NOT NULL CHECK(kind IN (
                 'file', 'symbol', 'session', 'agent', 'fact', 'issue'
               )),
               node_key TEXT NOT NULL,
               label TEXT NOT NULL,
               UNIQUE(project_id, kind, node_key)
             );
             CREATE TABLE memory_graph_edges (
               id INTEGER PRIMARY KEY,
               project_id TEXT NOT NULL,
               source_node INTEGER NOT NULL REFERENCES memory_graph_nodes(id) ON DELETE CASCADE,
               target_node INTEGER NOT NULL REFERENCES memory_graph_nodes(id) ON DELETE CASCADE,
               relation TEXT NOT NULL CHECK(relation IN (
                 'contains', 'modifies', 'touched', 'decided', 'contradicts',
                 'replaces', 'depends_on', 'explains', 'related_to'
               )),
               source_chat_id TEXT NOT NULL DEFAULT '',
               source_event_seq INTEGER NOT NULL DEFAULT 0,
               UNIQUE(project_id, source_node, target_node, relation,
                      source_chat_id, source_event_seq)
             );
             CREATE INDEX idx_memory_graph_edges_source
               ON memory_graph_edges(project_id, source_node, relation);
             CREATE INDEX idx_memory_graph_edges_target
               ON memory_graph_edges(project_id, target_node, relation);
             CREATE INDEX idx_memory_graph_edges_chat
               ON memory_graph_edges(project_id, source_chat_id, source_event_seq);
             CREATE TABLE memory_files (
               project_id TEXT NOT NULL,
               path TEXT NOT NULL,
               content_hash TEXT NOT NULL,
               indexed_at_ms INTEGER NOT NULL,
               node_id INTEGER NOT NULL UNIQUE
                 REFERENCES memory_graph_nodes(id) ON DELETE CASCADE,
               PRIMARY KEY(project_id, path)
             );
             PRAGMA user_version = 6;",
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

fn ensure_facts_fts(conn: &Connection) -> Result<bool, MemoryError> {
    let existed = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memory_facts_fts')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false);
    let mut setup = String::from(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_facts_fts USING fts5(
           content, content='memory_facts', content_rowid='id'
         );
         CREATE TRIGGER IF NOT EXISTS memory_facts_ai AFTER INSERT ON memory_facts BEGIN
           INSERT INTO memory_facts_fts(rowid, content) VALUES(new.id, new.content);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_facts_ad AFTER DELETE ON memory_facts BEGIN
           INSERT INTO memory_facts_fts(memory_facts_fts, rowid, content)
             VALUES('delete', old.id, old.content);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_facts_au AFTER UPDATE OF content ON memory_facts BEGIN
           INSERT INTO memory_facts_fts(memory_facts_fts, rowid, content)
             VALUES('delete', old.id, old.content);
           INSERT INTO memory_facts_fts(rowid, content) VALUES(new.id, new.content);
         END;",
    );
    if !existed {
        setup.push_str("\nINSERT INTO memory_facts_fts(memory_facts_fts) VALUES('rebuild');");
    }
    let result = conn.execute_batch(&setup);
    match result {
        Ok(()) => Ok(true),
        Err(error) => {
            tracing::warn!(error = %error, "memory: facts FTS5 unavailable, using substring search");
            Ok(false)
        }
    }
}

// ── writer ────────────────────────────────────────────────────────────────

fn writer_loop(conn: Connection, rx: Receiver<MemoryItem>, fts: bool) {
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
                    flush(&conn, &mut batch, fts);
                    return;
                }
            }
        }
        flush(&conn, &mut batch, fts);
    }
    while let Ok(item) = rx.try_recv() {
        batch.push(item);
    }
    flush(&conn, &mut batch, fts);
}

fn flush(conn: &Connection, batch: &mut Vec<MemoryItem>, fts: bool) {
    if batch.is_empty() {
        return;
    }
    let items = std::mem::take(batch);
    if let Err(err) = apply_batch(conn, &items, fts) {
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
        // Purge items carry no journal seq — a failed batch already rolled
        // back, so there is nothing for them to sweep.
        _ => None,
    }
}

fn note_gap_on(conn: &Connection, chat_id: &str, seq: u64) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_chat_gaps(chat_id, seq) VALUES(?1, ?2)",
        params![chat_id, seq as i64],
    )?;
    Ok(())
}

fn apply_batch(conn: &Connection, items: &[MemoryItem], fts: bool) -> Result<(), MemoryError> {
    let tx = conn.unchecked_transaction()?;
    // Per-batch settings cache: project enabled flag + exclusions.
    let mut settings: HashMap<String, (bool, Vec<String>)> = HashMap::new();
    let mut changed_paths = std::collections::HashSet::new();
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
    // `Resume` heals through `seq`: every persisted gap row at or below it
    // is covered by the replay and removed. Rows ABOVE it — drops that
    // landed while the replay ran — survive untouched.
    let mut gap_clear =
        tx.prepare("DELETE FROM memory_chat_gaps WHERE chat_id = ?1 AND seq <= ?2")?;
    // Forget horizons, loaded lazily per chat: (project scope, through_seq).
    // `project_id = ''` is the chat-wide wildcard written by forget_chat /
    // forget_all; a project-scoped row only covers its own project.
    let mut forgotten: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    let mut forgotten_stmt =
        tx.prepare("SELECT project_id, through_seq FROM memory_forgotten WHERE chat_id = ?1")?;

    for item in items {
        match item {
            MemoryItem::Record(rec) => {
                if forgotten_covers(
                    &mut forgotten_stmt,
                    &mut forgotten,
                    &rec.chat_id,
                    &rec.project_id,
                    rec.source_seq,
                )? {
                    // At or below a forget horizon: the row was deleted on
                    // purpose. Still advance the cursor — the event WAS
                    // processed, it just must not be stored.
                    cursor.execute(params![rec.chat_id, rec.project_id, rec.source_seq as i64])?;
                    continue;
                }
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
                    if rec.kind == "tool_call"
                        && matches!(
                            rec.tool_name.as_deref(),
                            Some("write_file" | "edit_file" | "apply_patch")
                        )
                        && let Some(path) = rel_path
                    {
                        changed_paths.insert((rec.project_id.clone(), path.to_string()));
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
            // Purges are scoped by the persisted forget horizons, NOT by
            // chat/project alone: a record legitimately committed ABOVE a
            // horizon (queued before the purge, seq > through_seq) must
            // survive the sweep. The EXISTS mirrors `forgotten_covers`.
            MemoryItem::PurgeChat { chat_id } => {
                purge_events(
                    &tx,
                    fts,
                    &format!("chat_id = ?1 AND {TOMBSTONE_COVERED}"),
                    params![chat_id],
                )?;
            }
            MemoryItem::PurgeProject { project_id } => {
                purge_events(
                    &tx,
                    fts,
                    &format!("project_id = ?1 AND {TOMBSTONE_COVERED}"),
                    params![project_id],
                )?;
            }
            MemoryItem::PurgeAll => {
                purge_events(&tx, fts, TOMBSTONE_COVERED, params![])?;
            }
        }
    }
    drop(insert);
    drop(fts_insert);
    drop(cursor);
    drop(gap_clear);
    drop(forgotten_stmt);
    for (project_id, path) in changed_paths {
        let file_node: Option<i64> = tx
            .query_row(
                "SELECT id FROM memory_graph_nodes
                 WHERE project_id = ?1 AND kind = 'file' AND node_key = ?2",
                params![project_id, path],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(file_node) = file_node {
            connect_file_history(&tx, &project_id, &path, file_node)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// SQL fragment: the row is covered by a `memory_forgotten` horizon —
/// chat-wide (`project_id = ''`) or scoped to the row's own project, at or
/// below `through_seq`. Used by the `Purge*` sweeps so they delete exactly
/// what the tombstones forbid, never legitimate post-horizon records.
const TOMBSTONE_COVERED: &str = "EXISTS (
    SELECT 1 FROM memory_forgotten f
    WHERE f.chat_id = memory_events.chat_id
      AND (f.project_id = '' OR f.project_id = memory_events.project_id)
      AND memory_events.source_seq <= f.through_seq
)";

/// Delete matching `memory_events` rows (and their FTS entries) inside the
/// batch transaction — shared by the `Purge*` queue items.
fn purge_events(
    tx: &Connection,
    fts: bool,
    where_clause: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<(), MemoryError> {
    let ids: Vec<i64> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id FROM memory_events WHERE {where_clause}"
        ))?;
        stmt.query_map(params, |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if fts {
        delete_fts_rows(tx, &ids)?;
    }
    if !ids.is_empty() {
        let mut del = tx.prepare(&format!("DELETE FROM memory_events WHERE {where_clause}"))?;
        del.execute(params)?;
    }
    Ok(())
}

/// Is `source_seq` covered by a forget horizon for this (chat, project)?
/// `project_id = ''` tombstones are chat-wide; a project-scoped tombstone
/// only covers records carrying that same project id. Horizons load once
/// per chat per batch — forgets are rare, batches are small.
fn forgotten_covers(
    stmt: &mut rusqlite::Statement<'_>,
    cache: &mut HashMap<String, Vec<(String, i64)>>,
    chat_id: &str,
    project_id: &str,
    source_seq: u64,
) -> Result<bool, MemoryError> {
    if !cache.contains_key(chat_id) {
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![chat_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        cache.insert(chat_id.to_string(), rows);
    }
    let seq = source_seq.min(i64::MAX as u64) as i64;
    Ok(cache
        .get(chat_id)
        .into_iter()
        .flatten()
        .any(|(scope, through)| (*scope == project_id || scope.is_empty()) && seq <= *through))
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

/// Copy the store files from `src_dir` to `dst_dir`. Runs before the first
/// open, so there is no live database to corrupt; idempotent (a no-op when
/// the destination already has a database or the source has none). The
/// source is left in place: it is derived data and a free rollback if the
/// new location misbehaves.
///
/// Staged: every file is copied into a scratch dir first and published by
/// rename, `memory.sqlite` LAST — its presence is what marks the
/// destination "already relocated", so an interrupted copy leaves a
/// retryable state instead of a db without its WAL.
pub fn relocate_store(src_dir: &Path, dst_dir: &Path) -> std::io::Result<()> {
    let src_db = src_dir.join(DB_FILE);
    if !src_db.exists() || dst_dir.join(DB_FILE).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst_dir)?;
    let stage = dst_dir.join(".komet-relocate");
    if stage.exists() {
        std::fs::remove_dir_all(&stage)?;
    }
    std::fs::create_dir_all(&stage)?;
    let publish = (|| -> std::io::Result<()> {
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("{DB_FILE}{suffix}");
            let src = src_dir.join(&name);
            if src.exists() {
                std::fs::copy(&src, stage.join(&name))?;
            }
        }
        // WAL/SHM first, the main file last: until `memory.sqlite` lands the
        // destination still looks un-relocated, so a crash here just retries.
        for name in [
            format!("{DB_FILE}-wal"),
            format!("{DB_FILE}-shm"),
            DB_FILE.to_string(),
        ] {
            let staged = stage.join(&name);
            if staged.exists() {
                std::fs::rename(&staged, dst_dir.join(&name))?;
            } else {
                // No staged file: drop any orphan left by an earlier attempt
                // so a stale WAL can't pair with the fresh database.
                let _ = std::fs::remove_file(dst_dir.join(&name));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&stage);
    publish
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{FactKind, FactScope, FactSource, NewMemoryFact};
    use crate::graph::MemorySymbol;
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

    fn fact(content: &str) -> NewMemoryFact {
        NewMemoryFact {
            project_id: "p_test".into(),
            chat_id: None,
            kind: FactKind::Decision,
            scope: FactScope::Project,
            content: content.into(),
            source: FactSource::AgentAssertion,
            source_agent: Some("claude-code".into()),
            source_model: Some("opus".into()),
            source_event_seq: Some(3),
            source_path: Some("src/main.rs".into()),
            confidence: 0.6,
            user_confirmed: false,
            valid_from_ms: now_ms(),
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
    fn facts_are_typed_scoped_searchable_and_have_provenance() {
        let (svc, _dir) = svc();
        let id = svc
            .create_fact(&fact("Use SQLite as the canonical memory store"))
            .unwrap();
        let found = svc.facts("p_test", None, false, 20).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert_eq!(found[0].kind, FactKind::Decision);
        assert_eq!(found[0].scope, FactScope::Project);
        assert_eq!(found[0].source, FactSource::AgentAssertion);
        assert_eq!(found[0].source_event_seq, Some(3));
        assert_eq!(
            svc.search_facts("p_test", None, "canonical memory", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            svc.facts("another-project", None, false, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn replacing_facts_is_atomic_and_keeps_stale_provenance() {
        let (svc, _dir) = svc();
        let old_id = svc
            .create_fact(&fact("Use the first storage design"))
            .unwrap();
        let mut replacement = fact("Use the revised storage design");
        replacement.valid_from_ms += 1;
        let new_id = svc.replace_fact(old_id, &replacement).unwrap();
        let all = svc.facts("p_test", None, true, 20).unwrap();
        let old = all.iter().find(|f| f.id == old_id).unwrap();
        assert_eq!(old.stale_at_ms, Some(replacement.valid_from_ms));
        assert_eq!(old.replaced_by, Some(new_id));
        assert_eq!(svc.facts("p_test", None, false, 20).unwrap().len(), 1);
    }

    #[test]
    fn global_facts_require_confirmation_and_follow_project_forget() {
        let (svc, _dir) = svc();
        let id = svc.create_fact(&fact("Keep this convention")).unwrap();
        assert!(svc.promote_fact_to_global(id, false).is_err());
        assert!(svc.promote_fact_to_global(id, true).unwrap());
        assert_eq!(
            svc.facts("other-project", None, false, 20).unwrap().len(),
            1
        );
        svc.forget_project("p_test", &[]).unwrap();
        assert!(
            svc.facts("other-project", None, true, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fact_validation_rejects_secrets_and_unconfirmed_global_scope() {
        let (svc, _dir) = svc();
        let mut secret = fact("Never store ghp_123456789012345678901234567890");
        assert!(svc.create_fact(&secret).is_err());
        secret = fact("A global assertion");
        secret.scope = FactScope::Global;
        assert!(svc.create_fact(&secret).is_err());
        secret.scope = FactScope::Session;
        secret.chat_id = None;
        assert!(svc.create_fact(&secret).is_err());
    }

    #[test]
    fn file_graph_updates_only_on_hash_change_and_marks_linked_facts_stale() {
        let (svc, _dir) = svc();
        let mut linked_fact = fact("The parser keeps its input bounded");
        linked_fact.source_path = Some("src/parser.rs".into());
        let fact_id = svc.create_fact(&linked_fact).unwrap();
        let symbols = vec![MemorySymbol {
            name: "Parser".into(),
            kind: "struct_item".into(),
            start_byte: 0,
            end_byte: 40,
        }];
        assert!(
            svc.index_file_symbols("p_test", "src/parser.rs", &"a".repeat(64), &symbols)
                .unwrap()
        );
        let neighbors = svc.graph_neighbors("p_test", "src/parser.rs", 20).unwrap();
        assert!(
            neighbors
                .iter()
                .any(|node| node.kind == "symbol" && node.label == "Parser")
        );
        assert!(
            neighbors
                .iter()
                .any(|node| node.kind == "fact" && node.key == fact_id.to_string())
        );
        assert!(
            !svc.index_file_symbols("p_test", "src/parser.rs", &"a".repeat(64), &symbols)
                .unwrap()
        );

        let changed_symbols = vec![MemorySymbol {
            name: "parse".into(),
            kind: "function_item".into(),
            start_byte: 50,
            end_byte: 90,
        }];
        assert!(
            svc.index_file_symbols("p_test", "src/parser.rs", &"b".repeat(64), &changed_symbols)
                .unwrap()
        );
        let facts = svc.facts("p_test", None, true, 10).unwrap();
        assert!(facts[0].stale_at_ms.is_some());
        let neighbors = svc.graph_neighbors("p_test", "src/parser.rs", 20).unwrap();
        assert!(!neighbors.iter().any(|node| node.label == "Parser"));
        assert!(neighbors.iter().any(|node| node.label == "parse"));

        assert!(svc.remove_file_index("p_test", "src/parser.rs").unwrap());
        assert!(
            svc.graph_neighbors("p_test", "src/parser.rs", 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn graph_index_obeys_exclusions_and_validates_paths_and_hashes() {
        let (svc, _dir) = svc();
        svc.set_exclusions("p_test", &["vendor".into()]).unwrap();
        let symbols = [];
        assert!(
            svc.index_file_symbols("p_test", "vendor/lib.rs", &"a".repeat(64), &symbols)
                .is_err()
        );
        assert!(
            svc.index_file_symbols("p_test", "../secret.rs", &"a".repeat(64), &symbols)
                .is_err()
        );
        assert!(
            svc.index_file_symbols("p_test", "src/lib.rs", "not-a-hash", &symbols)
                .is_err()
        );
    }

    #[test]
    fn obsidian_export_is_one_way_linked_and_refuses_unowned_destinations() {
        let (svc, dir) = svc();
        let mut linked = fact("Use [[SQLite]] as canonical storage");
        linked.chat_id = Some("chat-1".into());
        linked.scope = FactScope::Session;
        svc.create_fact(&linked).unwrap();
        let mut event = rec(7, "tool_call");
        event.tool_name = Some("edit_file".into());
        event.relative_path = Some("src/store.rs".into());
        svc.enqueue(MemoryItem::Record(Box::new(event))).unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);

        let destination = dir.path().join("vault");
        let report = svc.export_obsidian("p_test", &destination).unwrap();
        assert_eq!(report.facts, 1);
        assert_eq!(report.sessions, 1);
        let fact_note = std::fs::read_to_string(destination.join("Facts/decision-1.md")).unwrap();
        assert!(fact_note.contains("userConfirmed: false"));
        assert!(fact_note.contains("\\[\\[SQLite\\]\\]"));
        let session_path = std::fs::read_dir(destination.join("Sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let session = std::fs::read_to_string(session_path).unwrap();
        assert!(session.contains("[[../Facts/decision-1]]"));
        assert!(session.contains("[[src/store.rs]]"));
        assert_eq!(
            svc.export_obsidian("p_test", &destination).unwrap().facts,
            1
        );

        let unowned = dir.path().join("existing-vault");
        std::fs::create_dir(&unowned).unwrap();
        std::fs::write(unowned.join("keep.md"), "user content").unwrap();
        assert!(svc.export_obsidian("p_test", &unowned).is_err());
        assert_eq!(
            std::fs::read_to_string(unowned.join("keep.md")).unwrap(),
            "user content"
        );
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
    fn forget_chat_leaves_tombstone_cursor() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        // Journal tail is seq 7: the tombstone must land there so a replay
        // from the cursor cannot resurrect the deleted range.
        assert_eq!(svc.forget_chat("chat-1", Some(7)).unwrap(), 1);
        assert_eq!(svc.stats("p_test").unwrap().events, 0);
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.last_seq, 7);
        assert_eq!(cursor.gap_seq, None);
        // FTS row must be gone too (external-content delete path).
        assert!(
            svc.search("p_test", "session_start", 10)
                .unwrap()
                .is_empty()
        );
        // Re-enqueueing an OLD seq directly is still possible at this layer
        // (the engine never does it — replay starts at last_seq), but the
        // tombstone blocks nothing new: seq 8 indexes normally.
        svc.enqueue(MemoryItem::Record(Box::new(rec(8, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
    }

    #[test]
    fn forget_project_keeps_settings_and_tombstones_chats() {
        let (svc, _dir) = svc();
        svc.set_enabled("p_test", false).unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.chat_cursor("chat-1").unwrap().last_seq == 1);
        svc.forget_project("p_test", &[("chat-1".into(), 9)])
            .unwrap();
        // Forgetting activity is not a settings reset.
        assert!(!svc.is_enabled("p_test").unwrap());
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.last_seq, 9);
        assert_eq!(cursor.gap_seq, None);
    }

    #[test]
    fn briefing_digest_is_fair_across_agents() {
        let (svc, _dir) = svc();
        // Agent A floods 300 modified paths; agent B touches 2. The global
        // LIMIT used to starve B entirely — per-agent windows must not.
        let now = now_ms();
        for seq in 1..=300u64 {
            let mut r = rec(seq, "tool_call");
            r.agent = "agent-a".into();
            r.tool_name = Some("edit_file".into());
            r.relative_path = Some(format!("src/a{seq}.rs"));
            r.occurred_at_ms = now;
            svc.enqueue(MemoryItem::Record(Box::new(r))).unwrap();
        }
        for (i, seq) in [301u64, 302].iter().enumerate() {
            let mut r = rec(*seq, "tool_call");
            r.agent = "agent-b".into();
            r.tool_name = Some("write_file".into());
            r.relative_path = Some(format!("src/b{}.rs", i + 1));
            r.occurred_at_ms = now;
            svc.enqueue(MemoryItem::Record(Box::new(r))).unwrap();
        }
        // B also gets a session_end so it has an agent row in the digest.
        let mut end = rec(303, "session_end");
        end.agent = "agent-b".into();
        end.outcome = Some("completed".into());
        end.occurred_at_ms = now;
        svc.enqueue(MemoryItem::Record(Box::new(end))).unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 303);

        let digest = svc.briefing_digest("p_test", None, now - 1000).unwrap();
        let b = digest
            .agents
            .iter()
            .find(|a| a.agent == "agent-b")
            .expect("agent-b must appear despite agent-a's volume");
        assert_eq!(b.modified_paths.len(), 2);
        let a = digest.agents.iter().find(|a| a.agent == "agent-a").unwrap();
        assert_eq!(a.modified_paths.len(), 8, "per-agent cap is 8");
    }

    #[test]
    fn auto_vacuum_is_incremental_on_fresh_db() {
        let (svc, _dir) = svc();
        let conn = lock(&svc.conn);
        let mode: i64 = conn
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 2, "INCREMENTAL must be set before tables exist");
    }

    #[test]
    fn touched_uses_prefix_range_and_lists_calls_once() {
        let (svc, _dir) = svc();
        let mut call = rec(1, "tool_call");
        call.tool_name = Some("edit_file".into());
        call.relative_path = Some("src/main.rs".into());
        let mut result = rec(2, "tool_result");
        result.tool_name = Some("edit_file".into());
        result.relative_path = Some("src/main.rs".into());
        result.outcome = Some("ok".into());
        let mut other = rec(3, "tool_call");
        other.tool_name = Some("read_file".into());
        other.relative_path = Some("src/other.rs".into());
        for r in [call, result, other] {
            svc.enqueue(MemoryItem::Record(Box::new(r))).unwrap();
        }
        wait_for(|| svc.stats("p_test").unwrap().events == 3);
        let hits = svc.touched("p_test", "src/mai", 10).unwrap();
        // Prefix match on "src/mai" hits "src/main.rs"; the tool_result row
        // is filtered out (kind='tool_call') so each op appears once.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].relative_path.as_deref(), Some("src/main.rs"));
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

    #[test]
    fn prune_removes_old_events_and_their_fts_rows() {
        let (svc, _dir) = svc();
        let now = now_ms();
        let old = now - 100 * 24 * 60 * 60 * 1000;
        for seq in 1..=3u64 {
            let mut r = rec(seq, "tool_call");
            r.tool_name = Some("edit_file".into());
            r.relative_path = Some(format!("old/file{seq}.rs"));
            r.occurred_at_ms = old;
            svc.enqueue(MemoryItem::Record(Box::new(r))).unwrap();
        }
        let mut fresh = rec(4, "tool_call");
        fresh.tool_name = Some("edit_file".into());
        fresh.relative_path = Some("new/keep.rs".into());
        svc.enqueue(MemoryItem::Record(Box::new(fresh))).unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 4);

        let removed = svc.prune_older_than(now - RETENTION_MS).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(svc.stats("p_test").unwrap().events, 1);
        assert!(svc.search("p_test", "old/file1.rs", 10).unwrap().is_empty());
        assert_eq!(svc.search("p_test", "keep.rs", 10).unwrap().len(), 1);
        // Cursor untouched: pruned rows are not re-created by a replay.
        assert_eq!(svc.chat_cursor("chat-1").unwrap().last_seq, 4);
    }

    #[test]
    fn forgetting_an_unknown_chat_creates_no_state_row() {
        let (svc, _dir) = svc();
        assert_eq!(svc.forget_chat("ghost", None).unwrap(), 0);
        assert!(!svc.known_chats().unwrap().contains(&"ghost".to_string()));
    }

    #[test]
    fn projects_list_totals_and_touch_project() {
        let (svc, _dir) = svc();
        svc.touch_project("p_test", "/repo/root").unwrap();
        svc.set_exclusions("p_test", &["target/**".into(), "*.log".into()])
            .unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);

        let projects = svc.list_projects().unwrap();
        assert_eq!(projects.len(), 1);
        let project = &projects[0];
        assert_eq!(project.project_id, "p_test");
        assert_eq!(project.root_path.as_deref(), Some("/repo/root"));
        assert!(project.enabled);
        assert_eq!(project.exclusions, vec!["target/**", "*.log"]);
        assert_eq!(project.events, 1);
        assert!(project.last_activity_ms.is_some());

        let (events, chats) = svc.totals().unwrap();
        assert_eq!(events, 1);
        assert_eq!(chats, 1);
    }

    #[test]
    fn forget_all_wipes_events_and_tombstones_chats() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        svc.enqueue(MemoryItem::Checkpoint {
            chat_id: "chat-1".into(),
            seq: 7,
        })
        .unwrap();
        wait_for(|| svc.totals().unwrap().0 == 1);

        // Horizon = journal tail (7): replay from 7 must not resurrect rows.
        let removed = svc.forget_all(&[("chat-1".to_string(), 7)]).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(svc.totals().unwrap().0, 0);
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.last_seq, 7);
        assert_eq!(cursor.gap_seq, None);
        // Project settings survive a wipe.
        assert!(svc.is_enabled("p_test").unwrap());
    }

    #[test]
    fn relocate_copies_db_and_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let src_dir = src.path().join("memory");
        let dst_dir = dst.path().join("memory");
        let svc = MemoryService::open(&src_dir).unwrap();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        drop(svc);

        relocate_store(&src_dir, &dst_dir).unwrap();
        assert!(dst_dir.join("memory.sqlite").exists());
        // Second run is a no-op: the destination already has a database.
        relocate_store(&src_dir, &dst_dir).unwrap();
        let reopened = MemoryService::open(&dst_dir).unwrap();
        assert_eq!(reopened.stats("p_test").unwrap().events, 1);
    }

    #[test]
    fn relocate_without_source_is_a_noop() {
        let dst = tempfile::tempdir().unwrap();
        let missing = dst.path().join("missing");
        relocate_store(&missing, dst.path()).unwrap();
        assert!(!dst.path().join("memory.sqlite").exists());
    }

    #[test]
    fn forget_tombstone_blocks_late_and_replayed_records() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        // Journal tail = 10: anything at or below it must never reappear —
        // queued writes and catch-up replays alike.
        svc.forget_chat("chat-1", Some(10)).unwrap();
        assert_eq!(svc.stats("p_test").unwrap().events, 0);

        for seq in [7u64, 10, 12] {
            svc.enqueue(MemoryItem::Record(Box::new(rec(seq, "session_start"))))
                .unwrap();
        }
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        let events = svc.recent("p_test", 10, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_seq, 12, "only seq > horizon may index");
    }

    #[test]
    fn project_scoped_tombstone_does_not_block_other_projects() {
        let (svc, _dir) = svc();
        svc.forget_project("p_test", &[("chat-1".to_string(), 10)])
            .unwrap();
        // Same chat, same seq, DIFFERENT project: the horizon is scoped.
        let mut other = rec(5, "session_start");
        other.project_id = "p_other".into();
        svc.enqueue(MemoryItem::Record(Box::new(other))).unwrap();
        // Same project, seq above the horizon: allowed.
        svc.enqueue(MemoryItem::Record(Box::new(rec(11, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        assert_eq!(svc.stats("p_other").unwrap().events, 1);
    }

    #[test]
    fn gaps_survive_a_resume_beyond_the_replay_snapshot() {
        let (svc, _dir) = svc();
        svc.note_gap("chat-1", 10).unwrap();
        svc.note_gap("chat-1", 25).unwrap();
        // Healed through seq 20: gap 10 is covered, gap 25 is NOT — the
        // single-column gap_seq used to collapse 25 into 10 and lose it.
        svc.enqueue(MemoryItem::Resume {
            chat_id: "chat-1".into(),
            seq: 20,
        })
        .unwrap();
        wait_for(|| svc.chat_cursor("chat-1").unwrap().last_seq == 20);
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.gap_seq, Some(25));
        assert!(
            svc.chats_with_gaps()
                .unwrap()
                .contains(&"chat-1".to_string())
        );
        // A second Resume covering 25 clears it.
        svc.enqueue(MemoryItem::Resume {
            chat_id: "chat-1".into(),
            seq: 30,
        })
        .unwrap();
        wait_for(|| svc.chat_cursor("chat-1").unwrap().gap_seq.is_none());
    }

    #[test]
    fn gap_only_chat_is_known_and_curable() {
        let (svc, _dir) = svc();
        // Every record dropped before the first flush: only gap rows exist.
        svc.note_gap("chat-1", 4).unwrap();
        assert!(svc.known_chats().unwrap().contains(&"chat-1".to_string()));
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.last_seq, 0);
        assert_eq!(cursor.gap_seq, Some(4));
        svc.clear_gap("chat-1").unwrap();
        assert_eq!(svc.chat_cursor("chat-1").unwrap().gap_seq, None);
    }

    #[test]
    fn purge_sweep_preserves_post_horizon_records() {
        let (svc, _dir) = svc();
        svc.enqueue(MemoryItem::Record(Box::new(rec(1, "session_start"))))
            .unwrap();
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        // Forget at journal tail 10: forget_chat enqueues the PurgeChat
        // barrier itself. Anything at/below 10 is tombstoned; seq 12 is a
        // legitimate post-forget record and must survive the sweep.
        svc.forget_chat("chat-1", Some(10)).unwrap();
        for seq in [5u64, 12] {
            svc.enqueue(MemoryItem::Record(Box::new(rec(seq, "session_start"))))
                .unwrap();
        }
        wait_for(|| svc.stats("p_test").unwrap().events == 1);
        let events = svc.recent("p_test", 10, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_seq, 12, "post-horizon record was purged");
    }

    #[test]
    fn project_forget_keeps_chat_gaps_for_other_projects() {
        let (svc, _dir) = svc();
        // A gap belongs to the chat's journal, not to one project: dropping
        // it on forget_project would lose replay coverage for events another
        // project of the same chat missed.
        svc.note_gap("chat-1", 8).unwrap();
        svc.forget_project("p_test", &[("chat-1".to_string(), 10)])
            .unwrap();
        let cursor = svc.chat_cursor("chat-1").unwrap();
        assert_eq!(cursor.gap_seq, Some(8), "project forget must keep gaps");
        // Whole-chat forget still clears them.
        svc.forget_chat("chat-1", Some(10)).unwrap();
        assert_eq!(svc.chat_cursor("chat-1").unwrap().gap_seq, None);
    }
}
