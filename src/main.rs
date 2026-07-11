use clap::{Args, Parser, Subcommand};
use index_repo::registry::Registry;
use index_repo::service;
use index_repo::store::Store as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    about = "Semantic code indexer for ChromaDB using tree-sitter AST parsing.",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    legacy: LegacyArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the shared always-on service (single shared model, all active roots).
    Serve {
        /// ChromaDB host
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// ChromaDB port
        #[arg(long, default_value_t = 8000)]
        port: u16,
        /// Use HTTPS
        #[arg(long, default_value_t = false)]
        ssl: bool,
        /// Fs-event debounce window in ms
        #[arg(long, default_value_t = 800)]
        debounce: u64,
    },
    /// Register a repo root with the running service.
    Register {
        path: String,
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Unregister a repo root.
    Unregister {
        path: String,
        #[arg(long)]
        pid: Option<u32>,
    },
}

/// The original flat CLI (one-shot / `--daemon`), preserved verbatim for parity
/// and manual use.
#[derive(Args, Debug)]
struct LegacyArgs {
    /// Project directory to index (default: current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// ChromaDB host
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// ChromaDB port
    #[arg(long, default_value_t = 8000)]
    pub port: u16,

    /// Collection name (default: code-<basename>-<hash8>)
    #[arg(long)]
    pub collection: Option<String>,

    /// Use HTTPS
    #[arg(long, default_value_t = false)]
    pub ssl: bool,

    /// Drop the collection and re-embed everything
    #[arg(long, default_value_t = false)]
    pub full_rebuild: bool,

    /// Run as a long-lived live indexer
    #[arg(long, default_value_t = false)]
    pub daemon: bool,

