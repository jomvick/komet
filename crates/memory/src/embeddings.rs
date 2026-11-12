use std::path::Path;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::MemoryError;

pub const EMBEDDING_DIMENSIONS: usize = 384;
const MODEL: EmbeddingModel = EmbeddingModel::AllMiniLML6V2Q;

pub struct LocalEmbedder {
    model: TextEmbedding,
}

impl LocalEmbedder {
    pub fn load(cache_dir: &Path) -> Result<Self, MemoryError> {
        std::fs::create_dir_all(cache_dir)?;
        let options = TextInitOptions::new(MODEL)
            .with_cache_dir(cache_dir.to_path_buf())
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
