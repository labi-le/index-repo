# index-repo

Rust port of the semantic code indexer for ChromaDB (tree-sitter AST chunking +
`all-MiniLM-L6-v2` embeddings). Drop-in replacement for the previous
`uv`-run Python script `index_repo.py`, with identical observable behavior and
faster warm/incremental scans.

Status: **design stage**. See [`docs/spec.md`](docs/spec.md) for the authoritative
design and behavioral-parity contract, and [`docs/plan.md`](docs/plan.md) once the
implementation plan is written.

Consumed by the NixOS/home-manager `opencode` module as the `index-repo` binary
(`index-repo --daemon $PWD` live indexer + one-shot CLI).
