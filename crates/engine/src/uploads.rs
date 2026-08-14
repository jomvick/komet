//! Uploads — attachment staging + the content-addressed edge mirror
//! (feature-inventory §3.7 "Uploads"; port of komet's `uploads.ts`).
//!
//! The UI streams a file as base64 chunks (~60KB, sized for the relay when the
//! target device is remote); chunks stage on disk under `{uploads_root}/tmp/
//! {uploadId}/{seq}.b64` (surviving an engine restart mid-upload, unlike komet's
//! in-memory buffers), and `commit` assembles them into
//! `{uploads_root}/{id8}-{name}` and returns the absolute path, which the
//! composer appends to the prompt so the agent can read the file from disk.
//!
//! On commit the assembled bytes are queued durably for mirroring to the edge:
//! `PUT {edge}/attachments/{sha256}` (bearer auth, content-addressed R2 —
//! `edge/src/index.ts`). The persistent outbox survives offline starts and
//! process restarts, then drains when credentials or connectivity recover.
//! A device that doesn't hold the file locally can fall
//! back to `GET {edge}/attachments/{sha256}` with the same bearer; native keeps
//! reads local-first (`read_chunk` proxies through the owning device), so the
//! GET fallback is the disaster path, not the hot path.
//!
//! `read_chunk` serves transcript images back in 45KB base64 chunks. Path jail:
//! only files under the uploads dir or a workspace-known chat cwd are readable
//! (the RPC layer supplies the cwd roots) — and only supported image types, as
//! in komet.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::hex;

/// A pending upload must finish within this window (covers slow mesh links).
const STAGING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an assembled file (matches the edge's 32MB attachment cap).
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Multiple of 3 so independent base64 chunks concatenate losslessly.
const READ_CHUNK_BYTES: u64 = 45_000;
const MIRROR_RETRY_BASE: Duration = Duration::from_secs(1);
const MIRROR_RETRY_CAP: Duration = Duration::from_secs(60);

/// `ReadAttachmentChunk` reply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChunk {
    pub name: String,
    pub mime_type: String,
    /// Base64 of this chunk's byte range.
    pub data: String,
    pub next_offset: u64,
    pub done: bool,
}

struct UploadsInner {
    /// Profile-scoped durable home for new committed attachments.
    dir: PathBuf,
    /// Chunk staging (`{uploads_root}/tmp/{uploadId}/`).
    tmp: PathBuf,
    /// Historical roots accepted for reads only. Writes and staging never use them.
    read_only_roots: Vec<PathBuf>,
    /// Durable attachment mirror intents (`{sha}.pending`).
    outbox: PathBuf,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    mirror_kick: tokio::sync::watch::Sender<u64>,
    /// Cuts the mirror worker at any await point — a cancel mid-drain must
    /// drop the in-flight PUT, not let it finish under a stale bearer.
    mirror_cancel: tokio_util::sync::CancellationToken,
    mirror_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Uploads {
    inner: Arc<UploadsInner>,
}

impl Uploads {
    /// Use the historical device-global uploads directory.
    pub fn new(data_dir: &Path, edge: Option<EdgeConfig>) -> Self {
        Self::from_root(&data_dir.join("uploads"), edge)
    }

    /// Use an already-resolved profile uploads directory.
    pub fn from_root(dir: &Path, edge: Option<EdgeConfig>) -> Self {
        Self::from_root_with_fallback(dir, None, edge)
    }

    /// Use a profile root for all writes and an optional legacy read-only root.
    pub fn from_root_with_fallback(
        dir: &Path,
        legacy_read_root: Option<&Path>,
        edge: Option<EdgeConfig>,
    ) -> Self {
        let (mirror_kick, _) = tokio::sync::watch::channel(0);
        let uploads = Self {
            inner: Arc::new(UploadsInner {
                tmp: dir.join("tmp"),
                outbox: dir.join("mirror-outbox"),
                dir: dir.to_path_buf(),
                read_only_roots: legacy_read_root
                    .into_iter()
                    .map(Path::to_path_buf)
                    .collect(),
                edge,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
                mirror_kick,
                mirror_cancel: tokio_util::sync::CancellationToken::new(),
                mirror_task: std::sync::Mutex::new(None),
            }),
        };
        if uploads.inner.edge.is_some() && tokio::runtime::Handle::try_current().is_ok() {
            uploads.spawn_mirror_worker();
        }
        uploads
    }

