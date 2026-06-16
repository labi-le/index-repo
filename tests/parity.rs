/// Python ↔ Rust chunk-ID / metadata parity test.
///
/// Gated on `CHROMA_TEST=1` — skips silently otherwise.
/// Requires a live ChromaDB at 192.168.1.2:8000 and `uv` on PATH.
#[test]
fn parity() {
    if std::env::var("CHROMA_TEST").as_deref() != Ok("1") {
        eprintln!("parity: skipping (CHROMA_TEST != 1)");
        return;
    }

    let corpus = std::env::var("PARITY_CORPUS").unwrap_or_else(|_| "tests/fixtures".to_string());
    let index_bin =
        std::env::var("INDEX_REPO_BIN").unwrap_or_else(|_| "./result/bin/index-repo".to_string());
    let py_indexer = std::env::var("PYTHON_INDEXER").unwrap_or_else(|_| {
        "/home/labile/nixos/home-manager/modules/opencode/scripts/index_repo.py".to_string()
    });

    // Generate unique collection names from timestamp + pid.
    let suffix = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let pid = std::process::id();
        format!("{nanos}_{pid}")
    };
    let py_col = format!("parity_py_{suffix}");
    let rs_col = format!("parity_rs_{suffix}");

    eprintln!("parity: corpus={corpus}");
    eprintln!("parity: py_collection={py_col}  rs_collection={rs_col}");

    // ---- Run Python indexer ----
    eprintln!("parity: running Python indexer...");
    let py_status = std::process::Command::new("uv")
        .args([
            "run",
            &py_indexer,
            "--host",
            "192.168.1.2",
            "--port",
            "8000",
            "--collection",
            &py_col,
            &corpus,
        ])
        .status()
        .expect("failed to spawn uv / Python indexer");
    assert!(
        py_status.success(),
        "Python indexer failed with {py_status}"
    );

    // ---- Run Rust indexer ----
    eprintln!("parity: running Rust indexer...");
    let rs_status = std::process::Command::new(&index_bin)
        .args([
            "--host",
            "192.168.1.2",
            "--port",
            "8000",
            "--collection",
            &rs_col,
            &corpus,
        ])
        .status()
        .expect("failed to spawn Rust indexer");
    assert!(rs_status.success(), "Rust indexer failed with {rs_status}");

    // ---- Run compare.py ----
    eprintln!("parity: running compare.py...");
    // Work out the path to compare.py relative to crate root.
    let compare_py = {
        let manifest = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
        );
        manifest.join("tests/parity/compare.py")
    };

    let cmp_status = std::process::Command::new("uv")
        .args(["run", compare_py.to_str().unwrap(), &py_col, &rs_col])
        .status()
        .expect("failed to spawn compare.py");

    assert!(
        cmp_status.success(),
        "compare.py reported parity FAIL (exit {cmp_status}) — see output above"
    );
}
