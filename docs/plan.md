# index-repo (Rust port) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the Python `index_repo.py` ChromaDB semantic code indexer to Rust with byte-for-byte behavioral parity (chunk IDs, metadata, file selection, CLI, logs) and faster warm/incremental scans.

**Architecture:** Synchronous Rust binary. Parallel file walk/parse/hash (`rayon`), tree-sitter AST chunking, `fastembed` (`all-MiniLM-L6-v2`, local model) for embeddings, ChromaDB via direct `reqwest::blocking` v2 REST. ChromaDB access is hidden behind a `Store` trait so the one-shot and daemon logic are unit-testable with a mock. File watching via `notify-debouncer-full`.

**Tech Stack:** Rust 2021, fastembed/ort + system onnxruntime, tree-sitter (+python/rust/go/php/javascript/typescript/bash), reqwest, rayon, ignore, notify-debouncer-full, clap, sha1, serde_json. Packaged with Nix `rustPlatform.buildRustPackage` + a fixed-output model derivation, exposed via in-repo `flake.nix`.

**Companion:** `docs/spec.md` is the authoritative behavioral contract. Every "copy verbatim from spec §N" reference points to concrete content there; do not paraphrase those blocks.

---

## Build environment (read first)

There is **no global Rust toolchain**, but the machine has an existing Nix devshell `dev#rust` providing nixpkgs `cargo`, `rustup`, `pkg-config`, and `openssl`. The rustup `stable` toolchain (1.96.0 — rustc+cargo matched) is **already installed and set as default**. Therefore:

- **Do not create a toolchain devShell.** Use the existing one. Run every build/test command as: `nix develop dev#rust -c rustup run stable cargo ...` — e.g. `nix develop dev#rust -c rustup run stable cargo build`, `... cargo test ...`, `... cargo clippy -- -D warnings`, `... cargo fmt`. Tasks write bare `cargo ...` for brevity; **always** wrap with `nix develop dev#rust -c rustup run stable`. Use `rustup run stable cargo` (not the bare nixpkgs `cargo` on PATH) so cargo and rustc are the same 1.96.0 release (the bare `cargo` is 1.95 and would mismatch the rustup `rustc`).
- The in-repo `flake.nix` (Task 14) is for the **package** + model FOD (for nixos to consume), NOT the dev toolchain. It only sees git-tracked files, so `git add` new files before building via that flake.
- **Defer onnxruntime:** add `fastembed` to `Cargo.toml` only at Task 9. Tasks 0–8 build with no onnxruntime. `dev#rust` does **not** provide onnxruntime; from Task 9 on, layer it into the command:
  ```bash
  nix develop dev#rust -c bash -c '
    export ORT_STRATEGY=system
    export ORT_LIB_LOCATION="$(nix build --no-link --print-out-paths nixpkgs#onnxruntime)/lib"
    export CARGO_NET_OFFLINE=1
    rustup run stable cargo build'
  ```
  This `ort`↔system-onnxruntime link is the sharpest edge (no nixpkgs precedent) — if linking fails, verify those env vars and that `$ORT_LIB_LOCATION/libonnxruntime.so` exists, and escalate.
- `hyperfine` (Task 16) is absent → use `nix run nixpkgs#hyperfine -- ...`.

---

## File Structure

```
Cargo.toml                 crate manifest, pinned deps
flake.nix                  package + model FOD + devShell (for nixos to consume as input)
src/
  main.rs                  CLI parse, startup, mode dispatch, return codes, summary logs
  config.rs                constants (EXTS, EXTRA_IGNORE, EXT_TO_LANG, SEMANTIC_TYPES, _SCOPE_TYPES)
  splitlines.rs            Python-compatible str.splitlines()
  chunk.rs                 line_window, collect_semantic, get_scope, ts_chunk, detect_lang, chunk_file
  grammar.rs               lang key -> tree_sitter::Language; tracks grammars used
  chunkfile.rs             chunks_for_file (read, sha1 id, metadata)
  walk.rs                  load_ignore, iter_files (selection predicate), parallel collection
  embed.rs                 Embedder (fastembed local-model init + batch embed)
  store.rs                 Store trait + types (Chunk, Meta, Stats)
  chroma.rs                HttpStore: v2 REST impl of Store (heartbeat, get_or_create, get paginated, add, delete, delete_collection, count)
  oneshot.rs               one_shot_index over &dyn Store
  daemon.rs                build_path_to_ids, watcher+debounce, process_changes, signals
tests/
  parity.rs                gated integration: Python vs Rust ID/metadata parity
  fixtures/                tiny per-language sample files
docs/spec.md  docs/plan.md
```