    /// Daemon fs-event debounce window in ms
    #[arg(long, default_value_t = 800)]
    pub debounce: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: anyhow::Result<ExitCode> = match cli.command {
        Some(Command::Serve {
            host,
            port,
            ssl,
            debounce,
        }) => service::run_serve(&host, port, ssl, debounce).map(|c| ExitCode::from(c as u8)),
        Some(Command::Register { path, pid }) => {
            let pid = pid.unwrap_or_else(std::process::id);
            Registry::from_env()
                .register(Path::new(&path), pid)
                .map(|()| ExitCode::SUCCESS)
        }
        Some(Command::Unregister { path, pid }) => {
            let pid = pid.unwrap_or_else(std::process::id);
            Registry::from_env()
                .unregister(Path::new(&path), pid)
                .map(|()| ExitCode::SUCCESS)
        }
        None => legacy_run(cli.legacy),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn legacy_run(args: LegacyArgs) -> anyhow::Result<ExitCode> {
    let root: PathBuf = std::fs::canonicalize(&args.path).unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&args.path)
    });

    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return Ok(ExitCode::from(2));
    }

    let collection_name = args
        .collection
        .clone()
        .unwrap_or_else(|| index_repo::config::collection_name(&root));

    let mode_str = if args.daemon {
        "daemon"
    } else if args.full_rebuild {
        "full rebuild"
    } else {
        "incremental"
    };

    eprintln!(
        "indexing {} \u{2192} {}:{}  collection={} mode={}",
        root.display(),
        args.host,
        args.port,
        collection_name,
        mode_str
    );

    let mut store = index_repo::chroma::HttpStore::new(&args.host, args.port, args.ssl);
    if let Err(e) = store.heartbeat() {
        eprintln!(
            "error: cannot reach chromadb at {}:{} ({})\nis `systemctl status chromadb` running?",
            args.host, args.port, e
        );
        return Ok(ExitCode::from(3));
    }

    let spec = index_repo::walk::load_ignore(&root);

    if args.full_rebuild {
        let _ = store.delete_collection(&collection_name);
    }

    store.get_or_create(&collection_name)?;

    let embedder = index_repo::embed::Embedder::from_env()?;

    if args.daemon {
        let code =
            index_repo::daemon::run_daemon(&mut store, &embedder, &root, &spec, args.debounce)?;
        return Ok(ExitCode::from(code as u8));
    }

    let stats = index_repo::oneshot::one_shot_index(&mut store, &embedder, &root, &spec)?;
    let grammars = index_repo::grammar::used_grammars_str();
    let count = store.count()?;

    eprintln!(
        "done. files={} added={} unchanged={} deleted={} \
         (tree-sitter={}, window={}) skipped_binary={} grammars={} \
         collection={} count={}",
        stats.files,
        stats.added,
        stats.unchanged,
        stats.deleted,
        stats.ts_chunks,
        stats.win_chunks,
        stats.skipped_bin,
        grammars,
        collection_name,
        count
    );

    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn defaults() {
        let cli = Cli::parse_from(["index-repo"]);
        assert!(cli.command.is_none());
        let a = cli.legacy;
        assert_eq!(a.host, "127.0.0.1");
        assert_eq!(a.port, 8000);
        assert_eq!(a.debounce, 800);
        assert_eq!(a.path, ".");
        assert!(!a.daemon);
        assert!(!a.full_rebuild);
        assert!(!a.ssl);
        assert!(a.collection.is_none());
    }

    #[test]
    fn all_flags_parsed() {
        let cli = Cli::parse_from([
            "index-repo",
            "/some/path",
            "--host",
            "10.0.0.1",
            "--port",
            "9000",
            "--collection",
            "my-col",
            "--ssl",
            "--full-rebuild",
            "--daemon",
            "--debounce",
            "400",
        ]);
        assert!(cli.command.is_none());
        let a = cli.legacy;
        assert_eq!(a.path, "/some/path");
        assert_eq!(a.host, "10.0.0.1");
        assert_eq!(a.port, 9000);
        assert_eq!(a.collection.as_deref(), Some("my-col"));
        assert!(a.ssl);
        assert!(a.full_rebuild);
        assert!(a.daemon);
        assert_eq!(a.debounce, 400);
    }

    #[test]
    fn not_a_dir_returns_code_2() {
        let a = LegacyArgs {
            path: "/tmp/this_path_definitely_does_not_exist_xyz_12345".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9999,
            collection: None,
            ssl: false,
            full_rebuild: false,
            daemon: false,
            debounce: 800,
        };
        let result = legacy_run(a).unwrap();
        assert_eq!(result, ExitCode::from(2));
    }

    #[test]
    fn serve_subcommand_parses_with_defaults() {
        let cli = Cli::parse_from(["index-repo", "serve"]);
        match cli.command {
            Some(Command::Serve {
                host,
                port,
                ssl,
                debounce,
            }) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8000);
                assert!(!ssl);
                assert_eq!(debounce, 800);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn serve_subcommand_parses_options() {
        let cli = Cli::parse_from([
            "index-repo",
            "serve",
            "--host",
            "10.0.0.5",
            "--port",
            "9000",
            "--ssl",
            "--debounce",
            "200",
        ]);
        match cli.command {
            Some(Command::Serve {
                host,
                port,
                ssl,
                debounce,
            }) => {
                assert_eq!(host, "10.0.0.5");
                assert_eq!(port, 9000);
                assert!(ssl);
                assert_eq!(debounce, 200);
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn register_subcommand_parses_path_and_pid() {
        let cli = Cli::parse_from(["index-repo", "register", "/x", "--pid", "42"]);
        match cli.command {
            Some(Command::Register { path, pid }) => {
                assert_eq!(path, "/x");
                assert_eq!(pid, Some(42));
            }
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[test]
    fn unregister_subcommand_parses() {
        let cli = Cli::parse_from(["index-repo", "unregister", "/y"]);
        match cli.command {
            Some(Command::Unregister { path, pid }) => {
                assert_eq!(path, "/y");
                assert_eq!(pid, None);
            }
            other => panic!("expected Unregister, got {other:?}"),
        }
    }

    #[test]
    fn bare_args_still_parse_as_legacy() {
        let cli = Cli::parse_from(["index-repo", "/some/path", "--daemon"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.legacy.path, "/some/path");
        assert!(cli.legacy.daemon);
    }
}
