use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::chunkfile::chunks_for_file;
use crate::config::{EXTS, SPECIAL_NAMES};
use crate::grammar::used_grammars_str;
use crate::oneshot::one_shot_index;
use crate::store::{Embed, Record, Store};
use crate::walk::Ignore;
use anyhow::Result;
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode, Watcher};

// ---------------------------------------------------------------------------
// Public event type
// ---------------------------------------------------------------------------

pub enum Evt {
    Delete,
    Upsert,
}

/// Map a notify `EventKind` to our `Evt`.
///
/// Remove → Delete, Create/Modify → Upsert, everything else (Access/Other/Any) → None.
/// Extracted from `run_daemon` so the service dispatcher reuses identical mapping.
pub fn evt_for(kind: &EventKind) -> Option<Evt> {
    match kind {
        EventKind::Remove(_) => Some(Evt::Delete),
        EventKind::Create(_) | EventKind::Modify(_) => Some(Evt::Upsert),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// _safe: run a store Result, swallow on Err with exact spec §10.4 message
// ---------------------------------------------------------------------------

/// Call a store operation; on failure log exactly "daemon: chromadb call failed ({e})"
/// and return `None`. Mirrors Python `_safe`.
macro_rules! safe {
    ($call:expr) => {
        match $call {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("daemon: chromadb call failed ({e})");
                None
            }
        }
    };
}

// ---------------------------------------------------------------------------
// build_path_to_ids  (Python _build_path_to_ids, lines 508-527)
// ---------------------------------------------------------------------------

/// Reconstruct `path → {chunk ids}` from the collection's own metadata.
///
/// On error, logs "daemon: failed to load existing metadata ({e})" and returns
/// an empty map (daemon continues with empty state).
pub fn build_path_to_ids(store: &dyn Store) -> HashMap<String, HashSet<String>> {
    let mut mapping: HashMap<String, HashSet<String>> = HashMap::new();
    let pairs = match store.metadatas() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("daemon: failed to load existing metadata ({e})");
            return mapping;
        }
    };
    for (id, meta) in pairs {
        if !meta.path.is_empty() {
            mapping.entry(meta.path).or_default().insert(id);
        }
    }
    mapping
}

// ---------------------------------------------------------------------------
// process_changes  (Python _process_changes, lines 530-597)
// ---------------------------------------------------------------------------

