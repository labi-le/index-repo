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
