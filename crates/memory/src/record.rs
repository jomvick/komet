//! Row and queue types crossing the engine → store boundary.
//!
//! [`MemoryRecord`] is the allow-listed projection of one journaled
//! [`komet_proto::AgentEvent`]: metadata only — never prompts, message text,
//! tool arguments/results, command strings, diffs, or file contents. The
//! engine builds these; this crate stores them.

use serde::Serialize;

/// One allow-listed activity row, ready for `INSERT OR IGNORE`.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    pub project_id: String,
    pub chat_id: String,
    /// Journal seq of the source event — with `project_id`/`chat_id` it is the
    /// idempotency key (`UNIQUE(project_id, chat_id, source_seq)`).
    pub source_seq: u64,
    pub agent: String,
    pub model: Option<String>,
    /// `session_start` | `session_end` | `tool_call` | `tool_result` | `usage`.
    pub kind: &'static str,
    pub occurred_at_ms: i64,
    pub tool_name: Option<String>,
    /// Project-relative, exclusion-filtered path. `None` when the tool call
    /// carried no recognizable file path or the path failed policy.
    pub relative_path: Option<String>,
    /// `completed`/`interrupted`/`errored` on session ends, `ok`/`error` on
    /// tool results; `None` elsewhere.
    pub outcome: Option<String>,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// A unit of work for the single background writer.
#[derive(Debug)]
pub enum MemoryItem {
    /// Insert one allow-listed event row (idempotent on conflict). Boxed:
    /// records are wide and most queue traffic is the small variants.
    Record(Box<MemoryRecord>),
    /// "Every journaled seq ≤ `seq` for this chat has been processed" —
    /// advances the replay cursor past stretches that produce no rows
    /// (text deltas, ephemeral frames). Sent at run boundaries.
    Checkpoint { chat_id: String, seq: u64 },
    /// Catch-up finished replaying through `seq`: like [`Self::Checkpoint`]
    /// but also clears persisted gap markers at or below `seq`.
    Resume { chat_id: String, seq: u64 },
    /// Post-forget sweep, enqueued right after a `forget_*` commit: deletes
    /// any rows a straddling writer batch may have committed after the
    /// forget's own delete. Queue-ordered, so it runs after every record
    /// that was already in flight; later records are stopped by the
    /// `memory_forgotten` tombstone guard instead.
    PurgeChat { chat_id: String },
    /// Same sweep scoped to one project.
    PurgeProject { project_id: String },
    /// Same sweep across every project.
    PurgeAll,
}

/// A stored activity row, as read back for search/inspection/briefing.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryEvent {
    pub id: i64,
    pub project_id: String,
    pub chat_id: String,
    pub source_seq: u64,
    pub agent: String,
    pub model: Option<String>,
    pub kind: String,
    pub occurred_at_ms: i64,
    pub tool_name: Option<String>,
    pub relative_path: Option<String>,
    pub outcome: Option<String>,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// Per-chat replay cursor row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatCursor {
    /// Highest journal seq fully processed (records + checkpoints + resume).
    pub last_seq: u64,
    /// Lowest seq known to have been dropped at enqueue time (`None` = no
    /// known gap). Catch-up replays from `gap - 1` to heal it.
    pub gap_seq: Option<u64>,
}

// ── briefing digest (SQL aggregates → deterministic handoff text) ────────

/// One file an agent modified, most recent first (cap applied upstream).
#[derive(Debug, Clone, PartialEq)]
pub struct FileTouch {
    pub path: String,
    pub last_ms: i64,
}

/// One recent failure attributed to an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureDigest {
    /// e.g. "exec on build.sh" or "run errored".
    pub label: String,
    pub last_ms: i64,
}

/// A bounded, source-linked fact shown in a handoff briefing.
#[derive(Debug, Clone, PartialEq)]
pub struct FactDigest {
    pub kind: String,
    pub content: String,
    pub source: String,
    pub source_agent: Option<String>,
    pub source_path: Option<String>,
    pub confidence: f64,
    pub stale: bool,
}

/// Per-agent rollup for the briefing: sessions, volume, last outcome,
/// modified paths and recent failures.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDigest {
    pub agent: String,
    pub sessions: u32,
    pub tool_calls: u32,
    pub file_reads: u32,
    /// Outcome of the agent's most recent `session_end` in the window.
    pub last_status: Option<String>,
    pub last_activity_ms: i64,
    pub modified_paths: Vec<FileTouch>,
    pub failures: Vec<FailureDigest>,
}

/// Everything the briefing builder needs — produced by GROUP BY queries so
/// one high-volume agent cannot crowd the others out of a row cap.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefingDigest {
    pub chats: u64,
    pub agents: Vec<AgentDigest>,
    pub facts: Vec<FactDigest>,
}
