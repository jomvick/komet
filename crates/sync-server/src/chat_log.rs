//! chat2 log storage — the dumb-relay data model of the removed
//! `edge/src/chat-log.ts`, over one sqlite file per chat room.
//!
//! The room is an append-only log of opaque Loro update blobs plus one
//! client-written checkpoint blob. Nothing here interprets the bytes; CRDT
//! semantics live in the clients, cold start is a table read.
//!
//! One [`ChatLog`] owns one [`rusqlite::Connection`] behind a mutex: a push
//! appends the row and advances `meta.headSeq` in one transaction, so a crash
//! can never leave the sequence and the rows disagreeing.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension as _, params};

/// Per-row byte cap (the DO's `MAX_ROW_BYTES`). Post-strip updates are
/// KB-scale; a full checkpoint travels over HTTP, never as a row.
pub const MAX_ROW_BYTES: usize = 1024 * 1024;

/// Blob names for the checkpoint payload and its frontier (the DO's names).
pub const CHECKPOINT_BLOB: &str = "checkpoint";
pub const FRONTIER_BLOB: &str = "checkpoint-frontier";

/// One stored row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub seq: u64,
    pub device: String,
    pub batch_id: String,
    pub bytes: Vec<u8>,
}

/// The hello/stats surface (headSeq, floor, growth). `checkpointAt` is stored
/// but not surfaced — the client's hello shape has no field for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogStats {
    pub head_seq: u64,
    pub seq_floor: u64,
    pub row_count: u64,
    pub row_bytes: u64,
    pub checkpoint_seq: u64,
    pub checkpoint_size: u64,
}

/// Outcome of [`ChatLog::append`]. `batch_id UNIQUE` dedupes reconnect
/// re-pushes: a replay returns the ORIGINAL seq and appends nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended { seq: u64, dup: bool },
    Empty,
    TooLarge,
}

/// Outcome of [`ChatLog::commit_checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointOutcome {
    Committed { seq_floor: u64, pruned: u64 },
    Empty,
    FloorRegression,
    AheadOfHead,
}

pub struct ChatLog {
    db: Mutex<Connection>,
}

impl ChatLog {
    /// Open (or create) a chat room's tables.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let db = Connection::open(path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS rows (
                 seq INTEGER PRIMARY KEY,
                 device TEXT NOT NULL,
                 batch_id TEXT NOT NULL UNIQUE,
                 bytes BLOB NOT NULL,
                 received_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS blobs (
                 name TEXT PRIMARY KEY,
                 bytes BLOB NOT NULL
             );",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Append one opaque update. The insert and the `headSeq` bump share one
    /// transaction, so a concurrent reader never sees a hole in the log.
    pub fn append(
        &self,
        device: &str,
        batch_id: &str,
        bytes: &[u8],
        received_at: i64,
    ) -> rusqlite::Result<AppendOutcome> {
        if bytes.is_empty() {
            return Ok(AppendOutcome::Empty);
        }
        if bytes.len() > MAX_ROW_BYTES {
            return Ok(AppendOutcome::TooLarge);
        }
        let mut conn = self.connection();
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                "SELECT seq FROM rows WHERE batch_id = ?1",
                [batch_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            tx.commit()?;
            return Ok(AppendOutcome::Appended {
                seq: seq as u64,
                dup: true,
            });
        }
        let seq = meta_u64(&tx, "headSeq")? + 1;
        tx.execute(
            "INSERT INTO rows (seq, device, batch_id, bytes, received_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seq as i64, device, batch_id, bytes, received_at],
        )?;
        set_meta(&tx, "headSeq", &seq.to_string())?;
        tx.commit()?;
        Ok(AppendOutcome::Appended { seq, dup: false })
    }

