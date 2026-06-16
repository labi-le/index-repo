use crate::embed::Embedder;
use crate::store::Embed;
use once_cell::sync::OnceCell;

/// Wraps `Embedder` so the ONNX model is loaded only on first `embed()` call.
/// Shared across service worker threads as `Arc<LazyEmbedder>` and passed as `&dyn Embed`.
pub struct LazyEmbedder {
    cell: OnceCell<Embedder>,
}

impl LazyEmbedder {
    pub fn new() -> Self {
        Self {
            cell: OnceCell::new(),
        }
    }
}

impl Default for LazyEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embed for LazyEmbedder {
    fn embed(&self, docs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.cell.get_or_try_init(Embedder::from_env)?.embed(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_does_not_load_model() {
        // Construction is infallible and cheap; it must NOT touch the model
        // even when no model env is set.
        let _ = LazyEmbedder::new();
    }

    #[test]
    fn lazy_embeds_384_when_env_present() {
        // Mirror embed.rs's gating: only run when both env vars are set.
        let (Ok(_dir), Ok(_dylib)) = (
            std::env::var("INDEX_REPO_MODEL_DIR"),
            std::env::var("ORT_DYLIB_PATH"),
        ) else {
            return; // skip when model env absent
        };

        let embedder = LazyEmbedder::new();
        let out = embedder.embed(&["hello".into()]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 384);
    }
}
