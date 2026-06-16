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

    /// Paginated fetch of all (id, path) pairs from chunk metadatas.
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

    /// Minimal in-memory mock Store for use in oneshot/daemon tests.
    #[cfg(test)]
    pub struct MockStore {
        pub ids: HashSet<String>,
        pub metas: Vec<(String, Meta)>,
        pub added: Vec<(Record, Vec<f32>)>,
        pub deleted: Vec<String>,
        pub collection: Option<String>,
    }

    #[cfg(test)]
    impl MockStore {
        pub fn new() -> Self {
            Self {
                ids: HashSet::new(),
                metas: Vec::new(),
                added: Vec::new(),
                deleted: Vec::new(),
                collection: None,
            }
        }
    }

    #[cfg(test)]
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

    #[test]
    fn mock_store_add_delete() {
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
