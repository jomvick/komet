//! Registry room protocol (`/registry/{org}/ws`).
//!
//! The registry is the sidebar's authority (which chats, spaces and devices exist),
//! so unlike the chat rooms — which relay opaque Loro frames — it needs a stateful
//! peer: `hello` → `state`, `push` → merge each op and `ack`, `presence`, `probe`.
//! Semantics mirror the Durable Object this server replaced (and the Rust mock in
//! `komet-sync/src/registry/mock_server.rs`); the difference is that rows and `seq`
//! are persisted, so a restart neither resets the sequence nor loses the sidebar.

use std::collections::HashMap;
use std::path::Path;

use komet_doc::{RegistryRow, RowOp, apply_op};
use rusqlite::Connection;
use serde_json::{Value, json};

/// Everything one `/registry/{org}/ws` room knows.
pub struct RegistryRoom {
    seq: u64,
    /// Rows older than this are only sent on a full sync. Never advanced today
    /// (registries are small); the client protocol already carries it.
    gc_floor: u64,
    presence: HashMap<String, i64>,
    rows: HashMap<(String, String), RegistryRow>,
    db: Connection,
}

impl RegistryRoom {
    /// Open (or create) the room's tables in the server's room database.
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let db = Connection::open(db_path)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS registry_rows (
                 kind TEXT NOT NULL,
                 id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY (kind, id)
             );
             CREATE TABLE IF NOT EXISTS registry_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        let mut room = Self {
            seq: 0,
            gc_floor: 0,
            presence: HashMap::new(),
            rows: HashMap::new(),
            db,
        };
        room.seq = room
            .meta("seq")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        room.gc_floor = room
            .meta("gcFloor")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut stmt = room.db.prepare("SELECT data FROM registry_rows")?;
        let loaded: Vec<Vec<u8>> = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .filter_map(|row| row.ok())
            .collect();
        drop(stmt);
        for bytes in loaded {
            if let Ok(row) = serde_json::from_slice::<RegistryRow>(&bytes) {
                room.rows.insert((row.kind.clone(), row.id.clone()), row);
            }
        }
        Ok(room)
    }

    fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension as _;
        self.db
            .query_row(
                "SELECT value FROM registry_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
    }

    /// The reply to `hello`: the whole registry, or just rows newer than `cursor`.
    pub fn state_frame(&self, cursor: Option<u64>) -> String {
        let full = match cursor {
            None => true,
            Some(cursor) => cursor < self.gc_floor || cursor > self.seq,
        };
        let rows: Vec<&RegistryRow> = self
            .rows
            .values()
            .filter(|row| full || row.seq > cursor.unwrap_or(0))
            .collect();
        json!({
            "t": "state",
            "seq": self.seq,
            "full": full,
            "gcFloor": self.gc_floor,
            "rows": rows,
            "presence": self.presence,
        })
        .to_string()
    }

    /// Merge one batch. Returns the `ack` and, when anything changed, the `rows`
    /// frame to broadcast (to every socket, the sender included).
    pub fn push(&mut self, batch: &str, ops: &[RowOp]) -> (String, Option<String>) {
        let next_seq = self.seq + 1;
        let mut touched: Vec<RegistryRow> = Vec::new();
        let mut applied = 0u64;
        for op in ops {
            let before = touched
                .iter()
                .find(|row| row.kind == op.kind && row.id == op.id)
                .or_else(|| self.rows.get(&(op.kind.clone(), op.id.clone())));
            let (next, changed) = apply_op(before, op);
            let Some(mut next) = next else { continue };
            if !changed {
                continue;
            }
            applied += 1;
            next.seq = next_seq;
            touched.retain(|row| !(row.kind == next.kind && row.id == next.id));
            touched.push(next);
        }
        let rows_frame = if applied > 0 {
            // Row + seq land together: a crash between them would hand a client a
            // `rows` frame for a sequence the next boot never reaches.
            let persist = self.persist(next_seq, &touched);
            if let Err(err) = persist {
                tracing::error!(error = %err, "registry persistence failed");
            }
            self.seq = next_seq;
            for row in &touched {
                self.rows
                    .insert((row.kind.clone(), row.id.clone()), row.clone());
            }
            Some(json!({ "t": "rows", "seq": next_seq, "rows": touched }).to_string())
        } else {
            None
        };
        let ack = json!({ "t": "ack", "batch": batch, "seq": self.seq, "applied": applied });
        (ack.to_string(), rows_frame)
    }

    fn persist(&self, seq: u64, rows: &[RegistryRow]) -> rusqlite::Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for row in rows {
            let data = serde_json::to_vec(row).map_err(|err| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    err.to_string(),
                )))
            })?;
            tx.execute(
                "INSERT INTO registry_rows (kind, id, seq, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind, id) DO UPDATE SET seq = excluded.seq, data = excluded.data",
                rusqlite::params![row.kind, row.id, seq as i64, data],
            )?;
        }
        tx.execute(
            "INSERT INTO registry_meta (key, value) VALUES ('seq', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [seq.to_string()],
        )?;
        tx.commit()
    }

    /// Remember a peer's beat (ephemeral: presence is about now, not history) and
    /// return the frame to broadcast.
    pub fn presence(&mut self, device: &str, at: i64) -> String {
        self.presence.insert(device.to_string(), at);
        json!({ "t": "presence", "device": device, "at": at }).to_string()
    }

    pub fn probe_ok(&self) -> String {
        json!({ "t": "probe-ok", "seq": self.seq }).to_string()
    }
}

