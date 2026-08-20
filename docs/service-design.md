# index-repo — Shared Service Design (lifecycle/orchestration layer)

Status: design. Scope: a single long-running `systemd --user` service that holds
ONE lazily-loaded embedding model and watches all currently-active repos,
replacing the per-opencode-session `index-repo --daemon "$PWD"` spawn.

## 0. Invariant (the law)

This is **purely an orchestration layer wrapped around the existing core**.
Byte-for-byte UNCHANGED and MUST NOT be touched:

- `config`, `splitlines`, `chunk`, `chunkfile` (chunking, IDs, metadata) — the
  parity surface stays byte-for-byte identical.
- `embed::Embedder` (model, ONNX output, 384-d vectors).
- `store` (Store/Embed traits, HttpStore, git-identity content collection name).
- `oneshot::one_shot_index(...)`, `daemon::{process_changes, build_path_to_ids,
  watch_keep, safe!}` — reused as the per-root primitives. They now also take a
  `&mut dyn ManifestStore` and drive it alongside the content `Store` (membership
  reconciliation, §2.1); the chunk/ID/metadata behavior they produce is
  unchanged.

`daemon::run_daemon` and the existing `--daemon` flat-CLI path remain intact for
parity tests and manual use. The service does NOT call `run_daemon`; it
re-orchestrates the same primitives (`one_shot_index` + `build_path_to_ids` +
`process_changes`) per-root.

Root cause being fixed: `main.rs:136` eagerly builds `Embedder` before the daemon
branch, so every session pays 86 MB model + ort. The service makes embedding
lazy and shared.

## 1. Registration mechanism — DECISION: registry directory in $XDG_RUNTIME_DIR

Chosen over a unix socket and socket activation.

Justification (KISS + crash recovery + `kill -9`):
- The directory IS the state. No protocol/server socket. `register` = create a
  file; `unregister` = delete it. The wrapper's existing `trap _cleanup EXIT INT
  TERM` covers graceful unregister.
- Natural refcounting: one file per (root, session pid). A root is active iff ≥1
  file for it has a live pid. Two opencode on same repo → two files, one Indexer
  (dedupe by canonical path).
- `kill -9`: the file leaks but embeds the session PID → PID-liveness GC
  (`kill(pid,0)==ESRCH`) reaps it.
- Crash recovery: on (re)start the directory is the full desired set — rescan.

Layout:
```
$XDG_RUNTIME_DIR/index-repo/
  serve.lock              # flock target for the singleton guard (§5)
  roots/
    <hash>.<pid>          # line 1 = canonical absolute path; line 2 = collection name
