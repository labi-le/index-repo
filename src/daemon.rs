use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::chunkfile::chunks_for_file;
use crate::config::{EXTS, SPECIAL_NAMES};
use crate::grammar::used_grammars_str;
use crate::manifest::ManifestStore;
use crate::oneshot::one_shot_index;
use crate::registry::Registry;
use crate::store::{Embed, Record, Store};
use crate::walk::Ignore;
use anyhow::Result;
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{EventKind, RecursiveMode, Watcher};

const RESYNC_INTERVAL: Duration = Duration::from_secs(45 * 60);

pub enum Evt {
    Delete,
    Upsert,
    Resync,
}

/// Map a notify `EventKind` to our `Evt`.
/// Remove → Delete, Create/Modify → Upsert, everything else (Access/Other/Any) → None.
/// Extracted from `run_daemon` so the service dispatcher reuses identical mapping.
pub fn evt_for(kind: &EventKind) -> Option<Evt> {
    match kind {
        EventKind::Remove(_) => Some(Evt::Delete),
        EventKind::Create(_) | EventKind::Modify(_) => Some(Evt::Upsert),
        _ => None,
    }
}

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

/// Reconstruct `path → {chunk ids}` from the collection's own metadata.
///
/// On error, logs "daemon: failed to load existing metadata ({e})" and returns
/// an empty map (daemon continues with empty state).
pub fn build_path_to_ids(
    store: &dyn Store,
    my_ids: &HashSet<String>,
) -> HashMap<String, HashSet<String>> {
    let mut mapping: HashMap<String, HashSet<String>> = HashMap::new();
    let pairs = match store.metadatas() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("daemon: failed to load existing metadata ({e})");
            return mapping;
        }
    };
    for (id, meta) in pairs {
        if my_ids.contains(&id) && !meta.path.is_empty() {
            mapping.entry(meta.path).or_default().insert(id);
        }
    }
    mapping
}