    /// Rows `seq > after`, ascending, optionally excluding one device's own
    /// writes (the reconnect path that already holds them).
    pub fn rows_after(&self, after: u64, exclude: Option<&str>) -> rusqlite::Result<Vec<LogRow>> {
        let conn = self.connection();
        let (sql, excluded) = match exclude {
            Some(device) => (
                "SELECT seq, device, batch_id, bytes FROM rows \
                 WHERE seq > ?1 AND device != ?2 ORDER BY seq",
                Some(device),
            ),
            None => (
                "SELECT seq, device, batch_id, bytes FROM rows WHERE seq > ?1 ORDER BY seq",
                None,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<LogRow> {
            Ok(LogRow {
                seq: row.get::<_, i64>(0)? as u64,
                device: row.get(1)?,
                batch_id: row.get(2)?,
                bytes: row.get(3)?,
            })
        };
        let rows = match excluded {
            Some(device) => stmt
                .query_map(params![after as i64, device], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![after as i64], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// The hello surface: sequence, floor, live row aggregate and checkpoint
    /// metadata.
    pub fn stats(&self) -> rusqlite::Result<LogStats> {
        let conn = self.connection();
        let (row_count, row_bytes) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(bytes)), 0) FROM rows",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        Ok(LogStats {
            head_seq: meta_u64(&conn, "headSeq")?,
            seq_floor: meta_u64(&conn, "seqFloor")?,
            row_count,
            row_bytes,
            checkpoint_seq: meta_u64(&conn, "checkpointSeq")?,
            checkpoint_size: meta_u64(&conn, "checkpointSize")?,
        })
    }

    /// Commit a client-built checkpoint covering rows `seq <= seq_covered`.
    ///
    /// The guard is floor-monotonic (the dumb replacement for the s2
    /// vector-clock guard): the floor only moves forward and never past the
    /// head. Storing the blobs, pruning covered rows and moving the metadata
    /// all happen in one transaction.
    pub fn commit_checkpoint(
        &self,
        seq_covered: u64,
        frontier: &[u8],
        bytes: &[u8],
        committed_at: i64,
    ) -> rusqlite::Result<CheckpointOutcome> {
        if bytes.is_empty() {
            return Ok(CheckpointOutcome::Empty);
        }
        let mut conn = self.connection();
        let tx = conn.transaction()?;
        let floor = meta_u64(&tx, "seqFloor")?;
        let head = meta_u64(&tx, "headSeq")?;
        if seq_covered < floor {
            return Ok(CheckpointOutcome::FloorRegression);
        }
        if seq_covered > head {
            return Ok(CheckpointOutcome::AheadOfHead);
        }
        put_blob(&tx, CHECKPOINT_BLOB, bytes)?;
        put_blob(&tx, FRONTIER_BLOB, frontier)?;
        let before = row_count(&tx)?;
        tx.execute("DELETE FROM rows WHERE seq <= ?1", [seq_covered as i64])?;
        let after = row_count(&tx)?;
        set_meta(&tx, "seqFloor", &seq_covered.to_string())?;
        set_meta(&tx, "checkpointSeq", &seq_covered.to_string())?;
        set_meta(&tx, "checkpointSize", &bytes.len().to_string())?;
        set_meta(&tx, "checkpointAt", &committed_at.to_string())?;
        tx.commit()?;
        Ok(CheckpointOutcome::Committed {
            seq_floor: seq_covered,
            pruned: before - after,
        })
    }

    /// Store a named blob (checkpoint sidecars). Replaces any previous value.
    pub fn put_blob(&self, name: &str, bytes: &[u8]) -> rusqlite::Result<()> {
        put_blob(&self.connection(), name, bytes)
    }

    /// Read a named blob; `None` when it was never written.
    pub fn get_blob(&self, name: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        self.connection()
            .query_row("SELECT bytes FROM blobs WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        meta(&self.connection(), key)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        set_meta(&self.connection(), key, value)
    }
}

fn meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn meta_u64(conn: &Connection, key: &str) -> rusqlite::Result<u64> {
    Ok(meta(conn, key)?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

fn put_blob(conn: &Connection, name: &str, bytes: &[u8]) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO blobs (name, bytes) VALUES (?1, ?2) \
         ON CONFLICT(name) DO UPDATE SET bytes = excluded.bytes",
        params![name, bytes],
    )?;
    Ok(())
}

fn row_count(conn: &Connection) -> rusqlite::Result<u64> {
    conn.query_row("SELECT COUNT(*) FROM rows", [], |row| {
        row.get::<_, i64>(0).map(|count| count as u64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, ChatLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = ChatLog::open(&dir.path().join("chat.db")).unwrap();
        (dir, log)
    }

    #[test]
    fn seq_is_monotonic_from_head_seq_and_batches_dedupe() {
        let (_dir, log) = open();
        assert_eq!(
            log.append("dev-a", "batch-1", b"one", 1).unwrap(),
            AppendOutcome::Appended { seq: 1, dup: false }
        );
        assert_eq!(
            log.append("dev-b", "batch-2", b"two", 2).unwrap(),
            AppendOutcome::Appended { seq: 2, dup: false }
        );
        // A reconnect re-push acks the ORIGINAL seq and appends nothing.
        assert_eq!(
            log.append("dev-b", "batch-1", b"two", 3).unwrap(),
            AppendOutcome::Appended { seq: 1, dup: true }
        );
        assert_eq!(log.stats().unwrap().head_seq, 2);
        assert_eq!(log.stats().unwrap().row_count, 2);
    }

    #[test]
    fn append_rejects_empty_and_oversized_rows() {
        let (_dir, log) = open();
        assert_eq!(
            log.append("dev", "empty", b"", 1).unwrap(),
            AppendOutcome::Empty
        );
        assert_eq!(
            log.append("dev", "fat", &vec![0u8; MAX_ROW_BYTES + 1], 2)
                .unwrap(),
            AppendOutcome::TooLarge
        );
        assert_eq!(log.stats().unwrap().head_seq, 0);
    }

    #[test]
    fn rows_after_is_ordered_and_excludes_one_device() {
        let (_dir, log) = open();
        for (device, batch) in [("a", "b1"), ("b", "b2"), ("a", "b3")] {
            log.append(device, batch, batch.as_bytes(), 0).unwrap();
        }
        let all: Vec<u64> = log
            .rows_after(0, None)
            .unwrap()
            .iter()
            .map(|row| row.seq)
            .collect();
        assert_eq!(all, vec![1, 2, 3]);
        let foreign: Vec<(u64, String)> = log
            .rows_after(0, Some("a"))
            .unwrap()
            .into_iter()
            .map(|row| (row.seq, row.device))
            .collect();
        assert_eq!(foreign, vec![(2, "b".to_string())]);
        assert!(log.rows_after(3, None).unwrap().is_empty());
    }

    #[test]
    fn checkpoint_commit_prunes_rows_and_moves_the_floor() {
        let (_dir, log) = open();
        for seq in 1..=3 {
            log.append("dev", &format!("b{seq}"), b"x", 0).unwrap();
        }
        assert_eq!(
            log.commit_checkpoint(2, b"frontier", b"snapshot", 9)
                .unwrap(),
            CheckpointOutcome::Committed {
                seq_floor: 2,
                pruned: 2
            }
        );
        let stats = log.stats().unwrap();
        assert_eq!(stats.head_seq, 3);
        assert_eq!(stats.seq_floor, 2);
        assert_eq!(stats.row_count, 1);
        assert_eq!(stats.checkpoint_seq, 2);
        assert_eq!(stats.checkpoint_size, 8);
        assert_eq!(
            log.get_blob(CHECKPOINT_BLOB).unwrap().as_deref(),
            Some(&b"snapshot"[..])
        );
        assert_eq!(
            log.get_blob(FRONTIER_BLOB).unwrap().as_deref(),
            Some(&b"frontier"[..])
        );
        // The floor only moves forward.
        assert_eq!(
            log.commit_checkpoint(1, b"f", b"s", 10).unwrap(),
            CheckpointOutcome::FloorRegression
        );
        // ...and never past the head.
        assert_eq!(
            log.commit_checkpoint(4, b"f", b"s", 10).unwrap(),
            CheckpointOutcome::AheadOfHead
        );
        assert_eq!(
            log.commit_checkpoint(3, b"f", b"", 10).unwrap(),
            CheckpointOutcome::Empty
        );
        assert_eq!(
            log.commit_checkpoint(3, b"f", b"final", 10).unwrap(),
            CheckpointOutcome::Committed {
                seq_floor: 3,
                pruned: 1
            }
        );
    }

    #[test]
    fn stats_track_the_row_aggregate() {
        let (_dir, log) = open();
        assert_eq!(log.stats().unwrap(), LogStats::default());
        log.append("dev", "b1", b"12345", 0).unwrap();
        log.append("dev", "b2", b"67", 0).unwrap();
        let stats = log.stats().unwrap();
        assert_eq!(stats.row_count, 2);
        assert_eq!(stats.row_bytes, 7);
    }
}
