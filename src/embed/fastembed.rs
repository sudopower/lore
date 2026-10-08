use std::path::Path;
use std::sync::Mutex;

use ::fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use anyhow::{Result, anyhow};

use super::Embedder;

const BATCH_SIZE: usize = 64;
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// bge-small-en-v1.5 on CPU through the ONNX runtime. `TextEmbedding::embed` takes `&mut self`,
/// hence the mutex: one batch runs at a time.
pub struct FastEmbedder {
    model: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    /// Loads the model from `cache_dir`, downloading it (about 130 MB) when it is missing.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false);
        let model = TextEmbedding::try_new(options).map_err(|e| {
            anyhow!(
                "loading bge-small-en-v1.5 into {}: {e}",
                cache_dir.display()
            )
        })?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| anyhow!("embedding model lock poisoned"))?;
        model
            .embed(texts, Some(BATCH_SIZE))
            .map_err(|e| anyhow!("embedding failed: {e}"))
    }
}

impl Embedder for FastEmbedder {
    fn name(&self) -> &str {
        "fastembed:bge-small-en-v1.5"
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.embed(texts)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prefixed = vec![format!("{QUERY_PREFIX}{text}")];
        self.embed(&prefixed)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("embedder returned no vector"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::DIM;

    /// Downloads the model (about 130 MB) into a temp dir. Run with
    /// `cargo test -- --ignored fastembed_smoke`.
    #[test]
    #[ignore = "downloads the bge-small-en-v1.5 model"]
    fn fastembed_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let e = FastEmbedder::new(dir.path()).unwrap();
        assert_eq!(e.name(), "fastembed:bge-small-en-v1.5");
        let docs = e
            .embed_documents(&[
                "plan limits seats".to_string(),
                "limits for each plan".to_string(),
                "theme toggle button".to_string(),
            ])
            .unwrap();
        assert_eq!(docs.len(), 3);
        assert!(docs.iter().all(|v| v.len() == DIM));
        assert_eq!(e.embed_query("plan limits").unwrap().len(), DIM);
    }
}
