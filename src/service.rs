//! Singleton always-on service: one dispatcher + N per-root actors sharing a
//! single lazily-loaded embedding model.
//!
//! Pure orchestration layer. Reuses the parity-critical core unchanged:
//! `one_shot_index`, `process_changes`, `build_path_to_ids`, `watch_keep`,
//! `evt_for`. See `docs/service-design.md`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::{RecursiveMode, Watcher};

use crate::chroma::HttpStore;
use crate::daemon::{build_path_to_ids, evt_for, process_changes, watch_keep, Evt};
use crate::lazy::LazyEmbedder;
use crate::oneshot::one_shot_index;
use crate::registry::Registry;
use crate::store::Store;
use crate::walk::{load_ignore, Ignore};

const GC_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Conn {
    host: String,
    port: u16,
    ssl: bool,
}

/// Longest canonical-prefix match: return the deepest root that is a path
/// prefix of `path`. `None` if `path` lives under no root.
///
/// `Path::starts_with` is component-wise, so `/a/bb` is NOT a prefix of
/// `/a/b/c` — only true ancestors match. Among matches the deepest (most
/// components) wins, which is the owning root for nested roots.
pub fn route<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
}

/// All roots that are path-prefix ancestors of `path` (component-wise).
///
/// Unlike [`route`], this returns EVERY matching root so an edit under a
/// nested layout (`/repo` and `/repo/sub` both registered) updates each
/// owning collection, not just the deepest one.
pub fn route_all<'a>(path: &Path, roots: &'a [PathBuf]) -> Vec<&'a PathBuf> {
    roots.iter().filter(|r| path.starts_with(r)).collect()
}

/// If `path` is a registered root's OWN `.gitignore`, return that root.
///
/// Only a root's direct `.gitignore` triggers a live reload; nested
/// `.gitignore`s are intentionally ignored to preserve single-file selection
/// parity with Python (spec §5.1).
pub fn gitignore_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    if path.file_name().and_then(|n| n.to_str()) != Some(".gitignore") {
        return None;
    }
    let parent = path.parent()?;
    roots.iter().find(|r| r.as_path() == parent)
}

/// Compute `(to_start, to_stop)` between the desired root set and the currently
/// running set. `to_start` preserves `desired` order; `to_stop` is the current
/// roots no longer desired.
pub fn reconcile_diff(
    desired: &[PathBuf],
    current: &HashSet<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let desired_set: HashSet<&PathBuf> = desired.iter().collect();
    let to_start: Vec<PathBuf> = desired
        .iter()
        .filter(|d| !current.contains(*d))
        .cloned()
        .collect();
    let to_stop: Vec<PathBuf> = current
        .iter()
        .filter(|c| !desired_set.contains(*c))
        .cloned()
        .collect();
    (to_start, to_stop)
}

/// Dispatcher-side handle to a per-root actor thread.
struct Actor {
    /// Send a debounced batch of `(Evt, PathBuf)` to the actor. Dropping this
    /// sender closes the channel → the actor drains and exits (Stop signal).
    tx: Sender<Vec<(Evt, PathBuf)>>,
    join: JoinHandle<()>,
}

/// Per-root actor thread body. Owns its `HttpStore`, `path_to_ids`, `all_ids`.
/// Runs the initial `one_shot_index`, then serially applies batches.
fn actor_loop(
    root: PathBuf,
    conn: Conn,
    embedder: Arc<LazyEmbedder>,
    rx: Receiver<Vec<(Evt, PathBuf)>>,
) {
    let collection = crate::config::collection_name(&root);

    let mut store = HttpStore::new(&conn.host, conn.port, conn.ssl);
    if let Err(e) = store.get_or_create(&collection) {
        eprintln!("service: {} get_or_create failed: {e}", root.display());
        return;
    }

    let spec = load_ignore(&root);

    let stats = match one_shot_index(&mut store, &*embedder, &root, &spec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("service: {} initial sync failed: {e}", root.display());
            return;
        }
    };

    let mut path_to_ids = build_path_to_ids(&store);
    let mut all_ids: HashSet<String> = path_to_ids.values().flatten().cloned().collect();

    eprintln!(
        "service: {} synced files={} chunks={}",
        root.display(),
        stats.files,
        all_ids.len()
    );

    while let Ok(batch) = rx.recv() {
        process_changes(
            &mut store,
            &*embedder,
            &root,
            &batch,
            &mut path_to_ids,
            &mut all_ids,
        );
    }

    eprintln!("service: {} stopped", root.display());
}

