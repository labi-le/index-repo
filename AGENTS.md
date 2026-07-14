# AGENTS.md — index-repo

Operational cheat-sheet for coding agents. The design/behavior contract is in
`docs/spec.md`; this file is the "don't burn an hour rediscovering it" layer.

## Build / test / lint — MUST go through the nix devShell

`cargo`/`rustc`/`clippy`/`rustfmt` are **not on PATH**. The flake devShell also
bakes the ONNX runtime + model env (`ORT_DYLIB_PATH`, `INDEX_REPO_MODEL_DIR`)
the embedder needs, so run everything inside it:

```sh
nix develop --command cargo test --lib
nix develop --command bash -c 'cargo fmt && cargo clippy -- -D warnings && cargo test --lib'
```

- First `nix develop` may fetch the toolchain from the binary cache; then warm.
- Clippy is **lib+bin only** (project convention). `--all-targets` surfaces a
  pre-existing `useless_vec` in old test code — not worth chasing.
- `embed::` / `lazy::` tests actually load the ONNX model from the devShell env
  (pass in-shell, skip when the env is unset). Non-embed tests are pure/fast.

## Architecture (fast map)

- **`serve`** (the deployed path, `service.rs`): one dispatcher + N per-root
  **actors** sharing one lazy embedder. Each actor owns an `HttpStore`, runs
  `one_shot_index`, then applies debounced fs-event batches.
- Legacy `--daemon` (`daemon.rs`) and one-shot CLI (`oneshot.rs`) predate
  `serve` and are parity-locked — prefer changing `serve`.
- **Registry** (`registry.rs`): roots are **PID-keyed marker files** at
  `$XDG_RUNTIME_DIR/index-repo/roots/<hash>.<pid>`, GC'd the instant that PID
  dies. So `index-repo register <dir>` then exiting leaves **no** live root — a
  long-lived process (opencode/omp) must hold it. To smoke `serve` with a root:
  `register`, then rename the marker's `.<pid>` suffix to a live PID.
- **Collection name** = `code-<owner>-<repo>` from the git remote
  (`config.rs::collection_name`), mirrored byte-for-byte in
  `hooks/opencode/chroma-gate.ts`. Change one → change both.
- **Parity**: v0.1 is byte-for-byte with Python `index_repo.py`. `docs/spec.md
  §1` is the law; §16 lists the intentional deviations. Don't touch the
  chunk-id / embedding path without updating §16.

## ChromaDB v2 REST — sharp edges (live server `192.168.1.2:8000`, no auth)

- **Delete a collection by NAME, not id**: `DELETE …/collections/{name}`.
  Delete-by-id returns 404 and silently no-ops on this build.
- **Metadata is overwritten wholesale**: `PUT …/collections/{id}`
  `{"new_metadata":{…}}` replaces the entire dict — always send every key. The
  hnsw config is a separate field and is untouched.
- List: `GET …/collections?limit=N` → `[{id,name,metadata|null,…}]`.
- Names: 3–512 chars `[a-zA-Z0-9._-]`, must start/end alphanumeric.
- Full contract: `docs/spec.md §8`.

## Testing

- **Live ChromaDB tests are gated on `CHROMA_TEST=1`** and need the real server
  (`tests/parity.rs`, `tests/gc_e2e.rs`); they skip silently otherwise.
- **Global-state race**: `grammar::USED_GRAMMARS` is process-global. Tests that
  `reset` it or assert emptiness race under parallel `cargo test`. Assert only
  presence (inserts are monotonic); never reset in a shared test.
- Prefer pure fns + `testkit::MockStore` over the network for unit tests
  (`gc_decide`, `gc_sweep`, `parse_collection_list`).

## Deploying source changes to the running daemon

Workstations (pc/fx516/notebook) build the daemon **`fromSource`** via the
`index-repo` flake input pinned in `/home/labile/nix/flake.lock`. Pushing to
`main` does **not** auto-deploy. To ship:

```sh
git push                                   # in THIS repo
cd /home/labile/nix
nix flake update index-repo
sudo nixos-rebuild switch --flake .#pc --impure \
  --cores "$(nproc)" --option connect-timeout 5 --option stalled-download-timeout 20
git add flake.lock && git commit -m 'flake: bump index-repo' && git push
```

- The `--option …-timeout` flags dodge the
  cache.nixos.org substituter hangs that otherwise deadlock the rebuild.
- `services.index-repo` runs only on `withHomeManager` hosts (pc/fx516/notebook),
  **not** `server` (server is just the ChromaDB host).
- Verify: `HOME=/home/labile XDG_RUNTIME_DIR=/run/user/1000 systemctl --user
  status index-repo` and `journalctl --user -u index-repo -n 20`.

## Misc gotchas

- No CI gate runs fmt/clippy/tests on push (only release/flake-update
  workflows) — **you** own verification.
- `rust-toolchain.toml` says `stable`, but the devShell uses nixpkgs rustfmt;
  formatting can drift between the two. Always fmt inside the devShell.
