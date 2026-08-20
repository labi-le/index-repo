//! Live end-to-end GC tests against a real ChromaDB.
//!
//! Gated on `CHROMA_TEST=1` — skips silently otherwise. Requires a live
//! ChromaDB (default host 192.0.2.1:8000, override via `INDEX_REPO_TEST_HOST` /
//! `INDEX_REPO_TEST_PORT`).
//!
//! Two independent mechanisms are exercised:
//!   * collection TTL sweep (`gc_decide`) — unchanged by the manifest rework;
//!   * orphan-chunk sweep (`service::gc_orphans`) — reclaims content chunks no
//!     manifest references, the deferred deletion path that keeps concurrent
//!     checkouts sharing one content collection safe.

use index_repo::chroma::HttpStore;
use index_repo::config::manifest_collection_name;
use index_repo::manifest::{HttpManifest, ManifestStore};
use index_repo::oneshot::one_shot_index;
use index_repo::registry::Registry;
use index_repo::service::{gc_decide, gc_orphans};
use index_repo::store::{Embed, Store};
use index_repo::walk::load_ignore;
use std::collections::HashSet;

const DAY: u64 = 86_400;

/// Zero-vector embedder: these tests exercise storage and GC bookkeeping, not
/// semantic ranking, so real model weights are unnecessary.
struct FakeEmbed;

impl Embed for FakeEmbed {
    fn embed(&self, docs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0_f32; 384]; docs.len()])
    }
}