    /// The durable uploads dir (a path-jail root).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Stage one base64 chunk. Positional (`seq`) writes are IDEMPOTENT: a client
    /// retrying a chunk whose ack was lost overwrites the same slot instead of
    /// double-appending. Callers without `seq` get append-only behavior.
    pub fn append(&self, upload_id: &str, data: &str, seq: Option<u64>) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = match seq {
            Some(seq) => seq,
            None => next_free_seq(&dir)?,
        };
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        // Base64 inflates by ~4/3; bound the staged payload against the file cap.
        let staged: u64 = chunk_files(&dir)?
            .iter()
            .filter(|(seq, _)| *seq != at)
            .map(|(_, path)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            .sum();
        if (staged + data.len() as u64) * 3 / 4 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.b64")), data)?;
        Ok(())
    }

    /// Assemble the staged chunks into a durable file and return its absolute
    /// path. Synced profiles also persist a content-addressed mirror intent
    /// before acknowledging the commit, so an outage cannot lose the upload.
    pub fn commit(&self, upload_id: &str, file_name: &str) -> Result<String, EngineError> {
        let dir = self.staging_dir(upload_id)?;
        let mut parts = chunk_files(&dir)?;
        if parts.is_empty() {
            return Err(EngineError::Other("Unknown or expired upload".into()));
        }
        parts.sort_by_key(|(seq, _)| *seq);
        // Positional appends may leave holes if a chunk never arrived — joining
        // around them would silently corrupt the file.
        let mut joined = String::new();
        for (i, (seq, path)) in parts.iter().enumerate() {
            if *seq != i as u64 {
                return Err(EngineError::Other("Upload is missing a chunk".into()));
            }
            joined.push_str(std::fs::read_to_string(path)?.trim());
        }
        let bytes = BASE64
            .decode(joined.as_bytes())
            .map_err(|e| EngineError::Other(format!("upload is not valid base64: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::create_dir_all(&self.inner.dir)?;
        let name = sanitize(file_name);
        let id8: String = upload_id.chars().take(8).collect();
        let path = self.inner.dir.join(format!("{id8}-{name}"));
        std::fs::write(&path, &bytes)?;
        self.enqueue_mirror(&path, &bytes)?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(path.to_string_lossy().to_string())
    }

    /// Read one 45KB chunk of an attachment. `extra_roots` are the workspace's
    /// known chat cwds — together with the uploads dir they form the path jail.
    pub fn read_chunk(
        &self,
        path: &str,
        offset: u64,
        extra_roots: &[PathBuf],
    ) -> Result<AttachmentChunk, EngineError> {
        use std::io::{Read, Seek};
        let file = self.inspect(path, extra_roots)?;
        let size = file.size;
        let start = offset.min(size);
        let next_offset = (start + READ_CHUNK_BYTES).min(size);
        // Read ONLY this chunk's byte range — never the whole file per chunk.
        let mut buf = vec![0u8; (next_offset - start) as usize];
        let mut handle = std::fs::File::open(&file.resolved)?;
        handle.seek(std::io::SeekFrom::Start(start))?;
        let mut read = 0usize;
        while read < buf.len() {
            let n = handle.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(AttachmentChunk {
            name: file.name,
            mime_type: file.mime_type,
            data: BASE64.encode(&buf),
            next_offset,
            done: next_offset >= size,
        })
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn staging_dir(&self, upload_id: &str) -> Result<PathBuf, EngineError> {
        // The id becomes a directory name — jail it to a safe charset.
        let ok = !upload_id.is_empty()
            && upload_id.len() <= 64
            && upload_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !ok {
            return Err(EngineError::Other("Invalid upload id".into()));
        }
        Ok(self.inner.tmp.join(upload_id))
    }

    /// Reclaim staging dirs whose newest chunk is older than the TTL (an upload
    /// abandoned mid-stream must not hold up to 32MB forever).
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.inner.tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let newest = std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false),
                None => true, // empty dir — reclaim
            };
            if expired {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn inspect(&self, path: &str, extra_roots: &[PathBuf]) -> Result<InspectedFile, EngineError> {
        let outside = || EngineError::Other("Attachment is outside the upload cache".into());
        // Canonicalize BOTH sides so `..` segments and symlinks can't escape.
        let resolved = std::fs::canonicalize(path).map_err(|_| outside())?;
        let allowed = std::iter::once(&self.inner.dir)
            .chain(self.inner.read_only_roots.iter())
            .chain(extra_roots.iter())
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(&root) && resolved != root);
        if !allowed {
            return Err(outside());
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Err(EngineError::Other("Attachment is not a file".into()));
        }
        if meta.len() > MAX_BYTES {
            return Err(EngineError::Other("Attachment is too large".into()));
        }
        let mime_type = mime_by_ext(&resolved)
            .ok_or_else(|| EngineError::Other("Attachment is not a supported image".into()))?;
        Ok(InspectedFile {
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime_type: mime_type.to_string(),
            size: meta.len(),
            resolved,
        })
    }

    fn enqueue_mirror(&self, path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
        if self.inner.edge.is_none() {
            return Ok(());
        }
        let sha = hex(&Sha256::digest(&bytes));
        let marker = self.inner.outbox.join(format!("{sha}.pending"));
        if !marker.exists() {
            std::fs::create_dir_all(&self.inner.outbox)?;
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .ok_or_else(|| EngineError::Other("Attachment has no file name".into()))?;
            let body = serde_json::to_vec(&MirrorIntent { file_name }).map_err(|err| {
                EngineError::Other(format!("could not encode mirror intent: {err}"))
            })?;
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let temp = self
                .inner
                .outbox
                .join(format!(".{sha}-{}-{nonce}.tmp", std::process::id()));
            std::fs::write(&temp, body)?;
            if let Err(err) = std::fs::rename(&temp, &marker) {
                let _ = std::fs::remove_file(&temp);
                if !marker.exists() {
                    return Err(err.into());
                }
            }
        }
        self.inner
            .mirror_kick
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        Ok(())
    }

    /// Stop the mirror worker and wait for it to exit. A cancel mid-drain
    /// aborts the in-flight `PUT /attachments/{sha}` — after sign-out nothing
    /// queued may keep uploading under the old bearer. Idempotent; a no-op for
    /// local profiles (no edge, no worker spawned).
    pub async fn shutdown(&self) {
        self.inner.mirror_cancel.cancel();
        let task = self
            .inner
            .mirror_task
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    fn spawn_mirror_worker(&self) {
        let weak = Arc::downgrade(&self.inner);
        let mut kicks = self.inner.mirror_kick.subscribe();
        let mut token_changes = self.inner.edge.as_ref().and_then(EdgeConfig::token_changes);
        let cancel = self.inner.mirror_cancel.clone();
        let task = tokio::spawn(async move {
            let mut retry = MIRROR_RETRY_BASE;
            loop {
                let Some(inner) = weak.upgrade() else { return };
                let pending = tokio::select! {
                    _ = cancel.cancelled() => return,
                    pending = drain_mirror_outbox(&inner) => pending,
                };
                drop(inner);
                if !pending {
                    retry = MIRROR_RETRY_BASE;
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        changed = kicks.changed() => {
                            if changed.is_err() { return; }
                        }
                        _ = crate::workspace_host::token_changed(&mut token_changes) => {}
                    }
                    continue;
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(retry) => {
                        retry = (retry * 2).min(MIRROR_RETRY_CAP);
                    }
                    changed = kicks.changed() => {
                        if changed.is_err() { return; }
                        retry = MIRROR_RETRY_BASE;
                    }
                    _ = crate::workspace_host::token_changed(&mut token_changes) => {
                        retry = MIRROR_RETRY_BASE;
                    }
                }
            }
        });
        *self
            .inner
            .mirror_task
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = Some(task);
    }
}

#[derive(Serialize, Deserialize)]
struct MirrorIntent {
    file_name: String,
}

/// Drain every currently-readable mirror intent. Returns true while any work
/// remains, including transient auth/network failures.
async fn drain_mirror_outbox(inner: &UploadsInner) -> bool {
    let Some(edge) = &inner.edge else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&inner.outbox) else {
        return false;
    };
    let markers: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("pending"))
        .collect();
    if markers.is_empty() {
        return false;
    }
    let Some(bearer) = edge.bearer().await else {
        return true;
    };
    let mut pending = false;
    for marker in markers {
        let Some(sha) = marker.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            tracing::warn!(path = %marker.display(), "discarding invalid attachment mirror intent");
            let _ = std::fs::remove_file(&marker);
            continue;
        }
        let intent = std::fs::read(&marker)
            .ok()
            .and_then(|body| serde_json::from_slice::<MirrorIntent>(&body).ok());
        let Some(intent) = intent else {
            tracing::warn!(path = %marker.display(), "discarding unreadable attachment mirror intent");
            let _ = std::fs::remove_file(&marker);
            continue;
        };
        let safe_name = Path::new(&intent.file_name);
        if safe_name.components().count() != 1 || safe_name.file_name().is_none() {
            tracing::warn!(path = %marker.display(), "discarding unsafe attachment mirror intent");
            let _ = std::fs::remove_file(&marker);
            continue;
        }
        let path = inner.dir.join(safe_name);
        let Ok(bytes) = std::fs::read(&path) else {
            tracing::warn!(sha, path = %path.display(), "discarding mirror intent for missing attachment");
            let _ = std::fs::remove_file(&marker);
            continue;
        };
        if hex(&Sha256::digest(&bytes)) != sha {
            tracing::warn!(sha, path = %path.display(), "discarding mirror intent whose attachment changed");
            let _ = std::fs::remove_file(&marker);
            continue;
        }
        let url = format!("{}/attachments/{sha}", edge.url.trim_end_matches('/'));
        let mime = mime_by_ext(&path).unwrap_or("application/octet-stream");
        match inner
            .http
            .put(url)
            .bearer_auth(&bearer)
            .header("content-type", mime)
            .body(bytes)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let _ = std::fs::remove_file(&marker);
                tracing::debug!(sha, "attachment mirrored to edge");
            }
            Ok(response) => {
                pending = true;
                tracing::warn!(sha, status = %response.status(), "edge attachment mirror rejected; queued for retry");
            }
            Err(err) => {
                pending = true;
                tracing::warn!(sha, error = %err, "edge attachment mirror failed; queued for retry");
            }
        }
    }
    pending
}

