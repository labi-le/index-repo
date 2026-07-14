# index-repo

Rust port of the semantic code indexer for ChromaDB (tree-sitter AST chunking +
`all-MiniLM-L6-v2` embeddings). Drop-in replacement for the previous
`uv`-run Python script `index_repo.py`, with identical observable behavior and
faster warm/incremental scans.

Consumed by the NixOS/home-manager `opencode` module as the `index-repo` binary
(`index-repo --daemon $PWD` live indexer + one-shot CLI).

See [`docs/spec.md`](docs/spec.md) for the behavioral-parity contract and
[`docs/plan.md`](docs/plan.md) for the implementation plan.

---

## Benchmarks

**Corpus:** `/home/labile/nixos` — 151 files, 206 chunks (36 tree-sitter + 170 window).
**Machine:** x86_64-linux, ChromaDB at 192.168.1.2:8000.
**Tool:** `hyperfine` (warm=1, runs=8 warm / runs=3 cold).
**Python baseline:** `uv run index_repo.py` (PEP 723 inline deps, uv cache warm).
**Rust binary:** `./result/bin/index-repo` (Nix-wrapped, ORT + model env baked in).

### Warm path (incremental re-index, all chunks unchanged)

| Indexer | Mean ± σ | Min | Max |
|---------|----------|-----|-----|
| Python (`uv run`) | 1.030 s ± 0.141 s | 0.960 s | 1.378 s |
| Rust (`index-repo`) | 274.2 ms ± 16.4 ms | 259.3 ms | 302.3 ms |

**Speedup: 3.76× ± 0.56×** (hyperfine summary)

### Cold path (`--full-rebuild`, delete + re-embed all 206 chunks)

| Indexer | Mean ± σ | Min | Max |
|---------|----------|-----|-----|
| Python (`uv run`) | 10.469 s ± 0.154 s | 10.292 s | 10.572 s |
| Rust (`index-repo`) | 280.9 ms ± 13.0 ms | 270.1 ms | 295.4 ms |

**Speedup: 37.27× ± 1.81×** (hyperfine summary)

### Why

- **Warm path (3.76×):** no uv/Python interpreter startup plus a native single-pass walk/parse/hash. The incremental diff (fetch-existing-ids → set-diff → no-op add) dominates the warm cost; Rust does it with far less allocation and no GC pressure.
- **Cold path (37×):** fastembed's onnxruntime batching is dramatically more efficient than chromadb's default Python embedding function for 206 chunks. The Python client embeds client-side via `chromadb`'s built-in EF (also onnxruntime, but single-threaded and with Python overhead per batch); fastembed uses multi-threaded ort internally.

---

## Configuration

Configured by CLI flags (`--host`, `--port`, `--ssl`, `--collection`,
`--full-rebuild`, `--daemon`, `--debounce`) and environment variables:

| Env var | Default | Purpose |
|---------|---------|---------|
| `INDEX_REPO_MODEL_DIR` | (Nix wrapper) | Directory of the ONNX model + tokenizer files. Point at another model to swap embedders — **the query path (chroma-mcp) must use the same model** or vectors diverge. |
| `INDEX_REPO_CHROMA_TOKEN` | (unset) | Static token sent as `Authorization: Bearer <token>` on every ChromaDB request. Unset → unauthenticated. |
| `INDEX_REPO_MAX_FILE_BYTES` | `524288` | Max indexable file size in bytes. |
| `INDEX_REPO_MAX_LENGTH` | `256` | Embedding token truncation length. `256` byte-matches chromadb's default EF; raise only if the query path matches. |
| `INDEX_REPO_INTRA_THREADS` | `4` | ONNX intra-op threads. |
| `INDEX_REPO_EMBED_BATCH` | `32` | Embedding batch size. |
| `INDEX_REPO_POOLING` | `mean` | Token pooling: `mean` or `cls`. |
| `INDEX_REPO_TTL_DAYS` | `30` | Serve daemon drops collections not indexed within this many days. `0` disables GC. |
| `INDEX_REPO_GC_DRY_RUN` | (unset) | `1`/`true` → GC logs what it would drop without deleting. |

