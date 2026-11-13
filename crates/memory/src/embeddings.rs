use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::MemoryError;

/// Name of the ONNX Runtime library the release packagers place next to the
/// `komet` binary (see scripts/fetch-onnxruntime.sh). `ort` is built with
/// `load-dynamic`, so nothing is linked at compile time: the first `ort` API
/// call would otherwise panic on a missing dylib. We resolve it up front —
/// `ORT_DYLIB_PATH` first, then the bundled copy — and only then let fastembed
/// run. With no library anywhere, `ort` still falls back to a bare
/// `dlopen("libonnxruntime…")`, so a system-installed runtime keeps working.
#[cfg(target_os = "windows")]
const ORT_DYLIB_NAME: &str = "onnxruntime.dll";
#[cfg(target_os = "macos")]
const ORT_DYLIB_NAME: &str = "libonnxruntime.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const ORT_DYLIB_NAME: &str = "libonnxruntime.so";

fn ensure_ort_runtime() -> Result<(), MemoryError> {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH").filter(|value| !value.is_empty()) {
        let _env = ort::init_from(path).map_err(|error| {
            MemoryError::Other(format!("load ONNX Runtime from ORT_DYLIB_PATH: {error}"))
        })?;
        return Ok(());
    }
    let Ok(exe) = std::env::current_exe() else {
        return Ok(());
    };
    let Some(dir) = exe.parent() else {
        return Ok(());
    };
    // Where the packagers put the library: next to the binary (tarball,
    // Windows zip, portable exe), in a `lib/` subdir or the sibling `lib`,
    // `lib64` and `Frameworks` dirs of FHS layouts and the macOS bundle.
    let mut dirs = vec![dir.to_path_buf(), dir.join("lib")];
    if let Some(parent) = dir.parent() {
        dirs.push(parent.join("lib"));
        dirs.push(parent.join("lib").join("komet"));
        dirs.push(parent.join("lib64").join("komet"));
        dirs.push(parent.join("Frameworks"));
    }
    for candidate in dirs.into_iter().map(|d| d.join(ORT_DYLIB_NAME)) {
        if candidate.is_file() {
            let _env = ort::init_from(&candidate).map_err(|error| {
                MemoryError::Other(format!(
                    "load bundled ONNX Runtime {}: {error}",
                    candidate.display()
                ))
            })?;
            return Ok(());
        }
    }
    Ok(())
}

pub const EMBEDDING_DIMENSIONS: usize = 384;
const MODEL: EmbeddingModel = EmbeddingModel::AllMiniLML6V2Q;
/// Bumped when the bundled model changes: a READY marker from another
/// version is stale and must be downloaded again.
pub const MODEL_VERSION: &str = "all-minilm-l6-v2-q/1";
const READY_FILE: &str = ".komet-model-ready";
/// Sibling of the live model dir: downloads land here and are renamed
/// into place only after the model loads successfully.
const STAGING_SUFFIX: &str = ".staging";

/// What the settings card needs to render the model row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelState {
    /// No model files at all — the download has never run.
    Absent,
    /// A download is in flight right now (staging dir exists).
    Downloading,
    /// Marker verified: model files are complete and loadable.
    Ready { bytes: u64 },
    /// Something is wrong: stale/invalid marker, leftover partial files,
    /// or a failed download. `memory` keeps working without it.
    Failed(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReadyMarker {
    version: String,
    bytes: u64,
}

fn ready_path(dir: &Path) -> PathBuf {
    dir.join(READY_FILE)
}

fn staging_dir(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_os_string();
    name.push(STAGING_SUFFIX);
    PathBuf::from(name)
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() => {
                    total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
                }
                _ => {}
            }
        }
    }
    total
}