Dependency order for implementation: `config` → `splitlines` → `store(types)` → `grammar` → `chunk` → `chunkfile` → `walk` → `embed` → `chroma` → `oneshot` → `daemon` → `main` → packaging.

---

### Task 0: Cargo scaffold + CI-free skeleton

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `rust-toolchain.toml`

- [ ] **Step 1: Create `Cargo.toml`** (copy the dependency block verbatim from spec §12; add `[[bin]] name = "index-repo"`):

```toml
[package]
name = "index-repo"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "index-repo"
path = "src/main.rs"

[dependencies]
# (paste the full dependency list from docs/spec.md §12)
```

- [ ] **Step 2: Pin toolchain** — `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Minimal `src/main.rs`:**

```rust
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(0)
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build`
Expected: compiles (deps resolve). If `fastembed`/`ort` fail to link locally, set `ORT_STRATEGY=system ORT_LIB_LOCATION=$(nix eval --raw nixpkgs#onnxruntime)/lib` or develop inside `nix develop` (see Task 13). Record the resolved versions from `Cargo.lock`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml src/main.rs
git commit -m "chore: cargo scaffold"
```

---

### Task 1: `config.rs` constants

**Files:**
- Create: `src/config.rs`
- Test: in-file `#[cfg(test)]`

- [ ] **Step 1: Write failing tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exts_and_special_names() {
        assert!(EXTS.contains(".rs"));
        assert!(EXTS.contains(".tsx"));
        assert!(!EXTS.contains(".png"));
        assert!(SPECIAL_NAMES.contains("Makefile"));
        assert!(SPECIAL_NAMES.contains(".envrc"));
    }
    #[test]
    fn ext_to_lang_maps() {
        assert_eq!(ext_to_lang(".tsx"), Some("tsx"));
        assert_eq!(ext_to_lang(".ts"), Some("typescript"));
        assert_eq!(ext_to_lang(".mjs"), Some("javascript"));
        assert_eq!(ext_to_lang(".png"), None);
    }
    #[test]
    fn semantic_and_scope_sets_present() {
        assert!(semantic_types("rust").contains(&"function_item"));
        assert!(scope_types("python").contains(&"class_definition"));
        assert!(scope_types("go").is_empty());
    }
}
```

- [ ] **Step 2: Run, expect FAIL** — `cargo test config::` → unresolved names.

- [ ] **Step 3: Implement `config.rs`** — define `EXTS: phf::Set` or a `&[&str]` + helper, `SPECIAL_NAMES`, `EXTRA_IGNORE: &[&str]`, `ext_to_lang(&str)->Option<&'static str>`, `semantic_types(&str)->&'static [&'static str]`, `scope_types(&str)->&'static [&'static str]`, and the numeric consts (`CHUNK_LINES=120`, `OVERLAP=20`, `MAX_FILE_BYTES=512*1024`, `MAX_SEMANTIC_LINES=200`, `BATCH=2000`). Copy the data verbatim from spec §2. Prefer plain `match`/`&[..]` over extra crates (KISS).

- [ ] **Step 4: Run, expect PASS** — `cargo test config::`

- [ ] **Step 5: Commit** — `git commit -am "feat: config constants"`

---

### Task 2: `splitlines.rs` (Python-compatible)

**Files:**
- Create: `src/splitlines.rs`

Parity-critical: `line_window` uses Python `str.splitlines()` semantics, NOT Rust `lines()`.

- [ ] **Step 1: Failing tests:**

```rust
#[cfg(test)]
mod tests {
    use super::py_splitlines;
    #[test]
    fn basic_lf() { assert_eq!(py_splitlines("a\nb\nc"), vec!["a","b","c"]); }
    #[test]
    fn no_trailing_empty() { assert_eq!(py_splitlines("a\n"), vec!["a"]); }
    #[test]
    fn crlf_and_cr() { assert_eq!(py_splitlines("a\r\nb\rc"), vec!["a","b","c"]); }
    #[test]
    fn unicode_separators() {
        // U+2028 line separator, U+0085 NEL, vertical tab, form feed
        assert_eq!(py_splitlines("a\u{2028}b\u{0085}c\u{000b}d\u{000c}e"),
                   vec!["a","b","c","d","e"]);
    }
    #[test]
    fn empty() { assert!(py_splitlines("").is_empty()); }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `pub fn py_splitlines(s: &str) -> Vec<&str>` splitting on the full Python universal-newline set `{\n, \r, \r\n, \v(0x0B), \f(0x0C), \x1c, \x1d, \x1e, \u{85}, \u{2028}, \u{2029}}`, treating `\r\n` as one boundary, with no trailing empty element. Iterate `char_indices`, slice on boundaries.

- [ ] **Step 4: Run, expect PASS.**

- [ ] **Step 5: Commit** — `git commit -am "feat: python-compatible splitlines"`

---

### Task 3: `store.rs` shared types + `Store` trait

**Files:**
- Create: `src/store.rs`

- [ ] **Step 1: Define types (no tests; pure types):**

```rust
use std::collections::HashSet;
use anyhow::Result;

pub type Meta = serde_json::Map<String, serde_json::Value>;

#[derive(Clone)]
pub struct Record { pub id: String, pub body: String, pub meta: Meta }

#[derive(Default, Clone)]
pub struct Stats {
    pub files: usize, pub added: usize, pub unchanged: usize, pub deleted: usize,
    pub ts_chunks: usize, pub win_chunks: usize, pub skipped_bin: usize,
}

pub trait Store {
    fn existing_ids(&self) -> Result<HashSet<String>>;
    fn metadatas(&self) -> Result<Vec<(String, Meta)>>;
    fn add(&self, batch: &[Record]) -> Result<usize>;   // embeds internally
    fn delete(&self, ids: &[String]) -> Result<usize>;
    fn count(&self) -> Result<usize>;
}
```

- [ ] **Step 2: Build** — `cargo build`. **Step 3: Commit** — `git commit -am "feat: store trait + shared types"`

---

### Task 4: `grammar.rs`

**Files:**
- Create: `src/grammar.rs`

- [ ] **Step 1: Failing test:**

```rust
#[cfg(test)]
mod tests {
    use super::language_for;
    #[test]
    fn loads_all_langs() {
        for k in ["php","go","javascript","typescript","tsx","python","rust","bash"] {
            assert!(language_for(k).is_some(), "missing grammar {k}");
        }
        assert!(language_for("nope").is_none());
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `pub fn language_for(key: &str) -> Option<tree_sitter::Language>` mapping per spec §2 (`typescript`→`LANGUAGE_TYPESCRIPT`, `tsx`→`LANGUAGE_TSX`, php→full PHP grammar matching Python `language_php`). Also expose a thread-safe used-grammar recorder (e.g. `Mutex<BTreeSet<&'static str>>` or collect at call sites) to produce the `grammars=` log later.

- [ ] **Step 4: Run, expect PASS.** **Step 5: Commit** — `git commit -am "feat: tree-sitter grammar map"`

---

### Task 5: `chunk.rs` — line_window

**Files:**
- Create: `src/chunk.rs`

- [ ] **Step 1: Failing tests** (algorithm fully specified in spec §3.1, so expectations are derivable):

```rust
#[cfg(test)]
mod tests {
    use super::line_window;
    #[test]
    fn windows_with_overlap() {
        let text = (1..=250).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let w = line_window(&text);
        // step = 100, CHUNK_LINES = 120
        assert_eq!(w[0].0, 1);
        assert_eq!(w[1].0, 101);
        assert_eq!(w[2].0, 201);
        assert_eq!(w.len(), 3);
        assert!(w[0].1.starts_with("1\n2\n"));
    }
    #[test]
    fn skips_blank_only_window() {
        assert!(line_window("   \n  \n").is_empty());
    }
    #[test]
    fn single_short() {
        let w = line_window("a\nb");
        assert_eq!(w, vec![(1, "a\nb".to_string())]);
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `pub fn line_window(text: &str) -> Vec<(usize, String)>` exactly per spec §3.1 using `py_splitlines`, `step = max(1, CHUNK_LINES - OVERLAP)`, body via `join("\n")`, skip if `body.trim().is_empty()`, break when `i + CHUNK_LINES >= len`. `line_no = i + 1`.

- [ ] **Step 4: Run, expect PASS.** **Step 5: Commit** — `git commit -am "feat: line_window"`

---

### Task 6: `chunk.rs` — semantic chunking

**Files:**
- Modify: `src/chunk.rs`
- Create: `tests/fixtures/sample.py`, `tests/fixtures/sample.rs`

- [ ] **Step 1: Fixtures** — `tests/fixtures/sample.py`:

```python
import os

class Greeter:
    def hello(self, name):
        return f"hi {name}"

def top_level():
    return 1
```

- [ ] **Step 2: Failing tests:**

```rust
#[cfg(test)]
mod ts_tests {
    use super::{chunk_file, detect_lang};
    use std::path::Path;
    #[test]
    fn python_semantic_scope_and_preamble() {
        let src = include_str!("../tests/fixtures/sample.py");
        let chunks = chunk_file(src, Path::new("sample.py"));
        // import line is preamble; methods/functions are semantic
        let types: Vec<&str> = chunks.iter().map(|c| c.2.as_str()).collect();
        assert!(types.contains(&"preamble"));
        assert!(types.contains(&"function_definition"));
        // method hello has scope "Greeter"
        let hello = chunks.iter().find(|c| c.1.contains("hi {name}")).unwrap();
        assert_eq!(hello.3, "Greeter");
        // top_level has empty scope
        let top = chunks.iter().find(|c| c.1.contains("return 1")).unwrap();
        assert_eq!(top.3, "");
    }
    #[test]
    fn blade_php_is_not_semantic() {
        assert_eq!(detect_lang(Path::new("x.blade.php")), None);
    }
    #[test]
    fn unknown_ext_falls_back_to_window() {
        let chunks = chunk_file("line one\nline two", Path::new("notes.txt"));
        assert!(chunks.iter().all(|c| c.2 == "window"));
    }
}
```

- [ ] **Step 3: Run, expect FAIL.**

- [ ] **Step 4: Implement** `collect_semantic`, `get_scope`, `ts_chunk`, `detect_lang`, `chunk_file` exactly per spec §3.2–3.6. Chunk tuple = `(usize, String, String, String)` = `(line_no, body, node_type, scope)`. Use `tree_sitter::Parser`; decode node text with `String::from_utf8_lossy`. Honor the `end_point.column == 0` decrement, `MAX_SEMANTIC_LINES` split, gap→`preamble`, trailing gap. On parser/parse failure return empty so `chunk_file` falls back to `line_window` with type `"window"`.

- [ ] **Step 5: Run, expect PASS.** Add one more fixture/test for `sample.rs` covering `impl_item` scope (method inside `impl Foo` → scope `Foo`).

- [ ] **Step 6: Commit** — `git commit -am "feat: tree-sitter semantic chunking"`

---

### Task 7: `chunkfile.rs` — ids + metadata

**Files:**
- Create: `src/chunkfile.rs`

- [ ] **Step 1: Generate the SHA1 golden** (run once, paste output into the test):

```bash
python3 -c "import hashlib;print(hashlib.sha1(b'a.py:1:x = 1').hexdigest())"
```

- [ ] **Step 2: Failing tests** (replace `PASTE_HEX` with Step 1 output):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn id_matches_python_sha1() {
        assert_eq!(chunk_id("a.py", 1, "x = 1"), "PASTE_HEX");
    }
    #[test]
    fn meta_has_scope_only_when_present() {
        let m = build_meta("a.py", 3, "py", "function_definition", "Greeter");
        assert_eq!(m["scope"], "Greeter");
        let m2 = build_meta("a.py", 3, "py", "window", "");
        assert!(!m2.contains_key("scope"));
    }
    #[test]
    fn lang_field_rules() {
        assert_eq!(lang_field(Path::new("a.py")), "py");
        assert_eq!(lang_field(Path::new("Makefile")), "Makefile");
        assert_eq!(lang_field(Path::new(".envrc")), ".envrc");
    }
}
```

- [ ] **Step 3: Run, expect FAIL.**

- [ ] **Step 4: Implement** `chunk_id(rel,line,body)` = lowercase hex of `sha1(format!("{rel}:{line}:{body}").as_bytes())`; `build_meta`; `lang_field` per spec §1.2 (`suffix` without leading dots, lowercased, else file name); and `chunks_for_file(path, root) -> (String, Vec<Record>, usize, usize, bool)` per spec §4 (UTF-8 read; invalid→`ok=false`; other I/O error→empty `ok=true`; POSIX `rel`).

- [ ] **Step 5: Run, expect PASS.** **Step 6: Commit** — `git commit -am "feat: chunk ids + metadata"`

---

### Task 8: `walk.rs` — selection + ignore

**Files:**
- Create: `src/walk.rs`

- [ ] **Step 1: Failing tests** (build a temp tree):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn tmp() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("node_modules")).unwrap();
        fs::write(d.path().join("a.rs"), "fn x(){}").unwrap();
        fs::write(d.path().join("node_modules/b.js"), "1").unwrap();
        fs::write(d.path().join("big.rs"), "x".repeat(600*1024)).unwrap();
        fs::write(d.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(d.path().join("ignored.rs"), "fn y(){}").unwrap();
        fs::write(d.path().join("Makefile"), "all:").unwrap();
        fs::write(d.path().join("photo.png"), "x").unwrap();
        d
    }
    #[test]
    fn selection() {
        let d = tmp();
        let spec = load_ignore(d.path());
        let files: Vec<String> = iter_files(d.path(), &spec)
            .iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert!(files.contains(&"a.rs".to_string()));
        assert!(files.contains(&"Makefile".to_string()));
        assert!(!files.contains(&"b.js".to_string()));     // node_modules ignored
        assert!(!files.contains(&"ignored.rs".to_string())); // .gitignore
        assert!(!files.contains(&"big.rs".to_string()));    // > 512K
        assert!(!files.contains(&"photo.png".to_string())); // ext not indexable
    }
}
```

(Add `tempfile = "3"` to `[dev-dependencies]`.)

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** per spec §5: `load_ignore(root)` builds `ignore::gitignore::GitignoreBuilder` from EXTRA_IGNORE then root `.gitignore` lines. `iter_files(root, &spec) -> Vec<PathBuf>` does a plain recursive walk (use `ignore::WalkBuilder::new(root).standard_filters(false).hidden(false)` parallel, or `walkdir`), keeping files where ext∈EXTS or name∈SPECIAL_NAMES, not ignored (`matched_path_or_any_parents(rel,false).is_ignore()` is false), and size ≤ MAX_FILE_BYTES. POSIX-relative path for matching.

- [ ] **Step 4: Run, expect PASS.** **Step 5: Commit** — `git commit -am "feat: file walk + ignore"`

---

### Task 9: `embed.rs` — fastembed local model

**Files:**
- Create: `src/embed.rs`

- [ ] **Step 1: Failing test** (gated on a model dir so CI without the model still builds):

```rust
#[cfg(test)]
mod tests {
    use super::Embedder;
    #[test]
    fn embeds_384_normalized() {
        let dir = match std::env::var("INDEX_REPO_MODEL_DIR") { Ok(d) => d, Err(_) => return };
        let e = Embedder::from_dir(std::path::Path::new(&dir)).unwrap();
        let v = e.embed(&["hello world".to_string()]).unwrap();
        assert_eq!(v[0].len(), 384);
        let norm: f32 = v[0].iter().map(|x| x*x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "not L2-normalized: {norm}");
    }
}
```

- [ ] **Step 2: Run** — `cargo test embed::` (skips without env). With model present: FAIL until implemented.

- [ ] **Step 3: Implement** `Embedder::from_dir(&Path)` reading the 5 files (spec §9) into `UserDefinedEmbeddingModel::new(onnx_bytes, TokenizerFiles{...})` + `TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::default())`; `embed(&self, docs:&[String]) -> Result<Vec<Vec<f32>>>` via `TextEmbedding::embed(docs, None)`. Confirm fastembed default pooling=Mean + normalization; if the user-defined path needs explicit `Pooling::Mean`, set it.

- [ ] **Step 4: Run with model** — `INDEX_REPO_MODEL_DIR=... cargo test embed::` → PASS. (See Task 13 for fetching the model.)

- [ ] **Step 5: Commit** — `git commit -am "feat: fastembed local embedder"`

---

### Task 10: `chroma.rs` — v2 REST `Store`

**Files:**
- Create: `src/chroma.rs`

- [ ] **Step 1: Unit tests for URL/body construction** (pure, no network):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base_url_and_paths() {
        let b = base_url("192.168.1.2", 8000, false);
        assert_eq!(b, "http://192.168.1.2:8000/api/v2");
        assert_eq!(base_url("h", 8000, true), "https://h:8000/api/v2");
    }
    #[test]
    fn collections_path_uses_default_tenant_db() {
        assert_eq!(collections_path(&base_url("h",8000,false)),
          "http://h:8000/api/v2/tenants/default_tenant/databases/default_database/collections");
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `base_url`, `collections_path`, and `HttpStore`:
  - `connect(host,port,ssl) -> Result<()>` GET `/heartbeat` (used by main for exit code 3).
  - `get_or_create(name)` POST collections with `{"name","metadata":{"hnsw:space":"cosine"},"get_or_create":true}` → store `id`.
  - `delete_collection(name)` resolve id then DELETE; swallow errors (for `--full-rebuild`).
  - `existing_ids()` POST `/get {"include":[]}` paginated (limit/offset) → `HashSet`.
  - `metadatas()` POST `/get {"include":["metadatas"]}` paginated → `Vec<(id,meta)>`.
  - `add(batch)` embeds bodies via the owned `Embedder`, POST `/add {ids,embeddings,documents,metadatas}`.
  - `delete(ids)` POST `/delete {"ids":[...]}`.
  - `count()` GET `/count`.
  Reuse one `reqwest::blocking::Client`. `HttpStore` owns `Embedder` (lazy init on first `add`).

- [ ] **Step 4: Run** — `cargo test chroma::` (unit) → PASS.

- [ ] **Step 5: Gated live smoke test** in `tests/parity.rs` (see Task 14) — not here.

- [ ] **Step 6: Commit** — `git commit -am "feat: chromadb v2 rest store"`

---

### Task 11: `oneshot.rs`

**Files:**
- Create: `src/oneshot.rs`

- [ ] **Step 1: Failing test with a mock Store:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    struct Mock { existing: HashSet<String>, added: RefCell<Vec<String>>, deleted: RefCell<Vec<String>> }
    impl Store for Mock {
        fn existing_ids(&self) -> anyhow::Result<HashSet<String>> { Ok(self.existing.clone()) }
        fn metadatas(&self) -> anyhow::Result<Vec<(String,Meta)>> { Ok(vec![]) }
        fn add(&self, b: &[Record]) -> anyhow::Result<usize> { self.added.borrow_mut().extend(b.iter().map(|r| r.id.clone())); Ok(b.len()) }
        fn delete(&self, ids: &[String]) -> anyhow::Result<usize> { self.deleted.borrow_mut().extend_from_slice(ids); Ok(ids.len()) }
        fn count(&self) -> anyhow::Result<usize> { Ok(0) }
    }
    #[test]
    fn adds_new_keeps_unchanged_deletes_stale() {
        // fixture dir with one known file whose ids we precompute
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.py"), "def f():\n    return 1\n").unwrap();
        let id = crate::chunkfile::chunk_id("a.py", 1, "def f():\n    return 1");
        let mock = Mock { existing: HashSet::from(["STALE".into(), id.clone()]),
                          added: RefCell::new(vec![]), deleted: RefCell::new(vec![]) };
        let stats = one_shot_index(&mock, d.path()).unwrap();
        assert_eq!(stats.unchanged, 1);
        assert!(mock.deleted.borrow().contains(&"STALE".to_string()));
        assert_eq!(stats.files, 1);
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `one_shot_index(store: &dyn Store, root: &Path) -> Result<Stats>` per spec §6: fetch existing ids (on error log the spec §10.4 warning and treat empty), iterate `iter_files`, accumulate `seen`, buffer new records, flush at `BATCH`, then delete `existing - seen` in `BATCH` batches. Returns `Stats`.

- [ ] **Step 4: Run, expect PASS.** **Step 5: Commit** — `git commit -am "feat: one-shot incremental index"`

---

### Task 12: `daemon.rs`

**Files:**
- Create: `src/daemon.rs`

- [ ] **Step 1: Failing test for `process_changes` with the mock Store** (drive the pure delta logic directly, no real fs watch):

```rust
#[cfg(test)]
mod tests {
    // reuse Mock from oneshot tests (move Mock to a shared test helper module).
    #[test]
    fn delete_then_upsert_delta() {
        // 1) seed path_to_ids for "a.py" with one id; deleting the file removes it.
        // 2) modifying the file adds new ids, deletes stale, updates path_to_ids.
        // assert added/deleted counts and all_ids membership.
    }
}
```

Flesh out the body using the same Mock + a tempdir; assert the spec §7 invariants.

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** per spec §7: `build_path_to_ids(store) -> HashMap<String,HashSet<String>>` from `store.metadatas()`; `process_changes(store, root, changes, &mut path_to_ids, &mut all_ids) -> (added, deleted)`; and `run_daemon(store, root, debounce_ms) -> Result<i32>` using `notify-debouncer-full` (debounce = `debounce_ms`), filtering events with the `iter_files` predicate, mapping removed→delete / created|modified|renamed→upsert, and installing SIGTERM/SIGINT/SIGHUP handlers (e.g. `signal-hook`) to set an `AtomicBool` stop flag. Wrap each store call so failures log spec §10.4 messages and are swallowed; watch-loop crash → return 4; normal stop → log `daemon: stopped`, return 0. Emit the spec §10.4 daemon logs.

- [ ] **Step 4: Run, expect PASS.** **Step 5: Commit** — `git commit -am "feat: daemon live indexer"`

---

### Task 13: `main.rs` CLI + wiring

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Failing tests** (CLI parse + not-a-dir):

```rust
#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;
    #[test]
    fn defaults() {
        let a = Args::parse_from(["index-repo"]);
        assert_eq!(a.host, "192.168.1.2");
        assert_eq!(a.port, 8000);
        assert_eq!(a.debounce, 800);
        assert_eq!(a.path, ".");
        assert!(!a.daemon && !a.full_rebuild && !a.ssl);
    }
}
```

- [ ] **Step 2: Run, expect FAIL.**

- [ ] **Step 3: Implement** `Args` (clap derive) per spec §10.1; `main` per spec §10.2: resolve root (canonicalize), not-a-dir → print spec §10.2 message, return 2; compute collection `code-<basename>`; print the `indexing ...` line; `HttpStore::connect` → on failure print spec §10.2 message, return 3; `--full-rebuild` → delete collection; get_or_create; if daemon → `run_daemon`; else `one_shot_index` then print the spec §10.3 `done.` summary (with `grammars=` from the recorder and `count=` from `store.count()`); return 0. Map all exit paths to `ExitCode`.

- [ ] **Step 4: Run, expect PASS** — `cargo test`. Then `cargo clippy -- -D warnings` and `cargo fmt --check`.

- [ ] **Step 5: Commit** — `git commit -am "feat: cli + main wiring"`

---

### Task 14: `flake.nix` — package, model FOD, devShell

**Files:**
- Create: `flake.nix`

- [ ] **Step 1: Model FOD** — derivation fetching the 5 files of `Qdrant/all-MiniLM-L6-v2-onnx` (spec §9). Use `fetchurl` per file into a shared dir, or a single `fetchzip`/`runCommand` assembling the flat layout. Pin sha256 (fill via `nix build .#model` then paste the reported hash). Example skeleton:

