# index-repo — Design Spec (Rust port)

Authoritative design + behavioral-parity contract for porting the Python
`index_repo.py` semantic code indexer to Rust.

Primary goal: **maximize indexer speed** while keeping **observable behavior
byte-for-byte identical** to the Python implementation. The chunk identity
(SHA1) and ChromaDB contract are the hard parity surface; everything else is an
implementation detail as long as the observable outputs match.

Reference source: the previous `index_repo.py` (≈766 lines). This spec restates
its logic precisely; the Python file is the ground truth for any ambiguity.

---

## 0. Source-of-truth decisions (locked)

- **Embedder**: `all-MiniLM-L6-v2` (384-dim, mean-pool + L2 norm) via
  `fastembed` (unquantized `AllMiniLML6V2`). MUST stay in the same vector space
  as the query path (`chroma-mcp`, also `all-MiniLM-L6-v2`). Do **not** change
  the model — the query path is not being touched.
- **Embedding location**: computed client-side in the Rust binary; explicit
  embeddings are sent to ChromaDB on `add` (same as the Python client, which
  embedded client-side via Chroma's default EF).
- **Model delivery**: bundled via a Nix fixed-output derivation; the binary
  loads it from a local directory with zero network at runtime.
- **ChromaDB access**: **direct `reqwest` against the v2 REST API**. The
  community `chromadb` Rust crate is V1-only and is NOT usable against the
  1.0.x server.
- **Concurrency**: synchronous design — `reqwest::blocking` + `rayon` +
  `notify-debouncer-full`. No async runtime.
- **Cutover**: run `index-repo --full-rebuild` once after deployment so the
  whole collection lands in the fastembed vector space (embeddings differ from
  the old Chroma-default vectors by ~1e-4; a single rebuild avoids mixing
  spaces). IDs are content-based, so unchanged chunks would otherwise keep old
  vectors.

---

## 1. Behavioral parity contract (the law)

These MUST match the Python exactly:

1. **Chunk ID**: `sha1(format!("{rel}:{line_no}:{body}").as_bytes())` rendered as
   lowercase hex. `rel` is the file path relative to `root`, POSIX separators.
   `line_no` is 1-based. `body` is the chunk text. Any deviation changes IDs and
   breaks incremental diffing.
2. **Metadata** per chunk: `{ "path": rel, "line": line_no, "lang": lang,
   "type": node_type }`, plus `"scope": scope` **only when** scope is non-empty.
   - `lang` = file suffix without leading dots, lowercased; if empty, the file
     name. (Python: `path.suffix.lstrip(".").lower() or path.name`.)
     Examples: `foo.py`→`py`, `x.tsx`→`tsx`, `Makefile`→`Makefile`,
     `.envrc`→`.envrc`.
3. **Chunk set** for any given file content + path must be identical (same
   ordering, same `line_no`, same `body`, same `node_type`, same `scope`).
4. **File selection** (which files get indexed) must be identical.
5. **Collection contract**: collection name default `code-<basename(root)>`;
   created with metadata `{"hnsw:space":"cosine"}`.
6. **CLI / return codes / stderr text**: identical (see §10).

`node_type` values: a tree-sitter node kind (e.g. `function_definition`),
`"preamble"` (gap text between semantic nodes), or `"window"` (whole-file
line-window fallback).

---

## 2. Constants (copy verbatim)

```
CHUNK_LINES       = 120
OVERLAP           = 20
MAX_FILE_BYTES    = 512 * 1024
MAX_SEMANTIC_LINES = 200
BATCH             = 2000
```

**EXTS** (indexable extensions, lowercased, includes dot):
```
.py .pyi
.ts .tsx .js .jsx .mjs .cjs
.php
.go .rs .java .kt .swift .c .h .cc .cpp .hpp
.rb .ex .exs .cs
.nix
.sql
.sh .bash .zsh
.md .mdx .rst
.toml .yaml .yml .json .jsonc
.vue .svelte .html .css .scss
```

**Special filenames** also indexed (exact match on file name):
```
Makefile  Dockerfile  Justfile  .envrc
```

**EXTRA_IGNORE** (prepended to `.gitignore` patterns; gitwildmatch semantics):
```
.git/ .chroma/ .direnv/ .venv/ venv/
node_modules/ vendor/
dist/ build/ out/ target/ result
.next/ .nuxt/ .cache/ .parcel-cache/
__pycache__/ *.pyc
*.min.js *.min.css *.map
storage/ bootstrap/cache/
*.lock package-lock.json composer.lock uv.lock
yarn.lock pnpm-lock.yaml
```

**EXT_TO_LANG** (tree-sitter language key by suffix, lowercased):
```
.php → php
.go  → go
.js .jsx .mjs .cjs → javascript
.ts → typescript      .tsx → tsx
.py .pyi → python
.rs → rust
.sh .bash .zsh → bash
```

**SEMANTIC_TYPES** (AST node kinds extracted as their own chunk; walk stops at a
match so nested functions stay inside the parent):
```
php        : function_definition, method_declaration
go         : function_declaration, method_declaration, type_declaration
javascript : function_declaration, method_definition
typescript : function_declaration, method_definition, type_alias_declaration, interface_declaration
tsx        : function_declaration, method_definition, type_alias_declaration, interface_declaration
python     : function_definition
rust       : function_item, struct_item, enum_item, trait_item, macro_definition
bash       : function_definition
```

**_SCOPE_TYPES** (container node kinds whose `name` field becomes scope context):
```
php        : class_declaration, interface_declaration, trait_declaration, enum_declaration
go         : (none)
javascript : class_declaration
typescript : class_declaration
tsx        : class_declaration
python     : class_definition
rust       : impl_item, trait_item
bash       : (none)
```

**tree-sitter grammar mapping** (lang key → Rust grammar):
```
php        → tree_sitter_php   (the full PHP grammar, == Python tree_sitter_php.language_php)
go         → tree_sitter_go
javascript → tree_sitter_javascript
typescript → tree_sitter_typescript LANGUAGE_TYPESCRIPT
tsx        → tree_sitter_typescript LANGUAGE_TSX
python     → tree_sitter_python
rust       → tree_sitter_rust
bash       → tree_sitter_bash
```
Node-kind names are grammar-defined and identical to the Python bindings.

---

## 3. Chunking algorithm

A chunk is `(line_no: usize /*1-based*/, body: String, node_type: String,
scope: String)`.

### 3.1 `line_window(text) -> [(line_no, body)]`
Sliding-window line chunking.
- `lines = text.splitlines()` using **Python `str.splitlines()` semantics**
  (universal newlines: split on `\n \r \r\n \v \f \x1c \x1d \x1e \x85 \u2028
  \u2029`, no trailing empty element). This MUST match Python; implement a
  helper rather than using Rust `str::lines()` (which only splits `\n`/`\r\n`).
- if `lines` empty → yield nothing.
- `step = max(1, CHUNK_LINES - OVERLAP)` = 100.
- for `i` in `(0..len).step_by(step)`:
  - `body = lines[i .. min(i+CHUNK_LINES, len)].join("\n")`
  - if `body.trim()` non-empty → yield `(i + 1, body)`
  - if `i + CHUNK_LINES >= len` → break

### 3.2 `collect_semantic(root, lang) -> [Node]`
Pre-order walk; when a node kind is in `SEMANTIC_TYPES[lang]`, collect it and
**do not descend** into it. Otherwise recurse into children in order.

### 3.3 `get_scope(node, lang) -> String`
Climb parents from `node`:
- containers = `_SCOPE_TYPES[lang]`.
- for each ancestor whose kind ∈ containers: take child field `name`'s text; if
  absent and ancestor kind == `impl_item`, take child field `type`'s text.
- collect those names, then return them joined by `.` in **root→leaf** order
  (i.e. reverse of the climb order). Empty string if none.
- Node text decoded UTF-8 with replacement on invalid bytes.

### 3.4 `ts_chunk(text, lang) -> [Chunk]`
- get parser for `lang`; parse `text` as UTF-8 bytes. On any parser/parse error
  → return empty (caller falls back to line-window).
- `nodes = collect_semantic(root, lang)`; if empty → return empty.
- sort `nodes` by `start_byte` ascending.
- `lines = text.split('\n')` (note: plain `\n` split here, **not** splitlines);
  `total = lines.len()`.
- `cursor = 0` (next uncovered 0-based line).
- for each `node`:
  - `node_start = node.start_position().row`
  - `node_end = node.end_position().row`
  - if `node.end_position().column == 0 && node_end > node_start` → `node_end -= 1`
  - **gap before node**: if `cursor < node_start`:
    - `gap = lines[cursor..node_start].join("\n")`
    - if `gap.trim()` non-empty: for `(off, body)` in `line_window(gap)` →
      push `(cursor + off, body, "preamble", "")`
  - **the node itself**: `chunk_text = node.utf8_text(...)` (replacement on
    invalid), `scope = get_scope(node, lang)`, `start_line = node_start + 1`.
    - if `chunk_text.splitlines().count() > MAX_SEMANTIC_LINES`:
      for `(off, body)` in `line_window(chunk_text)` →
      push `(start_line + off - 1, body, node.kind(), scope.clone())`
    - else push `(start_line, chunk_text, node.kind(), scope)`
  - `cursor = max(cursor, node_end + 1)`
- **trailing gap**: if `cursor < total`: `gap = lines[cursor..].join("\n")`; if
  `gap.trim()` non-empty → line-window as `"preamble"` with `line_no = cursor + off`.

### 3.5 `detect_lang(path) -> Option<lang>`
- if file name ends with `.blade.php` → `None` (Blade templates are too mixed).
- else `EXT_TO_LANG.get(suffix.to_lowercase())`.

### 3.6 `chunk_file(text, path) -> [Chunk]`
- `lang = detect_lang(path)`; if `Some` → `ts = ts_chunk(text, lang)`; if `ts`
  non-empty → return `ts`.
- fallback: for `(line_no, body)` in `line_window(text)` →
  `(line_no, body, "window", "")`.

---

## 4. Per-file chunk computation (`chunks_for_file`)

Input: `path`, `root`. Output: `(rel, chunks: [(id, body, meta)], ts_count,
win_count, ok)`.

- `rel = path.relative_to(root)` as POSIX string.
- read file as UTF-8:
  - invalid UTF-8 (binary) → return `(rel, [], 0, 0, ok=false)`.
  - other I/O error → return `(rel, [], 0, 0, ok=true)` (treated as readable but
    empty; not counted as binary).
- `lang_meta = suffix without leading dots, lowercased, else file name`.
- for each `(line_no, body, node_type, scope)` in `chunk_file(text, path)`:
  - `id = sha1_hex("{rel}:{line_no}:{body}")`
  - `meta = {path: rel, line: line_no, lang: lang_meta, type: node_type}` and
    `scope` if non-empty.
  - tally `win_count` when `node_type == "window"`, else `ts_count`.
- return with `ok=true`.

---

## 5. File iteration + ignore (`iter_files`, `load_ignore`)

### 5.1 `load_ignore(root)`
Build one matcher from `EXTRA_IGNORE` **followed by** the lines of
`root/.gitignore` (only the root one; if it exists), using gitwildmatch
semantics. Use `ignore::gitignore::GitignoreBuilder` (add each pattern via
`add_line(None, pat)`), built relative to `root`.

### 5.2 `iter_files(root)`
Enumerate **all** files recursively (equivalent to Python `root.rglob("*")` +
`is_file()`), then keep a path iff:
1. suffix (lowercased) ∈ EXTS **or** file name ∈ {`Makefile`,`Dockerfile`,
   `Justfile`,`.envrc`}, AND
2. NOT ignored: `gitignore.matched_path_or_any_parents(rel, is_dir=false)` is
   not `Ignore` (matches Python `spec.match_file(rel)` including files inside an
   ignored directory), AND
3. `stat().len() <= MAX_FILE_BYTES` (on stat error → skip).

Parity-critical: do **not** use the `ignore` crate's automatic gitignore/hidden/
parent-ignore walking — it reads nested `.gitignore`s, global excludes, `.ignore`
files, and skips dotfiles, which diverges from the Python (single root
`.gitignore` + EXTRA_IGNORE, dotfiles included). Use a plain recursive walk and
apply our matcher explicitly.

Speed: walk + read + parse + hash in parallel with `rayon` (e.g. collect the
file list, then `par_iter().map(chunks_for_file)`); preserve deterministic
output by sorting/merging as needed (ordering within the add-batch does not
affect IDs, but keep file traversal stable for reproducible logs). A parallel
walker (`ignore::WalkBuilder` with `.standard_filters(false)`, or `jwalk`) is
acceptable as long as the selection predicate above is applied verbatim.

---

## 6. One-shot incremental scan (`one_shot_index`)

Returns stats `{files, added, unchanged, deleted, ts_chunks, win_chunks,
skipped_bin}`.

1. `existing = collection.get_ids_all()` (v2 `/get` with `include: []`; paginate
   to fetch ALL ids — see §8.4). On error → log warning, treat as empty:
   `"  warning: failed to fetch existing ids ({e}); treating as empty"`.
2. iterate `iter_files(root)`; for each file `chunks_for_file`:
   - `ok==false` → `skipped_bin += 1`; continue.
   - `files += 1`; `ts_chunks += ts`; `win_chunks += win`.
   - for each `(id, body, meta)`: record `id` in `seen`; if `id ∈ existing` →
     `unchanged += 1`; else buffer `(id, body, meta)`; when buffer ≥ `BATCH`
     flush via `/add` and `added += n`.
3. flush remainder.
4. `stale = existing - seen`; delete in `BATCH`-sized id batches via `/delete`;
   `deleted += n`.

`flush`: no-op on empty; else `/add` with embeddings computed for the batch
documents (see §9). `added` counts successfully added ids.

---

## 7. Daemon (`--daemon`)

Long-lived live indexer. Launched by the opencode wrapper as
`index-repo --daemon $PWD` under `setsid`, killed (process-group `SIGTERM`) on
opencode exit. No status socket.

1. **initial sync**: run `one_shot_index` once.
2. **build `path_to_ids`**: from the collection's own metadata (v2 `/get` with
   `include: ["metadatas"]`, paginated): map `path → {ids}`. On error log
   `"daemon: failed to load existing metadata ({e})"` and continue with empty.
   `all_ids = ⋃ path_to_ids.values()`.