/// Bring the running actor set in line with the registry's desired roots.
///
/// Keys everything by CANONICAL path (`scan()` already returns canonical roots).
/// The `contains_key` guard guarantees we never spawn two actors for one root.
fn reconcile<W: Watcher>(
    reg: &Registry,
    conn: &Conn,
    embedder: &Arc<LazyEmbedder>,
    watcher: &mut W,
    actors: &mut HashMap<PathBuf, Actor>,
    specs: &mut HashMap<PathBuf, Ignore>,
    reaper: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    let dead: Vec<PathBuf> = actors
        .iter()
        .filter(|(_, a)| a.join.is_finished())
        .map(|(r, _)| r.clone())
        .collect();
    for root in dead {
        if let Some(actor) = actors.remove(&root) {
            let Actor { tx, join } = actor;
            drop(tx);
            reaper.push(join);
        }
        specs.remove(&root);
        let _ = watcher.unwatch(&root);
        eprintln!("service: pruned dead actor {}", root.display());
    }

    let desired = reg.scan()?;
    let current: HashSet<PathBuf> = actors.keys().cloned().collect();
    let (to_start, to_stop) = reconcile_diff(&desired, &current);

    for root in to_stop {
        if let Some(actor) = actors.remove(&root) {
            let Actor { tx, join } = actor;
            drop(tx);
            // Defer the join to the reaper instead of joining here: a mid-embed
            // or slow-HTTP actor would otherwise stall the whole dispatcher loop.
            reaper.push(join);
        }
        specs.remove(&root);
        let _ = watcher.unwatch(&root);
        eprintln!("service: unwatch {}", root.display());
    }

    for root in to_start {
        // Dedupe guard (canonical key): without it a concurrent reconcile could
        // spawn a second actor for a root that already has one — a race hazard.
        if actors.contains_key(&root) {
            continue;
        }
        specs.insert(root.clone(), load_ignore(&root));

        let (tx, actor_rx) = channel::<Vec<(Evt, PathBuf)>>();
        let emb = Arc::clone(embedder);
        let root_thread = root.clone();
        let conn_thread = conn.clone();
        let join = thread::spawn(move || actor_loop(root_thread, conn_thread, emb, actor_rx));
        actors.insert(root.clone(), Actor { tx, join });

        if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
            eprintln!("service: failed to watch {}: {e}", root.display());
        } else {
            eprintln!("service: watch {}", root.display());
        }
    }

    Ok(())
}