```
- `<hash>` = first 16 hex of `sha1(canonical_path)`; groups all sessions of one root.
- `<pid>` = the opencode wrapper shell PID, passed in by the wrapper (see §6 note).
- content = line 1 `canonicalize(path)` (symlinks resolved); line 2 = the
  collection name, computed by `register` client-side (where git is available)
  so the daemon never shells out to git. Legacy path-only markers fall back to
  the git-free `fallback_collection_name`.

Refcount: `desired_roots = { canonical_path : ∃ file <hash>.<pid> with pid alive }`.
Indexer started on 0→≥1, stopped on ≥1→0.

State reconstruction: scan `roots/`, GC dead-pid files, compute desired_roots,
spawn an Indexer each. `path_to_ids`/`all_ids` rebuilt from ChromaDB via
`build_path_to_ids` (authoritative) — registry persists only the set of roots.

## 2. Concurrency / consistency — one dispatcher + N per-root actors + shared lazy model

Components:
- **Dispatcher thread (1):** owns the registry inotify receiver AND the single
  `notify-debouncer-full` receiver.
  - registry change → recompute desired_roots (with PID GC) → start/stop Indexer
    actors; `watcher.watch(root, Recursive)` / `unwatch(root)`.
  - debounced fs batch → split by owning root via **longest-prefix match** of the
    event path against active canonical roots; per matched root filter each path
    with `daemon::keep_event(root, spec, path, evt)` and map `EventKind→Evt`; send
    `Vec<(Evt, PathBuf)>` to that root's actor channel. `keep_event` lets
    `Delete`/`Resync` events bypass the extension+ignore filter (so removals and
    resyncs are never dropped), while `Upsert` still uses `watch_keep`.
  - a change to the root's `.gitignore` emits an `Evt::Resync` for that root,
    since the ignore matcher — and therefore file selection — has changed.
  - **periodic orphan sweep:** for each distinct content collection under the
    active roots, the dispatcher runs the single-threaded `service::gc_orphans`
    on an interval, reclaiming `content_ids − union(manifests)`; and it calls the
    manifest's `remove(roothash)` when a root's actor stops, so a departed
    checkout stops pinning its chunks.
- **Indexer actor thread (one per active root):** owns
  `Indexer { store: HttpStore, manifest: HttpManifest, root, roothash, spec,
  path_to_ids, all_ids }` — a content `HttpStore` and its sidecar `HttpManifest`
  side by side. On spawn runs the initial `one_shot_index(&mut store, &mut
  manifest, embedder, &root, &spec)` (which writes this root's manifest as the
  `seen` superset before adding content); then loop `rx.recv()` batch →
  `process_changes(&mut store, &mut manifest, embedder, &root, &roothash, &batch,
  &mut path_to_ids, &mut all_ids)`. It also re-runs `one_shot_index` on a periodic
  resync interval and whenever an `Evt::Resync` arrives, converging membership
  after out-of-band edits or `.gitignore` changes. Exits on a `Stop` message after
  draining the current batch.
- **Shared `Arc<LazyEmbedder>`** (§3): passed to every actor as `&dyn Embed`.

Watcher topology — ONE debouncer for all roots (multiple `watch()` calls on a
single watcher, one merged debounced stream; dispatcher routes by prefix). Avoids
N inotify instances + per-root debounce windows.

Per-root strictly serial (single-threaded actor; events queue + coalesce).
Across roots parallel (independent actor threads; disjoint state). Chunk/parse
kept as-is inside `one_shot_index`/`chunks_for_file` (parity-critical order — do
NOT rayon-ize inside it; cross-root concurrency comes from multiple actors). Embed
serialized behind the shared `Mutex<TextEmbedding>` (acceptable; ONNX inference is
the bottleneck regardless).

Pipeline (per batch, per root):
```
fs event → debouncer(coalesce) → dispatcher(route by root + keep_event + map Evt)
  → actor channel → process_changes:
       chunks_for_file (CPU)              [per-root serial]
     → prune delete-event refs            [manifest + path_to_ids/all_ids]
     → embed new docs (Mutex<model>)      [globally serial]
     → manifest.write(superset)           [BEFORE content add]
     → store.add (never store.delete)     [per-root HttpStore]
     → mutate path_to_ids / all_ids       [actor-owned]
```
A per-root batch never deletes content chunks: a `Delete` event prunes the exact
rel and every key under `rel/` (directory removal) from the manifest and in-memory
maps only. Rewriting the manifest as a superset before the content add keeps the
orphan sweep from reclaiming a just-added-but-not-yet-referenced chunk.

Race analysis:
- `path_to_ids`/`all_ids` safe ONLY because each is owned by exactly one actor
  and never shared. Never hand them to the dispatcher/threadpool.
- **Duplicate Indexers for one physical root (real hazard):** symlinks or racing
  registrations could spawn two actors on the same collection → divergent
  `all_ids`/add-delete thrash. **Mitigation: canonicalize at register time; key
  Indexers by canonical path; dispatcher dedupes.** (adds/deletes are id-idempotent
  so worst case is wasted work, but dedupe makes it correct.)
- Stop during a batch: actor finishes current `process_changes` (atomic w.r.t.
  its state), drains, exits; watcher `unwatch`es.
- Initial-sync vs live events: actor processes no batches until its initial
  `one_shot_index` returns; meanwhile events queue. Same ordering as today's daemon.

### 2.1 Per-root manifest reconciliation

The content collection is shared by every checkout of one origin, so each actor
owns a sidecar `HttpManifest` (collection `<content>__manifests`,
`config::manifest_collection_name`) that records exactly the chunk ids its root
references. `read`/`all_ids` take the highest generation per root, so a manifest
write is observed all-or-nothing; each root is the sole writer of its own rows,
so there is no cross-writer read-modify-write. Membership is NOT in chunk
metadata — the chunk id and metadata stay parity-identical (spec §1, §16).

Because deletion is out of the actor hot path, reclaiming chunks referenced by no
manifest is a dispatcher-owned, single-threaded pass: `service::gc_orphans`
deletes `content_ids − union(manifests)` in batches, snapshotting content ids
BEFORE the union and returning 0 (deleting nothing) when the union is empty. That
empty-union guard prevents wiping a content collection whose manifests have not
been written yet. The superset-before-add ordering (§2 pipeline) guarantees the
sweep never races a just-added chunk into an orphan.

Consequences: two checkouts (worktrees/clones) of one origin share the content
collection safely; identical file content collapses to one shared content doc
referenced by several manifests, and divergent content keeps distinct coexisting
ids. When an actor stops, the dispatcher removes that root's manifest so its
chunks can be reclaimed by the next sweep.

## 3. Lazy embedder — `LazyEmbedder` implementing `store::Embed`

```rust
// src/lazy.rs
use once_cell::sync::OnceCell;
use crate::embed::Embedder;
use crate::store::Embed;
use anyhow::Result;