/// Apply one debounced batch of filesystem events as a per-file delta.
///
/// Returns `(added, deleted)`.
/// Every store call is wrapped in `safe!` — failures are logged and swallowed.
pub fn process_changes(
    store: &mut dyn Store,
    embedder: &dyn Embed,
    root: &Path,
    changes: &[(Evt, PathBuf)],
    path_to_ids: &mut HashMap<String, HashSet<String>>,
    all_ids: &mut HashSet<String>,
) -> (usize, usize) {
    // Step 1: collapse batch into a per-rel action map (Delete wins).
    let mut actions: HashMap<String, Evt> = HashMap::new();
    let mut paths: HashMap<String, PathBuf> = HashMap::new();

    for (evt, path) in changes {
        let rel = match path.strip_prefix(root) {
            Ok(r) => posix_str(r),
            Err(_) => continue, // outside root
        };
        paths.insert(rel.clone(), path.clone());
        match evt {
            Evt::Delete => {
                actions.insert(rel, Evt::Delete);
            }
            Evt::Upsert => {
                // Only set Upsert if we don't already have a Delete for this rel
                if !matches!(actions.get(&rel), Some(Evt::Delete)) {
                    actions.insert(rel, Evt::Upsert);
                }
            }
        }
    }

    let mut added: usize = 0;
    let mut deleted: usize = 0;

    // Step 2: process each rel
    for (rel, action) in &actions {
        let path = &paths[rel];

        let is_delete = matches!(action, Evt::Delete) || !path.exists();

        if is_delete {
            let old: HashSet<String> = path_to_ids.remove(rel).unwrap_or_default();
            if !old.is_empty() {
                let old_vec: Vec<String> = old.iter().cloned().collect();
                safe!(store.delete(&old_vec));
                for id in &old {
                    all_ids.remove(id);
                }
                deleted += old.len();
            }
            continue;
        }

        // Upsert: recompute chunks
        let (_rel2, records, _ts, _win, ok) = chunks_for_file(path, root);
        if !ok {
            continue; // binary file slipped through
        }

        let seen: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();

        // Stale: ids previously in this path that are no longer present
        let stale: Vec<String> = path_to_ids
            .get(rel)
            .map(|old| old.difference(&seen).cloned().collect())
            .unwrap_or_default();
        if !stale.is_empty() {
            safe!(store.delete(&stale));
            for id in &stale {
                all_ids.remove(id);
            }
            deleted += stale.len();
        }

        // New: records not yet in all_ids
        let new_records: Vec<Record> = records
            .into_iter()
            .filter(|r| !all_ids.contains(&r.id))
            .collect();
        if !new_records.is_empty() {
            let docs: Vec<String> = new_records.iter().map(|r| r.body.clone()).collect();
            match embedder.embed(&docs) {
                Ok(embeddings) => {
                    let _ = safe!(store.add(&new_records, &embeddings));
                }
                Err(e) => {
                    eprintln!("daemon: chromadb call failed ({e})");
                }
            }
            // State updates are UNCONDITIONAL (mirrors Python which updates all_ids/added
            // regardless of whether col.add succeeded — the call is fire-and-forget via _safe)
            for r in &new_records {
                all_ids.insert(r.id.clone());
            }
            added += new_records.len();
        }

        path_to_ids.insert(rel.clone(), seen);
    }

    if added > 0 || deleted > 0 {
        // Exact spec §10.4 message (em dash —)
        eprintln!(
            "daemon: live update \u{2014} added={added} deleted={deleted} chunks={}",
            all_ids.len()
        );
    }

    (added, deleted)
}

// ---------------------------------------------------------------------------
// watch_keep  (Python _make_watch_filter, lines 600-619)
// ---------------------------------------------------------------------------

/// Return true if this path's events should be processed.
///
/// Mirrors Python's `_make_watch_filter`: ext/special-name check + ignore check.
/// NO size check, NO existence check (intentional — lets Delete events through).
pub fn watch_keep(root: &Path, spec: &Ignore, path: &Path) -> bool {
    // Must be inside root
    let rel = match path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let rel_posix = posix_str(rel);
    let rel_path = Path::new(&rel_posix);

    // Must not be ignored
    if spec.is_ignored(rel_path) {
        return false;
    }

    // Extension or special name check
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    let ext_lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    EXTS.contains(&ext_lower.as_str()) || SPECIAL_NAMES.contains(&file_name)
}

// ---------------------------------------------------------------------------
// run_daemon  (Python daemon_main, lines 622-675)
// ---------------------------------------------------------------------------