3. log initial-sync summary (see §10).
4. install signal handlers for `SIGTERM`, `SIGINT`, `SIGHUP` → set stop flag /
   stop the watcher.
5. **watch** `root` recursively with debounce = `args.debounce` ms (default
   800) using `notify-debouncer-full`; apply the same selection predicate as
   `iter_files` to filter events (gitignore + EXTS/special names). Map events:
   removed → `delete`; created/modified/renamed → `upsert` (a `delete` for a
   path wins over `upsert` within one debounced batch).
6. **process batch** (`process_changes`) per file `rel`:
   - if `delete` or path no longer exists: `old = path_to_ids.remove(rel)`; if
     non-empty → `/delete ids=old`; `all_ids -= old`; `deleted += n`.
   - else `chunks_for_file`; if `ok==false` → skip (binary slipped through).
     - `seen = {ids}`; `new = [(id,body,meta) for id ∉ all_ids]`.
     - `stale = path_to_ids[rel] - seen`; if non-empty → `/delete ids=stale`;
       `all_ids -= stale`; `deleted += n`.
     - if `new` non-empty → `/add`; `all_ids ∪= new_ids`; `added += n`.
     - `path_to_ids[rel] = seen`.
   - if `added>0 || deleted>0` → log `"daemon: live update — added={a}
     deleted={d} chunks={len(all_ids)}"`.