Default host is `127.0.0.1`; set `--host` (or NixOS `services.index-repo.host`)
for a remote ChromaDB.

The `serve` daemon garbage-collects collections whose repo hasn't been indexed
(opened or edited) in `INDEX_REPO_TTL_DAYS` days (default 30); set `0` to
disable, or `INDEX_REPO_GC_DRY_RUN=1` to preview.

### Languages

Tree-sitter AST chunking covers **Python, JavaScript, TypeScript, TSX, Rust, Go,
PHP, Bash, Java, C, C++, C#, Ruby**. Every other indexable extension falls back
to fixed 120-line overlapping windows.

---

## OpenCode integration

The repo ships an [OpenCode](https://opencode.ai) plugin at
[`hooks/opencode/chroma-gate.ts`](hooks/opencode/chroma-gate.ts) that steers
agents toward the index this daemon builds:

- Injects a system rule: **call `chroma_query_documents` first**, before
  `grep`/`glob`.
- **Blocks** unscoped `grep`/`glob` for a fixed set of agents
  (`build`, `orchestrator`, `general`, `explore`, `explorer`, `plan`) until a
  chroma query has run in the session. Narrowed searches (a concrete
  `path` + `include` for grep, or a concrete `path`/`pattern` for glob) are
  always allowed.
- The collection name is resolved at **runtime** to match the indexer exactly:
  `code-<owner>-<repo>` from the repo's git `origin` remote (stable across
  machines/clones), or `code-<basename>-<hash8>` when there is no git remote — so
  the hint always matches the live collection and repos never collide.
- Enforcement is configurable: `CHROMA_GATE_ENFORCE=0` disables blocking (the
  system-rule hint still injects); `CHROMA_GATE_AGENTS=a,b,c` overrides the
  enforced-agent set.

### Install (Nix / home-manager)

The flake's `homeManagerModules.default` deploys the plugin and (optionally)
registers the `chroma` MCP server. Add the module to your home-manager config
(e.g. via `sharedModules` or `imports`) and enable it:

```nix
{
  # Deploy the chroma-gate plugin to ~/.config/opencode/plugins/chroma-gate.ts
  services.index-repo.opencode.chromaGate.enable = true;

  # Optional: also register the `chroma` MCP server in opencode. Host/port/ssl
  # default to the NixOS `services.index-repo.{host,port,ssl}` of this indexer,
  # so they stay in sync automatically. Needs `uvx` (uv) on PATH for chroma-mcp.
  services.index-repo.opencode.chromaMcp = {
    enable = true;
    # host = "192.168.1.2";  # override if your ChromaDB is elsewhere
    # port = 8000;
    # ssl  = false;
  };
}
```

`chromaMcp` writes `programs.opencode.settings.mcp.chroma`, so it requires the
home-manager `programs.opencode` module to be present.

### Install (manual / non-Nix)

1. Copy the plugin into your opencode plugins dir:

   ```sh
   mkdir -p ~/.config/opencode/plugins
   cp hooks/opencode/chroma-gate.ts ~/.config/opencode/plugins/
   ```

2. Register a `chroma` MCP server in your opencode config
   (`~/.config/opencode/opencode.json`), pointed at the same ChromaDB the
   indexer writes to:

   ```json
   {
     "mcp": {
       "chroma": {
         "type": "local",
         "command": ["uvx", "chroma-mcp", "--client-type", "http",
                     "--host", "127.0.0.1", "--port", "8000", "--ssl", "false"],
         "enabled": true
       }
     }
   }
   ```

The plugin resolves the collection the same way the indexer does — from the git
`origin` remote (`code-<owner>-<repo>`), falling back to `code-<basename>-<hash8>`
— so no configuration is needed: start an agent in the indexed repo and it will
be told to query that collection first.
