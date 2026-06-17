use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use std::path::Path;
use std::sync::{Mutex, Once};

static ORT_INIT: Once = Once::new();

// Cap the per-batch transient peak. The arena itself is disabled below, so
// these just bound a single batch's scratch. Parity-safe: values unchanged.
const INTRA_THREADS: usize = 4;
const EMBED_BATCH: usize = 32;

fn maybe_init_ort() {
    ORT_INIT.call_once(|| {
        if let Ok(path) = std::env::var("ORT_DYLIB_PATH") {
            match ort::init_from(&path) {
                Ok(builder) => {
                    builder.commit();
                }
                Err(e) => {
                    eprintln!("embed: ort::init_from({path}) failed: {e}");
                }
            }
        }
    });
}

pub struct Embedder {
    model: Mutex<TextEmbedding>,
}

impl Embedder {
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var("INDEX_REPO_MODEL_DIR").context("INDEX_REPO_MODEL_DIR not set")?;
        Self::from_dir(Path::new(&dir))
    }

    pub fn from_dir(dir: &Path) -> Result<Self> {
        maybe_init_ort();

        let onnx_bytes = std::fs::read(dir.join("model.onnx")).context("reading model.onnx")?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: std::fs::read(dir.join("tokenizer.json"))
                .context("reading tokenizer.json")?,
            config_file: std::fs::read(dir.join("config.json")).context("reading config.json")?,
            special_tokens_map_file: std::fs::read(dir.join("special_tokens_map.json"))
                .context("reading special_tokens_map.json")?,
            tokenizer_config_file: std::fs::read(dir.join("tokenizer_config.json"))
                .context("reading tokenizer_config.json")?,
        };

        let model_def =
            UserDefinedEmbeddingModel::new(onnx_bytes, tokenizer_files).with_pooling(Pooling::Mean); // all-MiniLM-L6-v2: mean-pool + L2-norm

        // CPU::with_arena_allocator(false) → DisableCpuMemArena: scratch is
        // freed after each inference instead of pooled and retained for the
        // session's life (the ~8 GB resident growth). Values are unaffected.
        let te = TextEmbedding::try_new_from_user_defined(
            model_def,
            InitOptionsUserDefined::default()
                .with_intra_threads(INTRA_THREADS)
                .with_execution_providers(vec![ort::ep::CPU::default()
                    .with_arena_allocator(false)
                    .build()]),
        )?;

        Ok(Self {
            model: Mutex::new(te),
        })
    }
}

impl crate::store::Embed for Embedder {
    fn embed(&self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        if docs.is_empty() {
            return Ok(vec![]);
        }
        let mut guard = self.model.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let out = guard.embed(docs, Some(EMBED_BATCH))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Embed as _;

    #[test]
    fn embeds_384_dims() {
        let (Ok(_dylib), Ok(dir)) = (
            std::env::var("ORT_DYLIB_PATH"),
            std::env::var("INDEX_REPO_MODEL_DIR"),
        ) else {
            eprintln!("skipping: ORT_DYLIB_PATH/INDEX_REPO_MODEL_DIR unset");
            return;
        };
        let e = Embedder::from_dir(Path::new(&dir)).unwrap();
        let v = e.embed(&["hello world".to_string()]).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), 384);
    }
}