/// Parse the `ops` array of a `push` frame.
pub fn parse_ops(frame: &Value) -> Result<Vec<RowOp>, String> {
    serde_json::from_value::<Vec<RowOp>>(frame.get("ops").cloned().unwrap_or(Value::Null))
        .map_err(|err| err.to_string())
}

/// Error frame for a client that sent something unusable.
pub fn error_frame(code: &str, message: &str) -> String {
    json!({ "t": "error", "code": code, "message": message }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn op(kind: &str, id: &str, hlc: &str, name: &str) -> RowOp {
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), json!(name));
        RowOp {
            kind: kind.into(),
            id: id.into(),
            op: komet_doc::OpKind::Upsert,
            set: Some(set),
            hlc: hlc.into(),
            clocks: None,
        }
    }

    fn room() -> (tempfile::TempDir, RegistryRoom) {
        let dir = tempfile::tempdir().unwrap();
        let room = RegistryRoom::open(&dir.path().join("room.db")).unwrap();
        (dir, room)
    }

    fn rows(frame: &str) -> (u64, Vec<RegistryRow>) {
        let value: Value = serde_json::from_str(frame).unwrap();
        let seq = value["seq"].as_u64().unwrap();
        let rows = serde_json::from_value(value["rows"].clone()).unwrap();
        (seq, rows)
    }

    #[test]
    fn hello_returns_the_full_registry_then_a_cursor_delta() {
        let (_dir, mut room) = room();
        let state: Value = serde_json::from_str(&room.state_frame(None)).unwrap();
        assert_eq!(state["t"], "state");
        assert_eq!(state["full"], true);
        assert_eq!(state["rows"].as_array().unwrap().len(), 0);

        let (ack, rows_frame) = room.push("b1", &[op("chat", "c1", "1", "first")]);
        let rows_frame = rows_frame.expect("rows frame");
        let (pushed_seq, pushed_rows) = rows(&rows_frame);
        assert_eq!(pushed_rows.len(), 1);
        let ack: Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["applied"], 1);
        assert_eq!(ack["batch"], "b1");

        let delta: Value = serde_json::from_str(&room.state_frame(Some(0))).unwrap();
        assert_eq!(delta["full"], false, "a live cursor gets a delta");
        assert_eq!(delta["seq"], pushed_seq);
        assert_eq!(delta["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pushed_rows_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.db");
        {
            let mut room = RegistryRoom::open(&path).unwrap();
            room.push("b1", &[op("space", "s1", "1", "proj")]);
        }
        let room = RegistryRoom::open(&path).unwrap();
        let state: Value = serde_json::from_str(&room.state_frame(None)).unwrap();
        assert_eq!(state["seq"], 1, "the sequence must not restart at zero");
        let row: RegistryRow = serde_json::from_value(state["rows"][0].clone()).unwrap();
        assert_eq!(row.id, "s1");
        assert_eq!(row.fields["name"], "proj");
    }

    #[test]
    fn an_older_write_does_not_bump_the_sequence() {
        let (_dir, mut room) = room();
        room.push("b1", &[op("chat", "c1", "2", "newer")]);
        let (ack, rows) = room.push("b2", &[op("chat", "c1", "1", "older")]);
        let ack: Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["applied"], 0, "an LWW loser is not applied");
        assert_eq!(ack["seq"], 1);
        assert!(rows.is_none(), "nothing to broadcast");
    }

    #[test]
    fn deletes_are_tombstoned_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.db");
        let mut room = RegistryRoom::open(&path).unwrap();
        room.push("b1", &[op("chat", "c1", "1", "doomed")]);
        let delete = RowOp {
            kind: "chat".into(),
            id: "c1".into(),
            op: komet_doc::OpKind::Delete,
            set: None,
            hlc: "2".into(),
            clocks: None,
        };
        room.push("b2", &[delete]);
        drop(room);
        let room = RegistryRoom::open(&path).unwrap();
        let state: Value = serde_json::from_str(&room.state_frame(None)).unwrap();
        let row: RegistryRow = serde_json::from_value(state["rows"][0].clone()).unwrap();
        assert!(row.deleted);
        assert_eq!(row.seq, 2);
    }

    #[test]
    fn presence_is_ephemeral_but_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("room.db");
        let mut room = RegistryRoom::open(&path).unwrap();
        let frame: Value = serde_json::from_str(&room.presence("device-a", 42)).unwrap();
        assert_eq!(frame["t"], "presence");
        let state: Value = serde_json::from_str(&room.state_frame(None)).unwrap();
        assert_eq!(state["presence"]["device-a"], 42);
        drop(room);
        let room = RegistryRoom::open(&path).unwrap();
        let state: Value = serde_json::from_str(&room.state_frame(None)).unwrap();
        assert!(state["presence"].as_object().unwrap().is_empty());
    }
}