/// Apply one debounced batch of filesystem events as a per-file delta.
///
/// Content is never deleted; removals only drop this root's manifest references,
/// and the manifest is rewritten as a superset before any content add so a
/// concurrent orphan sweep never deletes an added-but-unreferenced chunk. A
/// delete event prunes the exact path and every path beneath it (directory
/// removal). Returns `(added, deleted_refs)`.
#[allow(clippy::too_many_arguments)]
pub fn process_changes(
    store: &mut dyn Store,
    manifest: &mut dyn ManifestStore,
    embedder: &dyn Embed,
    root: &Path,
    roothash: &str,
    changes: &[(Evt, PathBuf)],
    path_to_ids: &mut HashMap<String, HashSet<String>>,
    all_ids: &mut HashSet<String>,
) -> (usize, usize) {
    let mut actions: HashMap<String, Evt> = HashMap::new();
    let mut paths: HashMap<String, PathBuf> = HashMap::new();

    for (evt, path) in changes {
        let rel = match path.strip_prefix(root) {
            Ok(r) => posix_str(r),
            Err(_) => continue,
        };
        paths.insert(rel.clone(), path.clone());
        match evt {
            Evt::Delete => {
                actions.insert(rel, Evt::Delete);
            }
            Evt::Upsert => {
                if !matches!(actions.get(&rel), Some(Evt::Delete)) {
                    actions.insert(rel, Evt::Upsert);
                }
            }
            Evt::Resync => {}
        }
    }

    let mut target = all_ids.clone();
    let mut removed_rels: Vec<String> = Vec::new();
    let mut new_path_ids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut new_records: Vec<Record> = Vec::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut deleted: usize = 0;

    for (rel, action) in &actions {
        let path = &paths[rel];
        let is_delete = matches!(action, Evt::Delete) || !path.exists();

        if is_delete {
            let prefix = format!("{rel}/");
            let matched: Vec<String> = path_to_ids
                .keys()
                .filter(|k| k.as_str() == rel || k.starts_with(&prefix))
                .cloned()
                .collect();
            for k in matched {
                if let Some(ids) = path_to_ids.get(&k) {
                    for id in ids {
                        if target.remove(id) {
                            deleted += 1;
                        }
                    }
                }
                removed_rels.push(k);
            }
            continue;
        }

        let (_rel2, records, _ts, _win, ok) = chunks_for_file(path, root);
        if !ok {
            continue;
        }
        let seen: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();
        if let Some(old) = path_to_ids.get(rel) {
            for id in old.difference(&seen) {
                if target.remove(id) {
                    deleted += 1;
                }
            }
        }
        for r in records {
            target.insert(r.id.clone());
            if !all_ids.contains(&r.id) && queued.insert(r.id.clone()) {
                new_records.push(r);
            }
        }
        new_path_ids.insert(rel.clone(), seen);
    }

    if new_records.is_empty() && deleted == 0 && removed_rels.is_empty() {
        return (0, 0);
    }

    let root_tag = root.to_string_lossy();
    if safe!(manifest.write(roothash, &root_tag, &target)).is_none() {
        return (0, 0);
    }

    let mut added: usize = 0;
    if !new_records.is_empty() {
        let docs: Vec<String> = new_records.iter().map(|r| r.body.clone()).collect();
        match embedder.embed(&docs) {
            Ok(embeddings) => {
                if safe!(store.add(&new_records, &embeddings)).is_some() {
                    added = new_records.len();
                }
            }
            Err(e) => eprintln!("daemon: embedding failed ({e})"),
        }
    }

    *all_ids = target;
    for k in removed_rels {
        path_to_ids.remove(&k);
    }
    for (rel, ids) in new_path_ids {
        path_to_ids.insert(rel, ids);
    }

    if added > 0 || deleted > 0 {
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

/// Whether a filesystem event should be applied. Deletes bypass the
/// extension/ignore filter so removals of ignored paths and whole directories
/// still prune this root's manifest; upserts keep the file-selection filter.
pub fn keep_event(root: &Path, spec: &Ignore, path: &Path, evt: &Evt) -> bool {
    match evt {
        Evt::Delete => path.starts_with(root),
        Evt::Upsert => watch_keep(root, spec, path),
        Evt::Resync => true,
    }
}

// ---------------------------------------------------------------------------
// run_daemon  (Python daemon_main, lines 622-675)
// ---------------------------------------------------------------------------

pub fn run_daemon(
    store: &mut dyn Store,
    manifest: &mut dyn ManifestStore,
    embedder: &dyn Embed,
    root: &Path,
    spec: &Ignore,
    debounce_ms: u64,
) -> Result<i32> {
    eprintln!("daemon: initial sync of {}", root.display());

    let roothash = Registry::hash(root);
    let stats = one_shot_index(store, manifest, embedder, root, spec)?;

    let my_ids = manifest.read(&roothash).unwrap_or_default();
    let mut path_to_ids = build_path_to_ids(store, &my_ids);
    let mut all_ids: HashSet<String> = my_ids;

    let grammars = used_grammars_str();
    eprintln!(
        "daemon: initial sync done \u{2014} files={} added={} unchanged={} deleted={} chunks={} grammars={}",
        stats.files, stats.added, stats.unchanged, stats.deleted, all_ids.len(), grammars
    );

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }

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

    eprintln!(
        "daemon: watching {} (debounce={debounce_ms}ms)",
        root.display()
    );

    let mut last_resync = Instant::now();
    let exit_code = loop {
        if stop.load(Ordering::Relaxed) {
            break 0;
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                let changes: Vec<(Evt, PathBuf)> = events
                    .iter()
                    .flat_map(|debounced_event| {
                        let kind = debounced_event.kind;
                        debounced_event.paths.iter().filter_map(move |path| {
                            let evt = evt_for(&kind)?;
                            if keep_event(root, spec, path, &evt) {
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
                        manifest,
                        embedder,
                        root,
                        &roothash,
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
                if last_resync.elapsed() >= RESYNC_INTERVAL {
                    if one_shot_index(store, manifest, embedder, root, spec).is_ok() {
                        let my_ids = manifest.read(&roothash).unwrap_or_default();
                        path_to_ids = build_path_to_ids(store, &my_ids);
                        all_ids = my_ids;
                    }
                    last_resync = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 0,
        }
    };

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
    use crate::testkit::{FakeEmbed, MockManifest, MockStore};
    use std::fs;

    const RH: &str = "rh";

    fn setup_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    // ---- build_path_to_ids ----

    #[test]
    fn build_path_to_ids_groups_by_path() {
        let mut mock = MockStore::new();
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

        // Only this root's ids are grouped; whole-collection ids outside the set
        // (id3) are skipped so a shared collection never leaks a sibling's paths.
        let my_ids = HashSet::from(["id1".to_string(), "id2".to_string()]);
        let map = build_path_to_ids(&mock, &my_ids);
        assert_eq!(map.len(), 1);
        assert_eq!(
            map["src/a.rs"],
            HashSet::from(["id1".to_string(), "id2".to_string()])
        );
        assert!(
            !map.contains_key("src/b.rs"),
            "id3 is not in my_ids, so src/b.rs is not this root's"
        );
    }

    // ---- process_changes: delete never touches shared content ----

    #[test]
    fn delete_then_upsert_delta() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        let (_, initial_records, _, _, ok) = cff(&py_path, d.path());
        assert!(ok);
        assert!(!initial_records.is_empty());

        let initial_ids: HashSet<String> = initial_records.iter().map(|r| r.id.clone()).collect();

        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("a.py".to_string(), initial_ids.clone());
        let mut all_ids = initial_ids.clone();

        let mut mock = MockStore::new().with_ids(initial_ids.clone());
        let mut manifest = MockManifest::default();
        manifest.sets.insert(RH.to_string(), initial_ids.clone());

        // --- Phase 1: delete a.py ---
        let changes = vec![(Evt::Delete, py_path.clone())];
        let (added, deleted) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert_eq!(added, 0, "delete should not add");
        assert_eq!(
            deleted,
            initial_ids.len(),
            "all original refs should be dropped"
        );
        assert!(
            !path_to_ids.contains_key("a.py"),
            "a.py removed from path_to_ids"
        );
        assert!(all_ids.is_empty(), "all_ids should be empty");

        // A per-root prune only rewrites this root's manifest; shared content
        // chunks are reclaimed later by the single-threaded orphan sweep.
        assert!(
            mock.deleted.is_empty(),
            "content must never be deleted on a per-root prune"
        );
        let manifest_ids = manifest.read(RH).unwrap();
        assert!(
            manifest_ids.is_empty(),
            "root manifest no longer references the pruned ids"
        );
        for id in &initial_ids {
            assert!(
                mock.ids.contains(id),
                "content chunk {id} survives in store"
            );
        }

        // --- Phase 2: upsert a.py with different content ---
        fs::write(&py_path, "def g():\n    return 2\n").unwrap();
        let changes2 = vec![(Evt::Upsert, py_path.clone())];
        let (added2, _deleted2) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes2,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert!(added2 >= 1, "upsert should add at least 1 chunk");
        assert!(
            path_to_ids.contains_key("a.py"),
            "a.py re-added to path_to_ids"
        );
        let new_seen = path_to_ids["a.py"].clone();
        let manifest_ids2 = manifest.read(RH).unwrap();
        for id in &new_seen {
            assert!(all_ids.contains(id), "id {id} should be in all_ids");
            assert!(manifest_ids2.contains(id), "id {id} recorded in manifest");
            assert!(mock.ids.contains(id), "id {id} added to content store");
        }
    }

    // ---- Delete wins over Upsert in same batch ----

    #[test]
    fn delete_wins_over_upsert_same_batch() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        let (_, records, _, _, _) = cff(&py_path, d.path());
        let ids: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();
        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("a.py".to_string(), ids.clone());
        let mut all_ids = ids.clone();
        let mut mock = MockStore::new().with_ids(ids.clone());
        let mut manifest = MockManifest::default();
        manifest.sets.insert(RH.to_string(), ids.clone());

        let changes = vec![
            (Evt::Upsert, py_path.clone()),
            (Evt::Delete, py_path.clone()),
        ];
        let (added, _deleted) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert_eq!(added, 0, "delete wins — nothing should be added");
        assert!(
            !path_to_ids.contains_key("a.py"),
            "a.py should be gone from path_to_ids"
        );
        assert!(
            all_ids.is_empty(),
            "all_ids should be empty after delete wins"
        );
        assert!(
            manifest.read(RH).unwrap().is_empty(),
            "manifest emptied after delete wins"
        );
        assert!(mock.deleted.is_empty(), "content never deleted on prune");
    }

    // ---- Delete of a directory prunes the whole subtree ----

    #[test]
    fn delete_event_prunes_directory_subtree() {
        let d = tempfile::tempdir().unwrap();
        let id_a = "ida".to_string();
        let id_b = "idb".to_string();

        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("sub/a.rs".to_string(), HashSet::from([id_a.clone()]));
        path_to_ids.insert("sub/b.rs".to_string(), HashSet::from([id_b.clone()]));
        let mut all_ids = HashSet::from([id_a.clone(), id_b.clone()]);

        let mut mock = MockStore::new().with_ids(all_ids.clone());
        let mut manifest = MockManifest::default();
        manifest.sets.insert(RH.to_string(), all_ids.clone());

        let changes = vec![(Evt::Delete, d.path().join("sub"))];
        let (added, deleted) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert_eq!(added, 0, "directory delete adds nothing");
        assert_eq!(deleted, 2, "both ids beneath sub/ are pruned");
        assert!(!all_ids.contains(&id_a));
        assert!(!all_ids.contains(&id_b));
        assert!(!path_to_ids.contains_key("sub/a.rs"));
        assert!(!path_to_ids.contains_key("sub/b.rs"));
        assert!(
            manifest.read(RH).unwrap().is_empty(),
            "manifest empty after subtree prune"
        );
        assert!(mock.deleted.is_empty(), "content never deleted on prune");
    }

    // ---- Upsert after a rechunk drops the stale ids from this root ----

    #[test]
    fn upsert_rechunk_drops_stale_ids() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        let (_, records, _, _, _) = cff(&py_path, d.path());
        let old_ids: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();
        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        path_to_ids.insert("a.py".to_string(), old_ids.clone());
        let mut all_ids = old_ids.clone();

        let mut mock = MockStore::new().with_ids(old_ids.clone());
        let mut manifest = MockManifest::default();
        manifest.sets.insert(RH.to_string(), old_ids.clone());

        // Rewrite the file so the chunker yields different ids.
        fs::write(&py_path, "def g():\n    return 2\n").unwrap();
        let changes = vec![(Evt::Upsert, py_path.clone())];
        let (added, deleted) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert!(added >= 1, "the rechunked content is added");
        assert!(deleted >= 1, "the stale ids are dropped from this root");

        let new_ids = path_to_ids["a.py"].clone();
        let manifest_ids = manifest.read(RH).unwrap();
        for id in old_ids.difference(&new_ids) {
            assert!(!all_ids.contains(id), "stale id {id} gone from all_ids");
            assert!(
                !manifest_ids.contains(id),
                "stale id {id} gone from manifest"
            );
        }
        for id in &new_ids {
            assert!(all_ids.contains(id), "new id {id} present in all_ids");
            assert!(manifest_ids.contains(id), "new id {id} present in manifest");
            assert!(mock.ids.contains(id), "new id {id} added to content store");
        }
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

        assert!(watch_keep(d.path(), &spec, &d.path().join("foo.rs")));
        assert!(!watch_keep(d.path(), &spec, &d.path().join("image.png")));
        assert!(watch_keep(d.path(), &spec, &d.path().join("Makefile")));
        assert!(!watch_keep(d.path(), &spec, Path::new("/tmp/outside.rs")));
        assert!(watch_keep(d.path(), &spec, &d.path().join("ghost.rs")));
    }

    // ---- keep_event: deletes bypass the file-selection filter ----

    #[test]
    fn keep_event_lets_deletes_bypass_filter() {
        let d = tempfile::tempdir().unwrap();
        let spec = crate::walk::load_ignore(d.path());

        // An extensionless directory path is rejected by the upsert filter.
        let dir = d.path().join("sub");
        assert!(
            !watch_keep(d.path(), &spec, &dir),
            "precondition: upsert filter rejects the directory path"
        );
        assert!(
            keep_event(d.path(), &spec, &dir, &Evt::Delete),
            "deletes bypass the filter so directory removals still prune"
        );
        assert!(
            !keep_event(d.path(), &spec, &dir, &Evt::Upsert),
            "upserts keep the file-selection filter"
        );
        assert!(
            keep_event(d.path(), &spec, &dir, &Evt::Resync),
            "resync always passes"
        );
    }

    // ---- process_changes: a failed add lands no content ----

    #[test]
    fn failed_add_records_membership_but_lands_no_content() {
        let d = tempfile::tempdir().unwrap();
        let py_path = setup_file(d.path(), "a.py", "def f():\n    return 1\n");

        let mut path_to_ids: HashMap<String, HashSet<String>> = HashMap::new();
        let mut all_ids: HashSet<String> = HashSet::new();

        // The manifest is written as an optimistic superset before the content
        // add, so a concurrent orphan sweep never reclaims a chunk mid-insert;
        // a failed add lands no content and is reconciled by resync, not retry.
        let mut mock = MockStore::new().with_failing_add();
        let mut manifest = MockManifest::default();
        let changes = vec![(Evt::Upsert, py_path.clone())];
        let (added, _deleted) = process_changes(
            &mut mock,
            &mut manifest,
            &FakeEmbed,
            d.path(),
            RH,
            &changes,
            &mut path_to_ids,
            &mut all_ids,
        );

        assert_eq!(
            added, 0,
            "a failed add contributes nothing to the add count"
        );
        assert!(mock.ids.is_empty(), "no content lands when the add fails");

        let seen = path_to_ids.get("a.py").cloned().unwrap_or_default();
        assert!(!seen.is_empty(), "membership was recorded before the add");
        let manifest_ids = manifest.read(RH).unwrap();
        for id in &seen {
            assert!(manifest_ids.contains(id), "id {id} pre-written to manifest");
            assert!(all_ids.contains(id), "id {id} recorded in all_ids");
        }
    }
}