/// Inspect the model directory WITHOUT touching the network.
/// `dir` is the live model dir (`<data_dir>/models/memory`).
pub fn model_state(dir: &Path) -> ModelState {
    match std::fs::read_to_string(ready_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str::<ReadyMarker>(&text).ok())
    {
        Some(marker) if marker.version == MODEL_VERSION => {
            // The marker alone proves nothing after the fact: a deleted or
            // truncated model file would make fastembed silently re-download
            // inside `try_new` — the one thing `load` must never do. The
            // marker records the byte count measured at publish time; the
            // live dir (marker file included) can only be that size or more.
            if dir_bytes(dir) >= marker.bytes {
                ModelState::Ready {
                    bytes: marker.bytes,
                }
            } else {
                ModelState::Failed("model files are missing or truncated; download it again".into())
            }
        }
        Some(_) => ModelState::Failed("model marker is from another version".into()),
        None if ready_path(dir).exists() => ModelState::Failed("model marker is unreadable".into()),
        None => {
            if staging_dir(dir).exists() {
                return ModelState::Downloading;
            }
            if dir.exists() && dir_bytes(dir) > 0 {
                ModelState::Failed("model files are incomplete (no ready marker)".into())
            } else {
                ModelState::Absent
            }
        }
    }
}

/// Bytes currently sitting in the staging dir — the download progress the
/// status RPC reports while `ModelState::Downloading`.
pub fn staging_bytes(dir: &Path) -> u64 {
    dir_bytes(&staging_dir(dir))
}

/// Remove every trace of the model (live dir, staging dir, marker) and
/// return the freed byte count. Used by the explicit "remove model" action.
pub fn remove_model(dir: &Path) -> Result<u64, MemoryError> {
    let mut freed = 0u64;
    for path in [dir.to_path_buf(), staging_dir(dir)] {
        freed = freed.saturating_add(dir_bytes(&path));
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
    }
    Ok(freed)
}

