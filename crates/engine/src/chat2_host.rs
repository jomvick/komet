//! chat2 host wiring (docs/chat2-sync.md C3): the engine-side implementations
//! of [`komet_sync::chat_client::ChatDocSink`] and
//! [`komet_sync::chat_client::CheckpointFetcher`], binding a
//! [`crate::doc_host::ChatDocHandle`]'s live doc to a chat2 room.
//!
//! The C2 rule is enforced HERE: every sink method persists doc content AND
//! the room cursor in one `save_snapshot_with_cursor` transaction, so a
//! restored backup can never disagree with its own cursor — the root cause
//! of the redownload-forever class the old s2 clients suffered.

use std::sync::Arc;

use komet_doc::SessionDoc;
use komet_sync::chat_client::{ChatDocSink, CheckpointFetcher};
use komet_sync::{DocsStore, SyncError};
use futures::future::BoxFuture;

use crate::doc_host::EdgeConfig;

/// Doc epoch stamped on every chat2-synced snapshot (docs/chat2-sync.md M1:
/// thin docs are lineage epoch 2; M3 readers discard-and-adopt below it).
pub const CHAT2_DOC_EPOCH: u32 = 2;

/// [`ChatDocSink`] over a live [`SessionDoc`] + the cursor-bearing store.
///
/// Loro import of a remote row/checkpoint fires the doc's root subscription,
/// so the transcript watch, command drain, and debounced UI publish all ride
/// the existing change plumbing — this type only owns import + same-tx
/// persistence.
pub struct EngineChatSink {
    doc: Arc<SessionDoc>,
    store: Arc<DocsStore>,
    chat_id: String,
}

impl EngineChatSink {
    pub fn new(doc: Arc<SessionDoc>, store: Arc<DocsStore>, chat_id: impl Into<String>) -> Self {
        Self {
            doc,
            store,
            chat_id: chat_id.into(),
        }
    }

    /// Export the CURRENT doc and persist it with `cursor` in one tx.
    fn persist_with_cursor(&self, cursor: u64) {
        match self.doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot_with_cursor(
                    &self.chat_id,
                    &bytes,
                    cursor,
                    CHAT2_DOC_EPOCH,
                ) {
                    tracing::warn!(chat = %self.chat_id, error = %err,
                        "chat2 sink: snapshot persist failed (will retry on next change)");
                }
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err,
                    "chat2 sink: snapshot export failed");
            }
        }
    }
}

impl ChatDocSink for EngineChatSink {
    fn apply_row(&self, bytes: &[u8], cursor: u64) {
        if let Err(err) = self.doc.doc().import(bytes) {
            // Malformed remote bytes cost the row, never the doc (the same
            // skip-not-fail rule as transcript reads). The cursor still
            // advances: replaying a poison row forever is the wedge class.
            tracing::warn!(chat = %self.chat_id, error = %err,
                "chat2 sink: row import failed; skipping row");
        }
        self.persist_with_cursor(cursor);
    }

    fn apply_checkpoint(&self, bytes: &[u8], cursor: u64) -> Result<(), String> {
        self.doc
            .doc()
            .import(bytes)
            .map_err(|e| format!("checkpoint import: {e}"))?;
        self.persist_with_cursor(cursor);
        Ok(())
    }

    fn contains_frontier(&self, frontier: &[u8]) -> bool {
        if frontier.is_empty() {
            return true;
        }
        let Ok(vv) = loro::VersionVector::decode(frontier) else {
            // Unreadable frontier → claim NOT contained: the client then
            // fetches the checkpoint, which is always safe (full-state
            // merge), never silently skips history.
            return false;
        };
        self.doc.doc().oplog_vv().includes_vv(&vv)
    }

    fn advance_cursor(&self, cursor: u64) {
        self.persist_with_cursor(cursor);
    }
}

/// `GET /chat2/{chatId}/checkpoint` with Range resume — the fetcher half of
/// the C1 client contract. Partial downloads resume at the byte offset where
/// the previous attempt died (the DO serves 206), which is the entire point
/// of checkpoint-over-HTTP on the 1.2 Mbps links this design targets.
pub struct EdgeCheckpointFetcher {
    http: reqwest::Client,
    edge: EdgeConfig,
    chat_id: String,
}

impl EdgeCheckpointFetcher {
    pub fn new(http: reqwest::Client, edge: EdgeConfig, chat_id: impl Into<String>) -> Self {
        Self {
            http,
            edge,
            chat_id: chat_id.into(),
        }
    }
}

impl CheckpointFetcher for EdgeCheckpointFetcher {
    fn fetch(&self) -> BoxFuture<'static, Result<Vec<u8>, SyncError>> {
        let http = self.http.clone();
        let edge = self.edge.clone();
        let url = format!(
            "{}/chat2/{}/checkpoint",
            edge.url.trim_end_matches('/'),
            self.chat_id
        );
        Box::pin(async move {
            let mut got: Vec<u8> = Vec::new();
            // Range-resume loop: each attempt continues at the byte where
            // the last one stopped. Attempt count bounds a flapping link;
            // the ChatClient's own deadline bounds wall clock.
            for _attempt in 0..4 {
                let bearer = edge
                    .bearer()
                    .await
                    .ok_or_else(|| SyncError::Auth("signed out".into()))?;
                let mut req = http.get(&url).bearer_auth(&bearer);
                if !got.is_empty() {
                    req = req.header("range", format!("bytes={}-", got.len()));
                }
                let res = match req.send().await {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::warn!(error = %err, "chat2 checkpoint fetch attempt failed");
                        continue;
                    }
                };
                match res.status().as_u16() {
                    200 => got.clear(),
                    206 => {}
                    416 => return Err(SyncError::Protocol("checkpoint range beyond end".into())),
                    404 => return Err(SyncError::Protocol("no checkpoint".into())),
                    code => return Err(SyncError::Protocol(format!("checkpoint HTTP {code}"))),
                }
                let mut stream = res;
                loop {
                    match stream.chunk().await {
                        Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                        Ok(None) => return Ok(got),
                        Err(err) => {
                            // Mid-body drop: keep the bytes, resume via Range.
                            tracing::warn!(error = %err, resumed_at = got.len(),
                                "chat2 checkpoint stream dropped; resuming");
                            break;
                        }
                    }
                }
            }
            Err(SyncError::Protocol(
                "checkpoint fetch exhausted resume attempts".into(),
            ))
        })
    }
}