pub fn run_daemon(
    store: &mut dyn Store,
    embedder: &dyn Embed,
    root: &Path,
    spec: &Ignore,
    debounce_ms: u64,
) -> Result<i32> {
    // 1. Announce initial sync
    eprintln!("daemon: initial sync of {}", root.display());

    // 2. Run one-shot incremental index
    let stats = one_shot_index(store, embedder, root, spec)?;

    // 3. Build path→ids from collection metadata
    let mut path_to_ids = build_path_to_ids(store);
    let mut all_ids: HashSet<String> = path_to_ids.values().flatten().cloned().collect();

    // 4. Record grammars used during initial sync
    let grammars = used_grammars_str();

    // 5. Initial sync summary (exact spec §10.4)
    eprintln!(
        "daemon: initial sync done \u{2014} files={} added={} unchanged={} deleted={} chunks={} grammars={}",
        stats.files, stats.added, stats.unchanged, stats.deleted, all_ids.len(), grammars
    );

    // 6. Signal handlers — set stop flag on SIGTERM / SIGINT / SIGHUP
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        // Ignore registration errors (e.g. some signals can't be caught)
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }

    // 7. Set up debouncer with mpsc channel
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = match new_debouncer(Duration::from_millis(debounce_ms), None, tx) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("daemon: watch loop crashed ({e})");
            return Ok(4);
        }
    };

    if let Err(e) = debouncer.watcher().watch(root, RecursiveMode::Recursive) {
        eprintln!("daemon: watch loop crashed ({e})");
        return Ok(4);
    }

    // 8. Announce watching
    eprintln!(
        "daemon: watching {} (debounce={debounce_ms}ms)",
        root.display()
    );

    // 9. Watch loop
    let exit_code = loop {
        if stop.load(Ordering::Relaxed) {
            break 0;
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                // Map debounced events → (Evt, PathBuf) list
                let changes: Vec<(Evt, PathBuf)> = events
                    .iter()
                    .flat_map(|debounced_event| {
                        let kind = debounced_event.kind;
                        debounced_event.paths.iter().filter_map(move |path| {
                            let evt = evt_for(&kind)?; // Access/Other/Any → ignore
                            if watch_keep(root, spec, path) {
                                Some((evt, path.clone()))
                            } else {
                                None
                            }
                        })
                    })
                    .collect();

                if !changes.is_empty() {
                    process_changes(
                        store,
                        embedder,
                        root,
                        &changes,
                        &mut path_to_ids,
                        &mut all_ids,
                    );
                }
            }
            Ok(Err(errors)) => {
                for e in errors {
                    eprintln!("daemon: watch loop crashed ({e})");
                }
                return Ok(4);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Normal — check stop flag and loop
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed — exit
                break 0;
            }
        }
    };

    // 10. Normal stop
    eprintln!("daemon: stopped");
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn posix_str(p: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        p.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(target_os = "windows"))]
    {
        p.to_string_lossy().into_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunkfile::chunks_for_file as cff;
    use crate::store::Meta;
    use crate::testkit::{FakeEmbed, MockStore};
    use std::fs;

    fn setup_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    // ---- build_path_to_ids ----

    #[test]
    fn build_path_to_ids_groups_by_path() {
        let mut mock = MockStore::new();
        // Seed two ids on "src/a.rs" and one on "src/b.rs"
        mock.metas = vec![
            (
                "id1".to_string(),
                Meta {
                    path: "src/a.rs".to_string(),
                    line: 1,
                    lang: "rs".to_string(),
                    node_type: "function_item".to_string(),
                    scope: String::new(),
                },
            ),
            (
                "id2".to_string(),
                Meta {
                    path: "src/a.rs".to_string(),
                    line: 10,
                    lang: "rs".to_string(),
                    node_type: "function_item".to_string(),
                    scope: String::new(),
                },
            ),
            (
                "id3".to_string(),
                Meta {
                    path: "src/b.rs".to_string(),
                    line: 1,
                    lang: "rs".to_string(),
                    node_type: "struct_item".to_string(),
                    scope: String::new(),
                },
            ),
        ];

        let map = build_path_to_ids(&mock);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map["src/a.rs"],
            HashSet::from(["id1".to_string(), "id2".to_string()])
        );
        assert_eq!(map["src/b.rs"], HashSet::from(["id3".to_string()]));
    }

    // ---- process_changes: delete ----

    #[test]
    fn delete_then_upsert_delta() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        // Get the real ids the chunker produces
        let (_, initial_records, _, _, ok) = cff(&py_path, d.path());
        assert!(ok);
        assert!(!initial_records.is_empty());

        let initial_ids: HashSet<String> = initial_records.iter().map(|r| r.id.clone()).collect();

        // Seed state: a.py has those ids
        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("a.py".to_string(), initial_ids.clone());
        let mut all_ids = initial_ids.clone();

        let mut mock = MockStore::new().with_ids(initial_ids.clone());

        // --- Phase 1: delete a.py ---
        let changes = vec![(Evt::Delete, py_path.clone())];
        let _spec = crate::walk::load_ignore(d.path());
        let (added, deleted) = process_changes(
            &mut mock,
            &FakeEmbed,
            d.path(),
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert_eq!(added, 0, "delete should not add");
        assert_eq!(
            deleted,
            initial_ids.len(),
            "all original ids should be deleted"
        );
        assert!(
            !path_to_ids.contains_key("a.py"),
            "a.py removed from path_to_ids"
        );
        assert!(all_ids.is_empty(), "all_ids should be empty");
        for id in &initial_ids {
            assert!(
                mock.deleted.contains(id),
                "id {id} should be in mock.deleted"
            );
        }

        // --- Phase 2: upsert a.py with different content ---
        fs::write(&py_path, "def g():\n    return 2\n").unwrap();
        let changes2 = vec![(Evt::Upsert, py_path.clone())];
        let (added2, deleted2) = process_changes(
            &mut mock,
            &FakeEmbed,
            d.path(),
            &changes2,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert!(added2 >= 1, "upsert should add at least 1 chunk");
        // (deleted2 may be 0 if path_to_ids was empty going in)
        let _ = deleted2;
        assert!(
            path_to_ids.contains_key("a.py"),
            "a.py re-added to path_to_ids"
        );
        // all_ids should match the new seen set
        let new_seen = path_to_ids["a.py"].clone();
        for id in &new_seen {
            assert!(all_ids.contains(id), "id {id} should be in all_ids");
        }
    }

    // ---- Delete wins over Upsert in same batch ----

    #[test]
    fn delete_wins_over_upsert_same_batch() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        // Give the file existing state
        let (_, records, _, _, _) = cff(&py_path, d.path());
        let ids: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();
        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("a.py".to_string(), ids.clone());
        let mut all_ids = ids.clone();
        let mut mock = MockStore::new().with_ids(ids.clone());

        // Send both Upsert and Delete in the same batch
        let changes = vec![
            (Evt::Upsert, py_path.clone()),
            (Evt::Delete, py_path.clone()),
        ];
        let (added, _deleted) = process_changes(
            &mut mock,
            &FakeEmbed,
            d.path(),
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        // Net effect must be delete (delete wins)
        assert_eq!(added, 0, "delete wins — nothing should be added");
        assert!(
            !path_to_ids.contains_key("a.py"),
            "a.py should be gone from path_to_ids"
        );
        assert!(
            all_ids.is_empty(),
            "all_ids should be empty after delete wins"
        );
    }

    // ---- evt_for ----

    #[test]
    fn evt_for_mapping_unchanged() {
        use notify_debouncer_full::notify::event::{CreateKind, ModifyKind, RemoveKind};
        assert!(matches!(
            evt_for(&EventKind::Remove(RemoveKind::File)),
            Some(Evt::Delete)
        ));
        assert!(matches!(
            evt_for(&EventKind::Create(CreateKind::File)),
            Some(Evt::Upsert)
        ));
        assert!(matches!(
            evt_for(&EventKind::Modify(ModifyKind::Any)),
            Some(Evt::Upsert)
        ));
        assert!(evt_for(&EventKind::Access(
            notify_debouncer_full::notify::event::AccessKind::Any
        ))
        .is_none());
    }

    // ---- watch_keep ----

    #[test]
    fn watch_keep_filters() {
        let d = tempfile::tempdir().unwrap();
        let spec = crate::walk::load_ignore(d.path());

        // Indexable extension → keep
        assert!(watch_keep(d.path(), &spec, &d.path().join("foo.rs")));
        // Non-indexable extension → drop
        assert!(!watch_keep(d.path(), &spec, &d.path().join("image.png")));
        // Special name → keep
        assert!(watch_keep(d.path(), &spec, &d.path().join("Makefile")));
        // Outside root → drop
        assert!(!watch_keep(d.path(), &spec, Path::new("/tmp/outside.rs")));
        // No size check: a path that doesn't exist is still kept (delete events)
        assert!(watch_keep(d.path(), &spec, &d.path().join("ghost.rs")));
    }
}
