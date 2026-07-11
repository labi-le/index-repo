use crate::chunkfile::chunks_for_file;
use crate::config::BATCH;
use crate::store::{Embed, Record, Stats, Store};
use crate::walk::{iter_files, Ignore};
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

pub fn one_shot_index(
    store: &mut dyn Store,
    embedder: &dyn Embed,
    root: &Path,
    spec: &Ignore,
) -> Result<Stats> {
    let existing: HashSet<String> = match store.existing_ids() {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("  warning: failed to fetch existing ids ({e}); treating as empty");
            HashSet::new()
        }
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut buffer: Vec<Record> = Vec::new();

    let mut files: usize = 0;
    let mut added: usize = 0;
    let mut unchanged: usize = 0;
    let mut ts_chunks: usize = 0;
    let mut win_chunks: usize = 0;
    let mut skipped_bin: usize = 0;

    for path in iter_files(root, spec) {
        let (_rel, records, ts, win, ok) = chunks_for_file(&path, root);
        if !ok {
            skipped_bin += 1;
            continue;
        }
        files += 1;
        ts_chunks += ts;
        win_chunks += win;

        for record in records {
            seen.insert(record.id.clone());
            if existing.contains(&record.id) {
                unchanged += 1;
            } else {
                buffer.push(record);
                if buffer.len() >= BATCH {
                    added += flush(&mut buffer, store, embedder)?;
                }
            }
        }
    }

    added += flush(&mut buffer, store, embedder)?;

    let stale: Vec<String> = existing.difference(&seen).cloned().collect();
    let mut deleted: usize = 0;
    for chunk in stale.chunks(BATCH) {
        deleted += store.delete(chunk)?;
    }

    Ok(Stats {
        files,
        added,
        unchanged,
        deleted,
        ts_chunks,
        win_chunks,
        skipped_bin,
    })
}

fn flush(buffer: &mut Vec<Record>, store: &mut dyn Store, embedder: &dyn Embed) -> Result<usize> {
    if buffer.is_empty() {
        return Ok(0);
    }
    let docs: Vec<String> = buffer.iter().map(|r| r.body.clone()).collect();
    let embeddings = embedder.embed(&docs)?;
    let n = store.add(buffer, &embeddings)?;
    buffer.clear();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunkfile::chunks_for_file as cff;
    use crate::testkit::{FakeEmbed, MockStore};
    use std::fs;

    #[test]
    fn adds_new_keeps_unchanged_deletes_stale() {
        let d = tempfile::tempdir().unwrap();
        let py_path = d.path().join("a.py");
        fs::write(&py_path, "def f():\n    return 1\n").unwrap();

        let (_, real_records, _, _, ok) = cff(&py_path, d.path());
        assert!(ok, "fixture should parse cleanly");
        assert!(
            !real_records.is_empty(),
            "fixture should produce at least one chunk"
        );

        let unchanged_id = real_records[0].id.clone();

        let mut mock = MockStore::new().with_ids([unchanged_id.clone(), "STALE".to_string()]);

        let spec = crate::walk::load_ignore(d.path());
        let stats = one_shot_index(&mut mock, &FakeEmbed, d.path(), &spec).unwrap();

        assert_eq!(stats.files, 1, "files");

        assert!(stats.unchanged >= 1, "unchanged >= 1");

        assert!(
            mock.deleted.contains(&"STALE".to_string()),
            "STALE should be deleted; deleted={:?}",
            mock.deleted
        );
        assert!(stats.deleted >= 1, "deleted >= 1");
    }

    #[test]
    fn binary_file_skipped() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("bin.py"), b"\x00\x01\x02 binary content").unwrap();
        let mut mock = MockStore::new();
        let spec = crate::walk::load_ignore(d.path());
        let stats = one_shot_index(&mut mock, &FakeEmbed, d.path(), &spec).unwrap();
        assert_eq!(stats.skipped_bin, 1);
        assert_eq!(stats.files, 0);
    }

    #[test]
    fn empty_dir_returns_zero_stats() {
        let d = tempfile::tempdir().unwrap();
        let mut mock = MockStore::new();
        let spec = crate::walk::load_ignore(d.path());
        let stats = one_shot_index(&mut mock, &FakeEmbed, d.path(), &spec).unwrap();
        assert_eq!(stats.files, 0);
        assert_eq!(stats.added, 0);
        assert_eq!(stats.deleted, 0);
    }

    #[test]
    fn existing_ids_error_treated_as_empty() {
        struct FailingStore(MockStore);
        impl Store for FailingStore {
            fn heartbeat(&self) -> Result<()> {
                Ok(())
            }
            fn get_or_create(&mut self, n: &str) -> Result<()> {
                self.0.get_or_create(n)
            }
            fn delete_collection(&mut self, n: &str) -> Result<()> {
                self.0.delete_collection(n)
            }
            fn existing_ids(&self) -> Result<HashSet<String>> {
                anyhow::bail!("simulated backend error")
            }
            fn metadatas(&self) -> Result<Vec<(String, crate::store::Meta)>> {
                self.0.metadatas()
            }
            fn add(&mut self, r: &[Record], e: &[Vec<f32>]) -> Result<usize> {
                self.0.add(r, e)
            }
            fn delete(&mut self, ids: &[String]) -> Result<usize> {
                self.0.delete(ids)
            }
            fn count(&self) -> Result<usize> {
                self.0.count()
            }
        }

        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.rs"), "fn x() {}").unwrap();
        let mut store = FailingStore(MockStore::new());
        let spec = crate::walk::load_ignore(d.path());
        let stats = one_shot_index(&mut store, &FakeEmbed, d.path(), &spec).unwrap();
        assert!(
            stats.added >= 1,
            "should add chunks when existing treated as empty"
        );
    }
}