- Every ChromaDB call in the daemon is wrapped so a transient backend failure is
  logged (`"daemon: chromadb call failed ({e})"`) and swallowed; the watch loop
  survives.
- On watch-loop crash: log `"daemon: watch loop crashed ({e})"` and return `4`.
- On normal stop: log `"daemon: stopped"` and return `0`.

---

## 8. ChromaDB v2 REST contract (`reqwest::blocking`)

Base: `http{,s}://{host}:{port}/api/v2`. Tenant/database default segments:
`tenants/default_tenant/databases/default_database`.

### 8.1 heartbeat
`GET /api/v2/heartbeat` → 200 with `{"nanosecond_timestamp": <i64>}`. Used to
verify reachability; on failure the CLI exits 3 (see §10).

### 8.2 get-or-create collection
`POST /api/v2/tenants/default_tenant/databases/default_database/collections`
body `{"name": <name>, "metadata": {"hnsw:space":"cosine"}, "get_or_create":
true}` → `{"id": <uuid>, ...}`. Cache the returned `id` for record ops.

### 8.3 delete collection (only for `--full-rebuild`)
Resolve the collection id (get-or-create or GET by name), then
`DELETE /api/v2/.../collections/{id}`. Ignore "not found"/errors (Python
swallows all). Then re-create via §8.2.

