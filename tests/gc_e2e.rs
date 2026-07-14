//! Live end-to-end test for TTL collection GC against a real ChromaDB.
//!
//! Gated on `CHROMA_TEST=1` — skips silently otherwise. Requires a live
//! ChromaDB at 192.168.1.2:8000 (override via `INDEX_REPO_TEST_HOST` /
//! `INDEX_REPO_TEST_PORT`). Exercises the real HTTP path the serve daemon's GC
//! sweep uses: `list_collections` (parse), `touch_collection` (PUT metadata),
//! `delete_collection` (DELETE by name), and the pure `gc_decide`.

use index_repo::chroma::HttpStore;
use index_repo::service::gc_decide;
use index_repo::store::Store;

const DAY: u64 = 86_400;

#[test]
fn gc_e2e_drops_only_stale_marked() {
    if std::env::var("CHROMA_TEST").as_deref() != Ok("1") {
        eprintln!("gc_e2e: skipping (CHROMA_TEST != 1)");
        return;
    }

    let host = std::env::var("INDEX_REPO_TEST_HOST").unwrap_or_else(|_| "192.168.1.2".to_string());
    let port: u16 = std::env::var("INDEX_REPO_TEST_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8000);

    let now: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ttl = 30 * DAY;
    let suffix = format!("{}-{}", now, std::process::id());

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

    // Decide from the live listing.
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

    // Drop the doomed — guarded to our own test namespace so a real stale
    // collection sharing this ChromaDB is never touched by the test.
    for name in &doomed {
        if name.ends_with(&suffix) {
            store.delete_collection(name).unwrap();
        }
    }

    let after: std::collections::HashSet<String> = store
        .list_collections()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(!after.contains(&stale), "stale must be gone after sweep");
    assert!(after.contains(&fresh), "fresh must remain");
    assert!(after.contains(&foreign), "foreign must remain");

    // Cleanup.
    let _ = store.delete_collection(&fresh);
    let _ = store.delete_collection(&foreign);
}