pub struct LazyEmbedder { cell: OnceCell<Embedder> }
impl LazyEmbedder { pub fn new() -> Self { Self { cell: OnceCell::new() } } }

impl Embed for LazyEmbedder {
    fn embed(&self, docs: &[String]) -> Result<Vec<Vec<f32>>> {
        let e = self.cell.get_or_try_init(|| Embedder::from_env())?;
        e.embed(docs)
    }
}
```
- Shared as `Arc<LazyEmbedder>`; each actor passes `&*arc as &dyn Embed`. ZERO
  changes to `one_shot_index`/`process_changes` signatures.
- Idle ≈ 20 MB: `OnceCell` empty until the FIRST real `embed()`. `one_shot_index`
  only embeds when a flush buffer is non-empty; `process_changes` only when there
  are new records — steady state with no new chunks never loads the model. First
  genuinely-new chunk triggers a one-time `Embedder::from_env()`, then shared forever.
- Thread-safe: `OnceCell` is `Sync` for `T: Send+Sync`; `Embedder`'s
  `Mutex<TextEmbedding>` makes it `Send+Sync`. `get_or_try_init` runs the init at
  most once; concurrent losers block then reuse. Only success is cached.

## 4. CLI surface — subcommands + backward-compatible flat args

```rust
#[derive(Parser)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct Cli {
    #[command(subcommand)] command: Option<Command>,
    #[command(flatten)]    legacy: LegacyArgs, // current Args verbatim:
                                               // path(default "."), --host, --port,
                                               // --collection, --ssl, --full-rebuild,
                                               // --daemon, --debounce
}
#[derive(Subcommand)]
enum Command {
    /// Run the shared always-on service.
    Serve,
    /// Register a repo root with the running service.
    Register   { path: PathBuf, #[arg(long)] pid: Option<u32> },
    /// Unregister a repo root.
    Unregister { path: PathBuf, #[arg(long)] pid: Option<u32> },
}
```
Dispatch in `run()`: `Serve→service::serve()`, `Register→registry::register(...)`,
`Unregister→registry::unregister(...)`, `None→legacy_run(cli.legacy)` (== today's
`run()`, incl. `--daemon`→`run_daemon`). `args_conflicts_with_subcommands` +
`subcommand_negates_reqs` let positional `path`/flags coexist. Parity tests
unaffected. Edge: a dir literally named `serve`/`register`/`unregister` is shadowed
— pass `./serve`.

NOTE (correction to register PID): `register`/`unregister` MUST receive the
opencode wrapper PID via `--pid $$` from the wrapper — NOT `std::process::id()`
(which would be the short-lived `index-repo register` child, instantly dead → GC'd).

## 5. Singleton guard — flock on serve.lock

`serve` opens `$XDG_RUNTIME_DIR/index-repo/serve.lock`, takes `flock(LOCK_EX|LOCK_NB)`,
holds the fd for process lifetime. If held: print `index-repo: serve already running`
to stderr, exit 0 (benign — keeps systemd `Restart` from fighting a healthy
instance). flock auto-releases on death (incl. kill -9) → automatic recovery.

## 6. systemd + wrapper

```nix
systemd.user.services.index-repo = {
  Unit.Description = "Shared semantic code indexer (single shared model)";
  Service = {
    ExecStart  = "${indexerPkg}/bin/index-repo serve";
    Restart    = "on-failure";
    RestartSec = 2;
  };
  Install.WantedBy = [ "default.target" ];   # ALWAYS-ON
};
boot.kernel.sysctl."fs.inotify.max_user_watches"   = 524288;
boot.kernel.sysctl."fs.inotify.max_user_instances" = 1024;
```
Always-on (idle ≈20 MB via lazy model) guarantees `register` has a live consumer
without a start-race. `Restart=on-failure` (clean stop doesn't relaunch).

Wrapper (`home-manager/modules/opencode/wrappers.nix`) — replace the
`setsid index-repo --daemon "$PWD"` spawn + PID-kill with:
```sh
# start hook (after $PWD known):
index-repo register "$PWD" --pid $$
systemctl --user start --no-block index-repo.service   # idempotent safety net
# in _cleanup (already trapped EXIT INT TERM):
index-repo unregister "$PWD" --pid $$
```
No per-session daemon, no model load per session, no setsid/process-group kill.
Refcount + GC replace reap-on-exit.

## 7. Failure modes

- **Service crash:** `Restart=on-failure` relaunches; startup scan + GC + respawn
  one Indexer per desired root; each rebuilds `path_to_ids` from ChromaDB and runs
  `one_shot_index` to converge. Full recovery from registry alone.
- **Registry leak (kill -9):** PID-liveness GC reaps stale files. GC runs at
  startup, on every `roots/` inotify event, and on a periodic 30 s sweep. Last
  live file for a root gone → stop its Indexer (Stop + unwatch).
- **ChromaDB down:** `process_changes` store calls wrapped by `safe!` (logs
  `daemon: chromadb call failed (..)`, swallows; actor survives, retries next
  event). Initial sync error → actor logs + reschedules a backoff retry; service
  never aborts per-root.
- **Model-load failure:** `LazyEmbedder::embed` returns Err; surfaces in the
  existing embed-error branch; the add is skipped, actor survives, next attempt
  retries init (`get_or_try_init` caches only success). Degrades to
  "watch-but-can't-embed", not a crash.
- **inotify watch-limit:** `watch()` errs for the offending root; dispatcher logs
  and leaves it unwatched (others unaffected). Mitigate via sysctl (§6); single
  shared watcher minimizes instances.
- **Two opencode, same repo:** two files, same `<hash>` → one canonical root → one
  Indexer; unregister of one keeps refcount ≥1.

## 8. Module / file decomposition (parallel implementation)

| File | Responsibility | Depends on | Parallelizable? |
|---|---|---|---|
| `src/lazy.rs` | `LazyEmbedder` (`OnceCell<Embedder>` impl `store::Embed`). | `embed::Embedder`, `store::Embed` | Independent — build now. |
| `src/registry.rs` | layout consts; `register(path,pid)`, `unregister(path,pid)`, `scan()->desired_roots`, PID-liveness GC, canonicalize+hash, `serve.lock` flock helper. | std, sha1, libc | Independent — build now. |
| `src/service.rs` | `Indexer` + actor loop; dispatcher (registry inotify + single debouncer routing by longest-prefix + `watch_keep` + `EventKind→Evt`); `serve()` (singleton guard, startup scan, spawn/stop, periodic GC). Reuses `one_shot_index`/`process_changes`/`build_path_to_ids`/`watch_keep`/`safe!`. | lazy, registry, oneshot, daemon, store, walk, notify-debouncer-full | Integration bottleneck — after A/B APIs frozen. |
| `src/main.rs` (edit) | `Cli` subcommands → `service`/`registry`/`legacy_run`. | all | Last; thin wiring. |

Critical path: A (lazy) ∥ B (registry) → C (service) → D (main + Nix unit/wrapper).
`service.rs` is the sole integration/race-risk point — concentrate review there.
Nothing in A–D modifies the parity-critical core; existing chunk/ID parity tests
remain the regression gate.