### 8.4 get records
`POST /api/v2/.../collections/{id}/get` body `{"include": []}` for ids-only, or
`{"include": ["metadatas"]}` for id+metadata. **Paginate** with `limit`/`offset`
(or the server's documented cursor) to retrieve the full set; a single
unbounded `/get` may be capped. Accumulate all ids/metadatas.

### 8.5 add records
`POST /api/v2/.../collections/{id}/add` body
`{"ids":[...], "embeddings":[[...384 f32...], ...], "documents":[...],
"metadatas":[{...}, ...]}`. Embeddings are computed locally (see §9). Batch size
`BATCH` (2000).

### 8.6 delete records
`POST /api/v2/.../collections/{id}/delete` body `{"ids":[...]}`. Batch size
`BATCH`.

### 8.7 count (final log only)
`GET /api/v2/.../collections/{id}/count` → integer, for the `count=` field of
the one-shot summary line.

Reuse a single `reqwest::blocking::Client` (keep-alive) for all calls.

---

## 9. Embedding (fastembed, local model)

- Model: `AllMiniLML6V2` unquantized, loaded from a **local directory** with no
  network: `TextEmbedding::try_new_from_user_defined(UserDefinedEmbeddingModel
  ::new(model_onnx_bytes, TokenizerFiles{ tokenizer.json, config.json,
  special_tokens_map.json, tokenizer_config.json }), InitOptionsUserDefined
  ::default())`.
- Files live in a flat directory provided by Nix; the binary reads its path from
  env `INDEX_REPO_MODEL_DIR` (set by the Nix wrapper to the FOD store path). If
  unset, fall back to a sensible default and error clearly if the files are
  missing. **No new required CLI flag** (preserves CLI parity).
- Output: 384-dim vectors, mean-pooled + L2-normalized (fastembed default).
  These are sent as explicit `embeddings` on `/add`.
- Embedding runs in batches (align with the `BATCH` add batches; fastembed
  batches internally and uses onnxruntime threads). Initialize the model once
  and reuse.
- onnxruntime is provided by Nix (`pkgs.onnxruntime`); see §12.

The model is initialized lazily on first add so a no-op incremental run (nothing
new to embed) does not pay model load cost — matching the Python, where the
Chroma EF only embeds when `add` is called.

---

## 10. CLI, return codes, stderr (verbatim parity)

### 10.1 arguments
```
path                positional, default "."
--host              default "192.168.1.2"
--port    <int>     default 8000
--collection <str>  default None → code-<basename(root)>
--ssl               flag → https
--full-rebuild      flag → drop collection, re-embed everything
--daemon            flag → live indexer
--debounce <int>    default 800  (daemon fs-event debounce window, ms)
```

### 10.2 startup
- resolve `root = abspath(path)`. If not a directory:
  `eprintln "error: {root} is not a directory"`; return `2`.
- `collection = collection_arg or "code-{basename(root)}"`.
- `mode = daemon ? "daemon" : (full_rebuild ? "full rebuild" : "incremental")`.
- `eprintln "indexing {root} → {host}:{port}  collection={collection} mode={mode}"`.
- connect + heartbeat. On failure:
  `eprintln "error: cannot reach chromadb at {host}:{port} ({e})\nis ` + "`systemctl status chromadb`" + ` running?"`; return `3`.
- if `--full-rebuild`: delete collection (swallow errors).
- get-or-create collection with `{"hnsw:space":"cosine"}`.
- if `--daemon`: run daemon (return its code). else one-shot.

### 10.3 one-shot summary (stderr)
```
done. files={files} added={added} unchanged={unchanged} deleted={deleted} (tree-sitter={ts_chunks}, window={win_chunks}) skipped_binary={skipped_bin} grammars={grammars} collection={collection} count={count}
```
`grammars` = comma-joined sorted set of grammar keys **actually used** during
the run (Python reports lazily-loaded grammars; track which lang keys were
parsed). If none, `"none"`.

### 10.4 daemon messages (stderr, exact)
```
daemon: initial sync of {root}
daemon: initial sync done — files={files} added={added} unchanged={unchanged} deleted={deleted} chunks={len(all_ids)} grammars={grammars}
daemon: watching {root} (debounce={debounce}ms)
daemon: live update — added={a} deleted={d} chunks={n}
daemon: stopped
```
Error/edge messages (exact): `daemon: chromadb call failed ({e})`,
`daemon: failed to load existing metadata ({e})`,
`daemon: watch loop crashed ({e})`,
`  warning: failed to fetch existing ids ({e}); treating as empty`.

### 10.5 return codes
`0` success/normal stop · `2` not a directory · `3` chromadb unreachable ·
`4` daemon watch-loop crash.

---

## 11. Crate / module layout

```
Cargo.toml
Cargo.lock
src/
  main.rs        CLI parse, startup, mode dispatch, return codes, summary logs
  config.rs      constants (EXTS, EXTRA_IGNORE, EXT_TO_LANG, SEMANTIC_TYPES, _SCOPE_TYPES)
  walk.rs        load_ignore, iter_files (selection predicate), parallel walk
  chunk.rs       line_window (py-splitlines), collect_semantic, get_scope,
                 ts_chunk, detect_lang, chunk_file
  grammar.rs     lang key → tree-sitter Language; tracks used grammars
  chunkfile.rs   chunks_for_file (read, ids, metadata)
  embed.rs       fastembed local-model init + batch embed
  chroma.rs      v2 REST client (heartbeat, get_or_create, get(paginated),
                 add, delete, delete_collection, count)
  oneshot.rs     one_shot_index
  daemon.rs      build_path_to_ids, watcher + debounce, process_changes, signals
docs/
  spec.md  plan.md
```

---

## 12. Cargo dependencies (pinned starting point)

```toml
[dependencies]
fastembed = { version = "5.17", default-features = false, features = ["ndarray", "std"] }
# ort is pulled transitively (=2.0.0-rc.12); system onnxruntime via env (see Nix).
reqwest = { version = "0.12", features = ["json", "blocking"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha1 = "0.10"
rayon = "1"
ignore = "0.4"
notify-debouncer-full = "0.5"   # pin to a version exposing notify 7/8; verify at impl time
walkdir = "2"                   # if not using ignore's walker for the plain walk
clap = { version = "4", features = ["derive"] }
anyhow = "1"

tree-sitter = "0.25"
tree-sitter-python = "0.25"
tree-sitter-rust = "0.24"
tree-sitter-go = "0.25"
tree-sitter-php = "0.24"
tree-sitter-javascript = "0.25"
tree-sitter-typescript = "0.23"
tree-sitter-bash = "0.25"
```
Pin exact versions in `Cargo.lock`. Verify the `notify-debouncer-full` ↔
`tree-sitter` ↔ `fastembed`/`ort` versions resolve together at implementation
time. `default-features = false` on fastembed disables `ort-download-binaries`.

---

## 13. Nix packaging (consumed by the nixos repo)

The nixos `home-manager/modules/opencode` module currently builds `index-repo`
from the Python script via `writeTextFile`. It will instead build this crate.

- **Build**: `rustPlatform.buildRustPackage` with `cargoLock.lockFile`.
  - `nativeBuildInputs`: `pkg-config`.
  - `buildInputs`: `onnxruntime`, `openssl` (reqwest), `stdenv.cc.cc.lib`.
  - build env: `ORT_STRATEGY = "system"`,
    `ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib"`, `CARGO_NET_OFFLINE = "1"`.
  - ensure `onnxruntime` is resolvable at runtime (rpath from `buildInputs`, or
    wrap the binary with `ORT_DYLIB_PATH=${onnxruntime}/lib/libonnxruntime.so`).
- **Model FOD**: fetch the 5 files of `Qdrant/all-MiniLM-L6-v2-onnx`
  (`model.onnx`, `tokenizer.json`, `config.json`, `special_tokens_map.json`,
  `tokenizer_config.json`) into a store dir (fixed-output, pinned sha256). Wrap
  the binary to export `INDEX_REPO_MODEL_DIR` → that dir.
- **Wiring** (in the nixos repo, separate change):
  - `default.nix`: `indexerPkg` becomes the wrapped Rust package; keep
    `destination`/binary name `index-repo` so `wrappers.nix` is unchanged
    (`index-repo --daemon "$PWD"` under `setsid`).
  - `wrappers.nix`: unchanged.
  - `packages.nix`: keep `uv` (needed by `chroma-mcp`) and `nodejs`; the Python
    indexer's inline deps disappear with the script.
  - delete `home-manager/modules/opencode/scripts/index_repo.py`.
  - private-repo consumption: `fetchFromGitHub` for a private repo needs a
    build-time token, or add this repo as a flake input over `git+ssh://`.
    Decide at wiring time (see Risks).

---

## 14. Verification & parity test plan

1. **Build**: `cargo build --release`, `cargo clippy -- -D warnings`,
   `cargo fmt --check`.
2. **ID parity (embedding-independent, decisive)**: pick a fixture tree
   (this nixos repo is a good corpus). Run the Python `index_repo.py` against a
   fresh throwaway collection and the Rust binary against another fresh
   collection, both one-shot. Compare the **set of chunk IDs** and the
   per-chunk `(line, type, scope, lang, path)` metadata. They MUST be identical.
   (IDs are content-hash based and independent of the embedder, so this isolates
   chunking/selection parity.) Provide a small `--print-chunks`-style debug dump
   behind a hidden flag or a separate test binary to make the comparison cheap;
   if added, it must not alter normal output.
3. **Selection parity**: diff the list of indexed file paths (Python vs Rust)
   over several real repos to catch gitwildmatch edge cases.
4. **End-to-end**: run Rust `--full-rebuild` against the real ChromaDB; confirm
   `chroma_query_documents` still returns sensible hits (query path unchanged).
5. **Daemon**: touch/modify/delete files; confirm live add/delete logs and that
   `all_ids`/collection converge to the same state as a fresh one-shot.
6. **nixos gate**:
   `nix build .#nixosConfigurations.pc.config.system.build.toplevel --dry-run`
   with no new eval/deprecation warnings (the `Git tree is dirty` warning is
   expected/excluded), plus a real build of the package.
7. **Benchmark** (same command before/after): warm incremental run and cold
   `--full-rebuild`, wall-clock, on the nixos repo. Record numbers in the PR.

---

## 15. Risks / open considerations

- **`str.splitlines()` parity**: Python splits on the full universal-newline
  set; Rust `lines()` does not. Mismatch changes `body`/`line_no` → different
  IDs. Implement a Python-compatible splitter for `line_window`. (Low real-world
  incidence in code, but required for strict parity.)
- **gitwildmatch parity**: `ignore` crate vs Python `pathspec`. Mostly
  equivalent; verify with the selection-parity test (§14.3). Watch directory
  patterns (`node_modules/`) matching nested files via
  `matched_path_or_any_parents`.
- **onnxruntime runtime linkage** under Nix (rpath vs `ORT_DYLIB_PATH`) — the
  main packaging sharp edge.
- **Private repo consumption** by the nixos build (token vs `git+ssh` flake
  input).
- **Embedding ~1e-4 drift** vs the old Chroma-default vectors — accepted;
  mitigated by a one-time `--full-rebuild` at cutover.
- **`/get` pagination**: must fetch ALL ids/metadata; do not rely on a single
  unbounded response.
- **`grammars=` log**: track grammars actually used to mirror Python's lazy set.
```

---

## 16. Intentional deviations from strict parity (post-v0.1 hardening)

The v0.1 goal was byte-for-byte parity with `index_repo.py`. The following
changes deliberately break that contract to fix correctness, coverage, and
security drawbacks. A one-time `--full-rebuild` (or letting the daemon re-sync)
migrates existing collections.

- **Collection name** is now git-identity based: `code-<owner>-<repo>` (plus the
  in-repo sub-path when nested) from `remote.origin.url`, stable across machines
  and clones. Without a git remote it falls back to `code-<basename>-<hash8>`
  (SHA1 of the canonical path). Previously `code-<basename>` alone, which let two
  same-basename repos collide onto one collection and delete each other's chunks.
  The `chroma-gate.ts` plugin mirrors the scheme (including the git lookup).
  Caveat: two local checkouts of the *same* remote now map to one collection —
  index only one at a time, or pass `--collection`.
- **Daemon consistency**: `process_changes` updates in-memory state
  (`all_ids`/`path_to_ids`) only after the ChromaDB add/delete actually
  succeeds; a failed call is retried on the next fs event instead of being
  silently marked present (no more index drift on transient failures).
- **Non-UTF-8 files** are indexed via lossy decode; only files containing NUL
  bytes are treated as binary and skipped (previously any invalid UTF-8 was
  dropped).
- **Grammars**: added Java, C, C++, C#, Ruby (13 languages total). Other
  extensions still fall back to line-window chunking.
- **Configurable knobs** (env, defaults preserve v0.1 vectors/behavior):
  `INDEX_REPO_MAX_FILE_BYTES`, `INDEX_REPO_MAX_LENGTH`, `INDEX_REPO_INTRA_THREADS`,
  `INDEX_REPO_EMBED_BATCH`, `INDEX_REPO_POOLING`, `INDEX_REPO_CHROMA_TOKEN`.
- **Default host** is `127.0.0.1` (was `192.168.1.2`); set `--host` for remote.
- **Auth**: `INDEX_REPO_CHROMA_TOKEN` sends `Authorization: Bearer <token>`.

### Deliberately NOT changed (tradeoffs / external coupling)

- **Chunk-id scheme** `sha1(rel:line:body)` keeps `line`: a moved chunk gets a
  new id (re-embed churn). This is the price of always-correct line numbers in
  results, which a code-search tool needs. Kept.
- **Embedding model** (`all-MiniLM-L6-v2`) and **256-token** truncation remain
  the defaults: the query path (`chroma-mcp` / chromadb DefaultEmbeddingFunction)
  is external, so swapping requires changing both sides. Now env-overridable for
  users who control both.
- **Single-file `.gitignore`** selection (no nested/global excludes) is retained
  for selection parity; changing it is a broad semantics change.
