//! Constants carried over from komet `packages/session-doc/src/constants.ts`.
//! Per the original design these are starting points — re-measure with real heavy sessions.

/// Max bytes for a single message entry before continuation splitting.
pub const MSG_INLINE_MAX: usize = 256 * 1024;
/// History retention window for shallow-snapshot trimming (days). Dropped
/// 30 → 3 with the edge copy (2026-08-04): a month of op history per doc
/// blew the shared loro-wasm heap on the edge isolate before any trim ran.
pub const RETAIN_DAYS: u32 = 3;
/// Session DO folds its update log into the snapshot at this size (lossless).
pub const COMPACT_LOG_BYTES: usize = 8 * 1024 * 1024;
/// Soft ceiling for total doc size before aggressive trim.
pub const SOFT_CEILING_BYTES: usize = 25 * 1024 * 1024;
/// Host commits streamed assistant segments into the doc at this cadence (ms).
pub const STREAM_COMMIT_MS: u64 = 120;
/// Session DO batches update-log flushes at this cadence (ms).
pub const DO_FLUSH_MS: u64 = 5_000;
/// Byte budget for the in-memory doc LRU on device backends.
pub const DOC_LRU_BYTE_BUDGET: usize = 80 * 1024 * 1024;
/// Number of trailing messages materialized into the tail sidecar.
pub const TAIL_MESSAGE_COUNT: usize = 64;
/// Terminal output batching cadence (ms).
pub const TERMINAL_OUTPUT_BATCH_MS: u64 = 12;
/// Default TTL for durable commands.
pub const COMMAND_DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Current session doc schema version (`meta.schemaVersion`).
pub const SESSION_SCHEMA_VERSION: u32 = 1;
/// Byte cap for a compaction summary carried INSIDE the doc — reached only
/// through the opt-in [`CompactionSummaryPolicy::SummaryInDoc`]. Truncation
/// happens on a UTF-8 boundary and `MessagePart::Compaction::summary_bytes`
/// keeps the ORIGINAL size, so the UI can say how much it dropped.
pub const COMPACTION_SUMMARY_MAX_BYTES: usize = 8 * 1024;

/// Where the TEXT of a compaction summary lives. The default keeps the doc
/// (and therefore every sync peer, every export, every retained snapshot) free
/// of a blob the agent produced for itself: the metadata lands in the doc, the
/// text stays in the host's local run journal and is fetched on demand.
///
/// A summary is model prose about the whole conversation — it can carry
/// whatever the transcript contained, so shipping it to every device by default
/// is not a transparency win, it is an unbounded copy of the session in
/// everyone's oplog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionSummaryPolicy {
    /// Metadata only in the doc (`summary_bytes`, `summary_in_journal`); text
    /// through `GetCompactionSummary` from the host's run journal.
    #[default]
    JournalOnly,
    /// Text in the doc too, capped at [`COMPACTION_SUMMARY_MAX_BYTES`].
    SummaryInDoc,
}

impl CompactionSummaryPolicy {
    /// Whether summary TEXT may be written into a doc part.
    pub fn summary_in_doc(self) -> bool {
        matches!(self, Self::SummaryInDoc)
    }
}