/// Run the shared always-on service. Returns the process exit code.
pub fn run_serve(host: &str, port: u16, ssl: bool, debounce_ms: u64) -> Result<i32> {
    let reg = Registry::from_env();
    let conn = Conn {
        host: host.to_string(),
        port,
        ssl,
    };

    let _lock = match reg.acquire_serve_lock()? {
        None => {
            eprintln!("index-repo: serve already running");
            return Ok(0);
        }
        Some(f) => f,
    };

    let embedder = Arc::new(LazyEmbedder::new());

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }

    let (tx, rx) = channel();
    let mut debouncer = match new_debouncer(Duration::from_millis(debounce_ms), None, tx) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("service: watch loop crashed ({e})");
            return Ok(4);
        }
    };

    let roots_dir = reg.roots_dir();
    let _ = std::fs::create_dir_all(&roots_dir);
    if let Err(e) = debouncer
        .watcher()
        .watch(&roots_dir, RecursiveMode::NonRecursive)
    {
        eprintln!(
            "service: failed to watch registry dir {}: {e}",
            roots_dir.display()
        );
    }

    let mut actors: HashMap<PathBuf, Actor> = HashMap::new();
    let mut specs: HashMap<PathBuf, Ignore> = HashMap::new();
    let mut reaper: Vec<JoinHandle<()>> = Vec::new();

    reconcile(
        &reg,
        &conn,
        &embedder,
        debouncer.watcher(),
        &mut actors,
        &mut specs,
        &mut reaper,
    )?;
    eprintln!("service: watching (roots={})", actors.len());

    let mut last_gc = Instant::now();

    let exit_code = loop {
        if stop.load(Ordering::Relaxed) {
            break 0;
        }

        reaper.retain(|h| !h.is_finished());

        if last_gc.elapsed() >= GC_INTERVAL {
            if let Err(e) = reconcile(
                &reg,
                &conn,
                &embedder,
                debouncer.watcher(),
                &mut actors,
                &mut specs,
                &mut reaper,
            ) {
                eprintln!("service: reconcile failed ({e})");
            }
            last_gc = Instant::now();
        }

        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                let mut registry_changed = false;
                let mut groups: HashMap<PathBuf, Vec<(Evt, PathBuf)>> = HashMap::new();
                let active_roots: Vec<PathBuf> = actors.keys().cloned().collect();

                for ev in &events {
                    let kind = ev.kind;
                    for path in &ev.paths {
                        if path.starts_with(&roots_dir) {
                            registry_changed = true;
                            continue;
                        }
                        if let Some(root) = gitignore_root(path, &active_roots) {
                            specs.insert(root.clone(), load_ignore(root));
                            eprintln!("service: reloaded .gitignore for {}", root.display());
                            continue;
                        }
                        for root in route_all(path, &active_roots) {
                            let Some(spec) = specs.get(root) else {
                                continue;
                            };
                            if !watch_keep(root, spec, path) {
                                continue;
                            }
                            let Some(evt) = evt_for(&kind) else {
                                continue;
                            };
                            groups
                                .entry(root.clone())
                                .or_default()
                                .push((evt, path.clone()));
                        }
                    }
                }

                if registry_changed {
                    if let Err(e) = reconcile(
                        &reg,
                        &conn,
                        &embedder,
                        debouncer.watcher(),
                        &mut actors,
                        &mut specs,
                        &mut reaper,
                    ) {
                        eprintln!("service: reconcile failed ({e})");
                    }
                    last_gc = Instant::now();
                }

                for (root, batch) in groups {
                    if let Some(actor) = actors.get(&root) {
                        let _ = actor.tx.send(batch);
                    }
                }
            }
            Ok(Err(errors)) => {
                for e in errors {
                    eprintln!("service: watch error ({e})");
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 0,
        }
    };

    for (root, actor) in actors.drain() {
        let _ = debouncer.watcher().unwatch(&root);
        let Actor { tx, join } = actor;
        drop(tx);
        let _ = join.join();
    }
    for join in reaper.drain(..) {
        let _ = join.join();
    }

    eprintln!("service: stopped");
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_picks_deepest_nested_root() {
        let roots = vec![PathBuf::from("/a"), PathBuf::from("/a/b")];
        assert_eq!(
            route(Path::new("/a/b/c.rs"), &roots),
            Some(&PathBuf::from("/a/b")),
            "deepest root must win"
        );
        assert_eq!(
            route(Path::new("/a/x.rs"), &roots),
            Some(&PathBuf::from("/a")),
            "shallow file routes to /a"
        );
    }

    #[test]
    fn route_none_when_unrelated() {
        let roots = vec![PathBuf::from("/a"), PathBuf::from("/a/b")];
        assert_eq!(route(Path::new("/z/y.rs"), &roots), None);
    }

    #[test]
    fn route_all_returns_every_ancestor() {
        let roots = vec![PathBuf::from("/repo"), PathBuf::from("/repo/sub")];
        let mut got = route_all(Path::new("/repo/sub/x.rs"), &roots);
        got.sort();
        assert_eq!(
            got,
            vec![&PathBuf::from("/repo"), &PathBuf::from("/repo/sub")],
            "nested file must route to both ancestors"
        );

        let disjoint = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(
            route_all(Path::new("/a/x.rs"), &disjoint),
            vec![&PathBuf::from("/a")],
            "disjoint file routes to exactly one root"
        );

        assert!(route_all(Path::new("/z/y.rs"), &roots).is_empty());
    }

    #[test]
    fn route_component_wise_no_false_prefix() {
        let roots = vec![PathBuf::from("/a/bb")];
        assert_eq!(route(Path::new("/a/b/c.rs"), &roots), None);
    }

    #[test]
    fn gitignore_root_matches_each_roots_own_gitignore() {
        let roots = vec![PathBuf::from("/repo"), PathBuf::from("/repo/sub")];
        assert_eq!(
            gitignore_root(Path::new("/repo/.gitignore"), &roots),
            Some(&PathBuf::from("/repo"))
        );
        assert_eq!(
            gitignore_root(Path::new("/repo/sub/.gitignore"), &roots),
            Some(&PathBuf::from("/repo/sub")),
            "each root's own .gitignore maps to that root"
        );
    }

    #[test]
    fn gitignore_root_rejects_nested_and_non_gitignore() {
        let roots = vec![PathBuf::from("/repo")];
        assert_eq!(
            gitignore_root(Path::new("/repo/src/.gitignore"), &roots),
            None,
            "a nested .gitignore is not a reload trigger (single-file parity)"
        );
        assert_eq!(
            gitignore_root(Path::new("/repo/src/main.rs"), &roots),
            None,
            "an ordinary file is not a .gitignore"
        );
        assert_eq!(
            gitignore_root(Path::new("/other/.gitignore"), &roots),
            None,
            "a .gitignore whose parent is not a registered root is ignored"
        );
    }

    #[test]
    fn reconcile_diff_add_remove_noop() {
        let mut current = HashSet::new();
        current.insert(PathBuf::from("/a"));
        current.insert(PathBuf::from("/b"));

        let desired = vec![PathBuf::from("/b"), PathBuf::from("/c")];
        let (start, stop) = reconcile_diff(&desired, &current);

        assert_eq!(start, vec![PathBuf::from("/c")], "/c is new");
        assert_eq!(stop, vec![PathBuf::from("/a")], "/a is gone");
    }

    #[test]
    fn reconcile_diff_empty_cases() {
        let empty_current: HashSet<PathBuf> = HashSet::new();
        let (start, stop) = reconcile_diff(&[PathBuf::from("/a")], &empty_current);
        assert_eq!(start, vec![PathBuf::from("/a")]);
        assert!(stop.is_empty());

        let mut current = HashSet::new();
        current.insert(PathBuf::from("/a"));
        let (start, stop) = reconcile_diff(&[], &current);
        assert!(start.is_empty());
        assert_eq!(stop, vec![PathBuf::from("/a")]);
    }
}