/// Download the model into the staging dir and publish it atomically:
/// `try_new` performs the fetch (fastembed owns the bytes), a successful
/// construction IS the verification, then the READY marker is written and
/// the staging dir is renamed over the live one. `cancel` is polled before
/// and after the fetch — fastembed cannot interrupt a mid-flight download,
/// so a cancel during the fetch abandons the staging dir instead.
pub fn download_model(
    dir: &Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<u64, MemoryError> {
    use std::sync::atomic::Ordering;
    ensure_ort_runtime()?;
    let staging = staging_dir(dir);
    if cancel.load(Ordering::Relaxed) {
        return Err(MemoryError::Other("model download cancelled".into()));
    }
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let result = (|| -> Result<(), MemoryError> {
        let options = TextInitOptions::new(MODEL)
            .with_cache_dir(staging.clone())
            .with_show_download_progress(false)
            .with_intra_threads(2);
        let mut model = TextEmbedding::try_new(options).map_err(|error| {
            MemoryError::Other(format!("download local embedding model: {error}"))
        })?;
        // Real verification: a model that merely loads is not proof it is
        // complete. It must also produce a sane vector before READY is
        // written and the staging dir is published.
        let probe = model
            .embed(vec!["komet memory".to_string()], None)
            .map_err(|error| {
                MemoryError::Other(format!("verify downloaded embedding model: {error}"))
            })?;
        if probe.len() != 1
            || probe[0].len() != EMBEDDING_DIMENSIONS
            || probe[0].iter().any(|value| !value.is_finite())
        {
            return Err(MemoryError::Other(
                "downloaded embedding model failed verification".into(),
            ));
        }
        drop(model);
        if cancel.load(Ordering::Relaxed) {
            return Err(MemoryError::Other("model download cancelled".into()));
        }
        let bytes = dir_bytes(&staging);
        let marker = serde_json::to_string(&ReadyMarker {
            version: MODEL_VERSION.into(),
            bytes,
        })
        .map_err(|error| MemoryError::Other(format!("encode model marker: {error}")))?;
        std::fs::write(staging.join(READY_FILE), marker)?;
        // Publish: the live dir is replaced wholesale — a stale or partial
        // previous model must not mix with the fresh files.
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::rename(&staging, dir)?;
        Ok(())
    })();
    if result.is_err() && staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result.map(|()| dir_bytes(dir))
}

pub struct LocalEmbedder {
    model: TextEmbedding,
}

impl LocalEmbedder {
    /// Load the model for inference. NEVER downloads: the READY marker
    /// written by [`download_model`] must be present and match the bundled
    /// version, otherwise this fails before fastembed can touch the
    /// network. Callers gate on [`model_state`] == `Ready`.
    pub fn load(dir: &Path) -> Result<Self, MemoryError> {
        match model_state(dir) {
            ModelState::Ready { .. } => {}
            other => {
                return Err(MemoryError::Other(format!(
                    "local embedding model is not ready ({other:?}); download it explicitly first"
                )));
            }
        }
        ensure_ort_runtime()?;
        let options = TextInitOptions::new(MODEL)
            .with_cache_dir(dir.to_path_buf())
            .with_show_download_progress(false)
            .with_intra_threads(2);
        let model = TextEmbedding::try_new(options)
            .map_err(|error| MemoryError::Other(format!("load local embedding model: {error}")))?;
        Ok(Self { model })
    }

    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self
            .model
            .embed(texts, Some(16))
            .map_err(|error| MemoryError::Other(format!("generate local embeddings: {error}")))?;
        if vectors.len() != texts.len()
            || vectors.iter().any(|vector| {
                vector.len() != EMBEDDING_DIMENSIONS
                    || vector.iter().any(|value| !value.is_finite())
            })
        {
            return Err(MemoryError::Other(
                "local embedding model returned an invalid vector".into(),
            ));
        }
        Ok(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_marker(dir: &Path, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            ready_path(dir),
            serde_json::to_string(&ReadyMarker {
                version: version.into(),
                bytes: 10,
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn model_state_transitions_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("memory");

        assert_eq!(model_state(&model), ModelState::Absent);

        // Partial files without a marker: failed, never "ready".
        std::fs::create_dir_all(model.join("sub")).unwrap();
        std::fs::write(model.join("sub/model.onnx"), b"partial").unwrap();
        assert!(matches!(model_state(&model), ModelState::Failed(_)));

        // A staging dir means a download is in flight.
        let staging = staging_dir(&model);
        std::fs::create_dir_all(&staging).unwrap();
        assert_eq!(model_state(&model), ModelState::Downloading);
        assert_eq!(staging_bytes(&model), 0);
        std::fs::remove_dir_all(&staging).unwrap();

        // Marker with the current version → Ready.
        write_marker(&model, MODEL_VERSION);
        assert_eq!(model_state(&model), ModelState::Ready { bytes: 10 });

        // Stale version marker → Failed (must be re-downloaded).
        write_marker(&model, "old-version");
        assert!(matches!(model_state(&model), ModelState::Failed(_)));

        // remove_model wipes live + staging and reports freed bytes.
        write_marker(&model, MODEL_VERSION);
        std::fs::write(model.join("weights.bin"), vec![0u8; 100]).unwrap();
        let freed = remove_model(&model).unwrap();
        assert!(freed >= 100);
        assert_eq!(model_state(&model), ModelState::Absent);
    }

    #[test]
    fn ready_marker_without_its_files_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("memory");
        std::fs::create_dir_all(&model).unwrap();
        // Marker claims a 1 MB model; nothing but the marker is on disk.
        std::fs::write(
            ready_path(&model),
            serde_json::to_string(&ReadyMarker {
                version: MODEL_VERSION.into(),
                bytes: 1_000_000,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(model_state(&model), ModelState::Failed(_)));
        // ...and load refuses instead of letting fastembed re-download.
        assert!(LocalEmbedder::load(&model).is_err());
    }

    #[test]
    fn load_refuses_without_ready_marker() {
        let dir = tempfile::tempdir().unwrap();
        match LocalEmbedder::load(dir.path()) {
            Err(err) => assert!(err.to_string().contains("not ready")),
            Ok(_) => panic!("load must refuse without a READY marker"),
        }
    }
}