```nix
model = pkgs.runCommand "all-MiniLM-L6-v2-onnx" { } ''
  mkdir -p $out
  cp ${pkgs.fetchurl { url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/model.onnx"; hash = "sha256-AAAA..."; }} $out/model.onnx
  # repeat for tokenizer.json, config.json, special_tokens_map.json, tokenizer_config.json
'';
```

- [ ] **Step 2: Package** — `rustPlatform.buildRustPackage` with `cargoLock.lockFile = ./Cargo.lock`, `nativeBuildInputs = [ pkg-config ]`, `buildInputs = [ onnxruntime openssl stdenv.cc.cc.lib ]`, env `ORT_STRATEGY="system"`, `ORT_LIB_LOCATION="${onnxruntime}/lib"`, `CARGO_NET_OFFLINE="1"`. Wrap the binary (`makeWrapper`/`wrapProgram`) to export `INDEX_REPO_MODEL_DIR=${model}` and, if needed at runtime, `ORT_DYLIB_PATH=${onnxruntime}/lib/libonnxruntime.so`. Expose `packages.default` and `packages.index-repo`.

- [ ] **Step 3: devShell** — `mkShell` with rust toolchain + the same `ORT_*` env + `INDEX_REPO_MODEL_DIR=${model}` so `cargo test` (incl. embed + parity) works in `nix develop`.

