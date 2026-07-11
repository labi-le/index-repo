use anyhow::{Context, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use std::path::Path;
use std::sync::{LazyLock, Mutex, Once};

static ORT_INIT: Once = Once::new();

// Defaults byte-match chromadb's DefaultEmbeddingFunction (batch=32,
// truncation=256, mean pooling) so index vectors stay in the same vector space
// as the chroma-mcp query path. Override the env vars ONLY if you also switch
// the query path to a matching model/config (see INDEX_REPO_MODEL_DIR).
static INTRA_THREADS: LazyLock<usize> = LazyLock::new(|| env_usize("INDEX_REPO_INTRA_THREADS", 4));
static EMBED_BATCH: LazyLock<usize> = LazyLock::new(|| env_usize("INDEX_REPO_EMBED_BATCH", 32));
static MAX_LENGTH: LazyLock<usize> = LazyLock::new(|| env_usize("INDEX_REPO_MAX_LENGTH", 256));

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Pooling strategy, overridable via `INDEX_REPO_POOLING` (`mean` | `cls`).
/// Default `mean` matches all-MiniLM-L6-v2; a code model may need `cls`.
fn env_pooling() -> Pooling {
    match std::env::var("INDEX_REPO_POOLING").as_deref() {
        Ok("cls") | Ok("CLS") => Pooling::Cls,
        _ => Pooling::Mean,
    }
}

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
            UserDefinedEmbeddingModel::new(onnx_bytes, tokenizer_files).with_pooling(env_pooling());

        // CPU::with_arena_allocator(false) → DisableCpuMemArena: scratch is
        // freed after each inference instead of pooled and retained for the
        // session's life (the ~8 GB resident growth). Values are unaffected.
        let te = TextEmbedding::try_new_from_user_defined(
            model_def,
            InitOptionsUserDefined::default()
                .with_intra_threads(*INTRA_THREADS)
                .with_max_length(*MAX_LENGTH)
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
        let out = guard.embed(docs, Some(*EMBED_BATCH))?;
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
