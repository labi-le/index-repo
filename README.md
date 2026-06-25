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

- **Warm path (3.76×):** parallel walk/parse/hash via `rayon` + no uv/Python startup overhead. The incremental diff (fetch-existing-ids → set-diff → no-op add) is the dominant cost; Rust does it faster with less GC pressure.
- **Cold path (37×):** fastembed's onnxruntime batching is dramatically more efficient than chromadb's default Python embedding function for 206 chunks. The Python client embeds client-side via `chromadb`'s built-in EF (also onnxruntime, but single-threaded and with Python overhead per batch); fastembed uses multi-threaded ort internally.

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
- The collection name is derived at **runtime** as
  `code-<basename of workspace root>` — exactly the scheme the indexer uses
  (`code-{}` of `root.file_name()`), so the hint always matches the live
  collection without per-project configuration.

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

The plugin computes the collection from the workspace basename, so no further
configuration is required — start an agent in the indexed repo and it will be
told to query `code-<basename>` first.