- [ ] **Step 4: Verify build**

Run: `nix build .#index-repo 2>&1` (fill the FOD hashes when Nix reports mismatches; rebuild).
Expected: a `result/bin/index-repo` that runs `index-repo --help`.

- [ ] **Step 5: Commit** — `git commit -am "build: nix flake (package + model FOD + devshell)"`

---

### Task 15: `tests/parity.rs` — decisive Python↔Rust parity (gated)

**Files:**
- Create: `tests/parity.rs`
- Create: `tests/parity/compare.py`

- [ ] **Step 1:** Gate the test behind `CHROMA_TEST=1` (needs the live server at 192.168.1.2:8000). The test:
  1. picks a fixture corpus (e.g. `tests/fixtures` or a path from `PARITY_CORPUS`),
  2. runs the **Python** reference `index_repo.py --collection parity_py_<rand> <corpus>`,
  3. runs the **Rust** `index-repo --collection parity_rs_<rand> <corpus>`,
  4. fetches ids+metadata from both collections (via `compare.py` using the `chromadb` client) and asserts: identical ID sets, and for each id identical `(path,line,type,scope,lang)`.
  5. cleans up both collections.

- [ ] **Step 2:** `compare.py` connects with `chromadb.HttpClient`, `col.get(include=["metadatas"])` for both, diffs sets + per-id metadata, exits non-zero on any mismatch and prints the first 20 diffs.

