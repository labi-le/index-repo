//! Test-support utilities shared across modules.
//! Only compiled in `#[cfg(test)]` builds (declared as `#[cfg(test)] pub(crate) mod testkit`).

use crate::store::{Embed, Meta, Record, Store};
use anyhow::Result;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// MockStore
// ---------------------------------------------------------------------------

/// In-memory `Store` implementation for unit tests.
/// Tracks every call so tests can assert on what was added/deleted.
pub(crate) struct MockStore {
    pub ids: HashSet<String>,
    pub metas: Vec<(String, Meta)>,
    pub added: Vec<(Record, Vec<f32>)>,
    pub deleted: Vec<String>,
    pub collection: Option<String>,
}

impl MockStore {
    pub(crate) fn new() -> Self {
        Self {
            ids: HashSet::new(),
            metas: Vec::new(),
            added: Vec::new(),
            deleted: Vec::new(),
            collection: None,
        }
    }

    /// Pre-seed existing ids (simulate a collection that already has chunks).
    pub(crate) fn with_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.ids.extend(ids);
        self
    }
}

impl Store for MockStore {
    fn heartbeat(&self) -> Result<()> {
        Ok(())
    }
    fn get_or_create(&mut self, name: &str) -> Result<()> {
        self.collection = Some(name.to_string());
        Ok(())
    }
    fn delete_collection(&mut self, _name: &str) -> Result<()> {
        self.ids.clear();
        self.metas.clear();
        Ok(())
    }
    fn existing_ids(&self) -> Result<HashSet<String>> {
        Ok(self.ids.clone())
    }
    fn metadatas(&self) -> Result<Vec<(String, Meta)>> {
        Ok(self.metas.clone())
    }
    fn add(&mut self, records: &[Record], embeddings: &[Vec<f32>]) -> Result<usize> {
        for (r, e) in records.iter().zip(embeddings.iter()) {
            self.ids.insert(r.id.clone());
            self.metas.push((r.id.clone(), r.meta.clone()));
            self.added.push((r.clone(), e.clone()));
        }
        Ok(records.len())
    }
    fn delete(&mut self, ids: &[String]) -> Result<usize> {
        let n = ids.len();
        for id in ids {
            self.ids.remove(id);
            self.deleted.push(id.clone());
        }
        Ok(n)
    }
    fn count(&self) -> Result<usize> {
        Ok(self.ids.len())
    }
}

// ---------------------------------------------------------------------------
// FakeEmbed
// ---------------------------------------------------------------------------

/// Zero-vector embedder for tests — avoids onnxruntime.
pub(crate) struct FakeEmbed;

impl Embed for FakeEmbed {
    fn embed(&self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0_f32; 384]; docs.len()])
    }
}