struct InspectedFile {
    resolved: PathBuf,
    name: String,
    mime_type: String,
    size: u64,
}

fn chunk_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, EngineError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(seq) = seq
            && path.extension().and_then(|e| e.to_str()) == Some("b64")
        {
            files.push((seq, path));
        }
    }
    Ok(files)
}

fn next_free_seq(dir: &Path) -> Result<u64, EngineError> {
    Ok(chunk_files(dir)?
        .iter()
        .map(|(seq, _)| seq + 1)
        .max()
        .unwrap_or(0))
}

fn sanitize(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "upload".into()
    } else {
        tail
    }
}

fn mime_by_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoveringToken {
        value: Mutex<Option<String>>,
        changes: tokio::sync::watch::Sender<u64>,
    }

    impl RecoveringToken {
        fn unavailable() -> Self {
            let (changes, _) = tokio::sync::watch::channel(0);
            Self {
                value: Mutex::new(None),
                changes,
            }
        }

        fn recover(&self) {
            *self.value.lock().unwrap() = Some("test-token".into());
            self.changes
                .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        }
    }

    #[async_trait::async_trait]
    impl komet_rpc::TokenSource for RecoveringToken {
        async fn token(&self) -> Option<String> {
            self.value.lock().unwrap().clone()
        }

        fn subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
            Some(self.changes.subscribe())
        }
    }

    async fn attachment_edge() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 16 * 1024];
                    let _ = stream.read(&mut request).await;
                    seen.fetch_add(1, Ordering::SeqCst);
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        (url, requests, task)
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("my photo (1).png"), "my_photo__1_.png");
        assert_eq!(sanitize(""), "upload");
    }

    #[test]
    fn local_commit_does_not_create_a_mirror_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = Uploads::from_root(dir.path(), None);
        uploads
            .append("upload-1", &BASE64.encode(b"local"), Some(0))
            .unwrap();

        uploads.commit("upload-1", "image.png").unwrap();

        assert!(!dir.path().join("mirror-outbox").exists());
    }

    #[tokio::test]
    async fn mirror_outbox_survives_restart_and_wakes_when_token_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let (edge_url, requests, edge_task) = attachment_edge().await;

        let first_token = Arc::new(RecoveringToken::unavailable());
        let uploads = Uploads::from_root(
            dir.path(),
            Some(EdgeConfig::new(edge_url.clone(), first_token)),
        );
        uploads
            .append("upload-1", &BASE64.encode(b"queued attachment"), Some(0))
            .unwrap();
        uploads.commit("upload-1", "image.png").unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            std::fs::read_dir(dir.path().join("mirror-outbox"))
                .unwrap()
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some("pending")
                })
                .count(),
            1
        );
        drop(uploads);

        let recovered_token = Arc::new(RecoveringToken::unavailable());
        let restarted = Uploads::from_root(
            dir.path(),
            Some(EdgeConfig::new(edge_url, recovered_token.clone())),
        );
        tokio::task::yield_now().await;
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        recovered_token.recover();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pending = std::fs::read_dir(dir.path().join("mirror-outbox"))
                    .unwrap()
                    .flatten()
                    .any(|entry| {
                        entry.path().extension().and_then(|ext| ext.to_str()) == Some("pending")
                    });
                if requests.load(Ordering::SeqCst) == 1 && !pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("queued mirror did not drain after token recovery");

        drop(restarted);
        edge_task.abort();
    }
}