- [ ] **Step 3: Run** — `CHROMA_TEST=1 PARITY_CORPUS=/home/labile/nixos cargo test --test parity -- --nocapture`
Expected: PASS (identical IDs + metadata). Investigate any diff against spec §3–5 (likely splitlines or ignore edge cases).

- [ ] **Step 4: Commit** — `git commit -am "test: python-parity integration harness"`

---

### Task 16: Benchmark

- [ ] **Step 1:** With a warm collection, time both on the same corpus:

```bash
hyperfine --warmup 1 \
  'python3 /home/labile/nixos/home-manager/modules/opencode/scripts/index_repo.py --collection bench /home/labile/nixos' \
  './result/bin/index-repo --collection bench /home/labile/nixos'
```

- [ ] **Step 2:** Repeat for cold path (`--full-rebuild`, separate throwaway collection). Record warm + cold numbers in the PR description. (Expect a large warm-path win from parallel walk/parse + no uv/python startup; modest cold-path win bounded by onnxruntime.)

---

## Integration (nixos repo — separate change in /home/labile/nixos)

Not a subagent task in this repo; performed after the binary builds green. Use the nix-routing skill + docs/routes.md; output full files; pass the verification gate.

- [ ] Add `index-repo` as a **flake input** (private repo → `git+ssh://git@github.com/labi-le/index-repo`) in the nixos `flake.nix`; thread it to the opencode module.
- [ ] `home-manager/modules/opencode/default.nix`: replace the `writeTextFile` `indexerPkg` with `inputs.index-repo.packages.${system}.default` (binary name stays `index-repo`).
- [ ] `home-manager/modules/opencode/wrappers.nix`: unchanged (still `index-repo --daemon "$PWD"` under `setsid`). Verify.
- [ ] `home-manager/modules/opencode/packages.nix`: keep `uv` + `nodejs`; the python indexer deps vanish with the script.
- [ ] Delete `home-manager/modules/opencode/scripts/index_repo.py`.
- [ ] Gate: `nix build .#nixosConfigurations.pc.config.system.build.toplevel --dry-run` → no new eval/deprecation warnings (Git-tree-dirty excluded). Then a real switch + one-time `index-repo --full-rebuild` cutover.

---

## Self-Review (against spec)

- **Spec coverage:** §1 ids/meta → T7; §2 constants → T1; §3 chunking → T2/T5/T6; §4 chunkfile → T7; §5 walk/ignore → T8; §6 oneshot → T11; §7 daemon → T12; §8 chroma → T10; §9 embed → T9; §10 CLI/logs/codes → T13; §12 deps → T0; §13 nix → T14 + Integration; §14 verification → T15/T16 + Integration gate. No uncovered section.
- **Placeholders:** none — every code step has concrete code; the only deferred values are the two reference-generated goldens (SHA1 hex in T7, FOD hashes in T14), each with the exact command to produce it.
- **Type consistency:** chunk tuple `(usize,String,String,String)` and `Record{id,body,meta}` / `Store` trait are used identically across T5–T13.
