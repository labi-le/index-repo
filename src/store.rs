use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Chunk metadata — serialises to the exact JSON ChromaDB expects (spec §1.2 / §4)
// ---------------------------------------------------------------------------

/// Per-chunk metadata stored in ChromaDB.
///
/// JSON shape:
///   { "path": "...", "line": N, "lang": "...", "type": "...", ["scope": "..."] }
/// `scope` is omitted (not set to null) when it is empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub path: String,
    pub line: usize,
    pub lang: String,
    /// ChromaDB metadata key is literally `"type"`.
    #[serde(rename = "type")]
    pub node_type: String,
    /// Omitted from JSON when empty.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub scope: String,
}

// ---------------------------------------------------------------------------
// A computed chunk ready to be sent to the store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Record {
    pub id: String,
    pub body: String,
    pub meta: Meta,
}

// ---------------------------------------------------------------------------
// One-shot statistics (spec §6)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files: usize,
    pub added: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub ts_chunks: usize,
    pub win_chunks: usize,
    pub skipped_bin: usize,
}

// ---------------------------------------------------------------------------
// Store trait (spec §8 ops needed by oneshot / daemon)
// ---------------------------------------------------------------------------

pub trait Store {
    /// Verify reachability (heartbeat). Used by main for exit code 3.
    fn heartbeat(&self) -> Result<()>;

    /// Create or retrieve the named collection.
    /// Must cache the returned collection id internally for subsequent ops.
    fn get_or_create(&mut self, name: &str) -> Result<()>;

    /// Delete a collection (used for `--full-rebuild`). Swallow errors.
    fn delete_collection(&mut self, name: &str) -> Result<()>;

    /// Paginated fetch of ALL chunk ids from the current collection.
    fn existing_ids(&self) -> Result<HashSet<String>>;

    /// Paginated fetch of all (id, meta) pairs from chunk metadatas.
    /// Used by the daemon to build its path→ids map (spec §7).
    fn metadatas(&self) -> Result<Vec<(String, Meta)>>;

    /// Add a batch of records (ids, embeddings, documents, metadatas).
    /// Embeddings are computed externally and passed in; the store just POSTs them.
    fn add(&mut self, records: &[Record], embeddings: &[Vec<f32>]) -> Result<usize>;

    /// Delete records by id.
    fn delete(&mut self, ids: &[String]) -> Result<usize>;

    /// Count records in the current collection.
    fn count(&self) -> Result<usize>;
}

// ---------------------------------------------------------------------------
// Embed trait
// ---------------------------------------------------------------------------

/// Abstraction over embedding computation.
/// Implemented by the real fastembed embedder (Task 9) and FakeEmbed in tests.
pub trait Embed {
    /// Embed document bodies → one 384-dim L2-normalised vector each, in order.
    fn embed(&self, docs: &[String]) -> Result<Vec<Vec<f32>>>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with_scope() -> Meta {
        Meta {
            path: "src/foo.rs".to_string(),
            line: 42,
            lang: "rs".to_string(),
            node_type: "function_item".to_string(),
            scope: "MyStruct".to_string(),
        }
    }

    fn meta_no_scope() -> Meta {
        Meta {
            path: "src/bar.py".to_string(),
            line: 1,
            lang: "py".to_string(),
            node_type: "window".to_string(),
            scope: String::new(),
        }
    }

    #[test]
    fn meta_scope_serializes() {
        let m = meta_with_scope();
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v["scope"], "MyStruct",
            "scope must be present when non-empty"
        );
        assert_eq!(
            v["type"], "function_item",
            "key must be 'type', not 'node_type'"
        );
        assert_eq!(v["lang"], "rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["path"], "src/foo.rs");
    }

    #[test]
    fn meta_no_scope_omits_key() {
        let m = meta_no_scope();
        let v = serde_json::to_value(&m).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("scope"),
            "scope must be omitted when empty, got: {v}"
        );
        assert_eq!(v["type"], "window");
    }

    #[test]
    fn meta_roundtrip() {
        let m = meta_with_scope();
        let json = serde_json::to_string(&m).unwrap();
        let m2: Meta = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn stats_default() {
        let s = Stats::default();
        assert_eq!(s.files, 0);
        assert_eq!(s.added, 0);
    }

    #[test]
    fn mock_store_add_delete() {
        use crate::testkit::MockStore;
        let mut store = MockStore::new();
        store.get_or_create("test-col").unwrap();
        assert_eq!(store.collection.as_deref(), Some("test-col"));

        let rec = Record {
            id: "abc123".to_string(),
            body: "fn foo() {}".to_string(),
            meta: meta_no_scope(),
        };
        store.add(&[rec], &[vec![0.1_f32; 384]]).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert!(store.existing_ids().unwrap().contains("abc123"));

        store.delete(&["abc123".to_string()]).unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }
}