fn test_host_port() -> (String, u16) {
    let host = std::env::var("INDEX_REPO_TEST_HOST").unwrap_or_else(|_| "192.168.1.2".to_string());
    let port: u16 = std::env::var("INDEX_REPO_TEST_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);
    (host, port)
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{nanos}-{}", std::process::id())
}

fn skip() -> bool {
    if std::env::var("CHROMA_TEST").as_deref() != Ok("1") {
        eprintln!("gc_e2e: skipping (CHROMA_TEST != 1)");
        return true;
    }
    false
}

#[test]
fn gc_e2e_drops_only_stale_marked() {
    if skip() {
        return;
    }

    let (host, port) = test_host_port();
    let now: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ttl = 30 * DAY;
    let suffix = unique_suffix();

    let stale = format!("code-gc-e2e-stale-{suffix}");
    let fresh = format!("code-gc-e2e-fresh-{suffix}");
    let foreign = format!("gc-e2e-foreign-{suffix}");

    let mut store = HttpStore::new(&host, port, false);
    store.heartbeat().expect("chroma unreachable");

    // stale: marked, last indexed 40 days ago → must be GC'd.
    store.get_or_create(&stale).unwrap();
    store.touch_collection(now - 40 * DAY).unwrap();
    // fresh: marked, last indexed now → must survive.
    store.get_or_create(&fresh).unwrap();
    store.touch_collection(now).unwrap();
    // foreign: created but never stamped (no `index_repo` marker) → must survive.
    store.get_or_create(&foreign).unwrap();

    let cols = store.list_collections().unwrap();
    let doomed = gc_decide(&cols, now, ttl);
    assert!(
        doomed.contains(&stale),
        "stale must be doomed; doomed={doomed:?}"
    );
    assert!(!doomed.contains(&fresh), "fresh must survive");
    assert!(
        !doomed.contains(&foreign),
        "foreign (unmarked) must survive"
    );

    // Guard to our own namespace so a real stale collection sharing this
    // ChromaDB is never touched by the test.
    for name in &doomed {
        if name.ends_with(&suffix) {
            store.delete_collection(name).unwrap();
        }
    }

    let after: HashSet<String> = store
        .list_collections()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(!after.contains(&stale), "stale must be gone after sweep");
    assert!(after.contains(&fresh), "fresh must remain");
    assert!(after.contains(&foreign), "foreign must remain");

    let _ = store.delete_collection(&fresh);
    let _ = store.delete_collection(&foreign);
}

#[test]
fn gc_e2e_orphan_reclaimed_referenced_kept() {
    if skip() {
        return;
    }

    let (host, port) = test_host_port();
    let suffix = unique_suffix();
    let content_name = format!("code-gc-e2e-content-{suffix}");
    let manifest_name = manifest_collection_name(&content_name);

    let mut store = HttpStore::new(&host, port, false);
    store.heartbeat().expect("chroma unreachable");
    store.get_or_create(&content_name).unwrap();

    let mut manifest = HttpManifest::new(&host, port, false, &manifest_name);
    manifest.get_or_create().unwrap();

    let dir = tempfile::tempdir().unwrap();
    // Several kept files, one dropped: the orphan share must sit well under the
    // ratio guard rather than exactly on it.
    for i in 0..6 {
        let a = format!("fn keep_a{i}() {{\n    let x = {i};\n    x + 1\n}}\n");
        let b = format!("fn keep_b{i}() {{\n    let y = {i};\n    y * 2\n}}\n");
        std::fs::write(dir.path().join(format!("keep{i}.rs")), a + &b).unwrap();
    }
    std::fs::write(
        dir.path().join("drop.rs"),
        "fn drop_me() {\n    let y = 2;\n    y * 2\n}\n",
    )
    .unwrap();

    let spec = load_ignore(dir.path());
    let roothash = Registry::hash(dir.path());

    let stats = one_shot_index(&mut store, &mut manifest, &FakeEmbed, dir.path(), &spec).unwrap();
    assert!(
        stats.added >= 7,
        "every fixture file should chunk; added={}",
        stats.added
    );

    let content_ids = store.existing_ids().unwrap();
    let all_ids = manifest.read(&roothash).unwrap();
    assert!(
        !content_ids.is_empty(),
        "content collection must hold chunks"
    );
    assert_eq!(
        content_ids, all_ids,
        "fresh single-root index: content ids equal this root's manifest ids"
    );

    // Removing a file re-runs the root and rewrites its manifest to the smaller
    // set; content is left untouched (one_shot never deletes).
    std::fs::remove_file(dir.path().join("drop.rs")).unwrap();
    let restats = one_shot_index(&mut store, &mut manifest, &FakeEmbed, dir.path(), &spec).unwrap();
    assert_eq!(restats.deleted, 0, "one_shot must never delete content");

    let keep_ids = manifest.read(&roothash).unwrap();
    let orphan_ids: HashSet<String> = all_ids.difference(&keep_ids).cloned().collect();
    assert!(
        !orphan_ids.is_empty(),
        "removing drop.rs must leave unreferenced chunks"
    );
    assert!(!keep_ids.is_empty(), "the kept files stay referenced");

    let before_gc = store.existing_ids().unwrap();
    assert!(
        orphan_ids.iter().all(|id| before_gc.contains(id)),
        "orphans still present in content before the sweep"
    );
    assert!(
        keep_ids.iter().all(|id| before_gc.contains(id)),
        "referenced chunks present before the sweep"
    );
    assert!(
        (orphan_ids.len() as f64) < before_gc.len() as f64 * 0.5,
        "fixture must leave the orphan share under the ratio guard: {}/{}",
        orphan_ids.len(),
        before_gc.len()
    );

    let reclaimed = gc_orphans(&mut store, &manifest);
    assert_eq!(
        reclaimed,
        orphan_ids.len(),
        "gc_orphans reclaims exactly the unreferenced chunks"
    );

    let after_gc = store.existing_ids().unwrap();
    assert!(
        orphan_ids.iter().all(|id| !after_gc.contains(id)),
        "orphan chunks deleted by the sweep"
    );
    assert!(
        keep_ids.iter().all(|id| after_gc.contains(id)),
        "referenced chunks survive the sweep"
    );
    assert_eq!(
        after_gc, keep_ids,
        "content collapses to the referenced set"
    );

    let _ = store.delete_collection(&content_name);
    let _ = store.delete_collection(&manifest_name);
}

#[test]
fn gc_e2e_empty_manifest_guard_returns_zero() {
    if skip() {
        return;
    }

    let (host, port) = test_host_port();
    let suffix = unique_suffix();
    let content_name = format!("code-gc-e2e-guard-{suffix}");
    let manifest_name = manifest_collection_name(&content_name);

    let mut store = HttpStore::new(&host, port, false);
    store.heartbeat().expect("chroma unreachable");
    store.get_or_create(&content_name).unwrap();

    let mut manifest = HttpManifest::new(&host, port, false, &manifest_name);
    manifest.get_or_create().unwrap();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn a() {\n    let z = 3;\n    z - 1\n}\n",
    )
    .unwrap();

    let spec = load_ignore(dir.path());
    let roothash = Registry::hash(dir.path());
    one_shot_index(&mut store, &mut manifest, &FakeEmbed, dir.path(), &spec).unwrap();

    let before = store.existing_ids().unwrap();
    assert!(!before.is_empty(), "content collection must be non-empty");

    // Dropping the only manifest empties the live union. The guard must refuse
    // to nuke a not-yet-manifested content collection rather than treat every
    // chunk as an orphan.
    manifest.remove(&roothash).unwrap();
    assert!(
        manifest.all_ids().unwrap().is_empty(),
        "manifest union must be empty for the guard case"
    );

    let reclaimed = gc_orphans(&mut store, &manifest);
    assert_eq!(reclaimed, 0, "empty-manifest guard must reclaim nothing");

    let after = store.existing_ids().unwrap();
    assert_eq!(
        before, after,
        "guard must leave the content collection untouched"
    );

    let _ = store.delete_collection(&content_name);
    let _ = store.delete_collection(&manifest_name);
}
