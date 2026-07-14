use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use sha1::{Digest, Sha1};

pub const CHUNK_LINES: usize = 120;
pub const OVERLAP: usize = 20;
/// Max indexable file size in bytes. Overridable via `INDEX_REPO_MAX_FILE_BYTES`.
pub static MAX_FILE_BYTES: LazyLock<u64> = LazyLock::new(|| {
    std::env::var("INDEX_REPO_MAX_FILE_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(512 * 1024)
});
pub const MAX_SEMANTIC_LINES: usize = 200;
pub const BATCH: usize = 2000;

/// Collection TTL in seconds — the serve daemon GC-drops index_repo-owned
/// collections not indexed within this window. Overridable via
/// `INDEX_REPO_TTL_DAYS` (default 30); `0` disables GC.
pub static TTL_SECS: LazyLock<u64> =
    LazyLock::new(|| ttl_secs_from(std::env::var("INDEX_REPO_TTL_DAYS").ok().as_deref()));

fn ttl_secs_from(days: Option<&str>) -> u64 {
    days.and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30)
        .saturating_mul(86_400)
}

/// Whether GC runs in dry-run mode (`INDEX_REPO_GC_DRY_RUN=1|true`): logs what
/// it would drop without deleting.
pub fn gc_dry_run() -> bool {
    flag_enabled(std::env::var("INDEX_REPO_GC_DRY_RUN").ok().as_deref())
}

fn flag_enabled(v: Option<&str>) -> bool {
    matches!(v, Some(s) if s == "1" || s.eq_ignore_ascii_case("true"))
}

/// ChromaDB collection name for a repo `root`.
///
/// Resolution order:
/// 1. Git identity — `code-<owner>-<repo>` from `remote.origin.url` (SSH/HTTPS
///    forms normalize to the same slug), plus the sub-path when `root` is nested
///    inside the repo. Stable across machines and clones.
/// 2. Fallback (no git remote) — `code-<basename>-<hash8>` (`hash8` = first 8
///    hex of SHA1 of the canonical path); machine-local but collision-safe.
///
/// The opencode `chroma-gate.ts` plugin mirrors this scheme exactly.
pub fn collection_name(root: &Path) -> String {
    if let Some(slug) = git_slug(root) {
        return finalize_collection(&slug);
    }
    fallback_collection_name(root)
}

/// Git-free name: `code-<basename>-<hash8>` (`hash8` = first 8 hex of SHA1 of the
/// canonical path). Used by the daemon when a marker carries no precomputed name,
/// so `serve` never has to shell out to `git`.
pub fn fallback_collection_name(root: &Path) -> String {
    let base = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let digest = Sha1::digest(root.as_os_str().as_encoded_bytes());
    let hash8: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    finalize_collection(&format!("{base}-{hash8}"))
}

/// `code-` + sanitized slug, capped at ChromaDB's 63-char limit (hash suffix
/// on truncation).
fn finalize_collection(raw: &str) -> String {
    let s = sanitize_slug(raw);
    let name = format!("code-{s}");
    if name.len() <= 63 {
        return name;
    }
    let h: String = Sha1::digest(raw.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("code-{}-{h}", &s[..s.len().min(49)])
}

/// Lowercase, map chars outside `[a-z0-9._-]` to `-`, collapse repeats, trim
/// `-`. Output is ASCII so byte-slicing in `finalize_collection` is safe.
fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';
        let c = if keep { c.to_ascii_lowercase() } else { '-' };
        if c == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// `owner/repo[/subpath]` from the repo's `origin` remote, or `None` when `root`
/// is not a git repo or has no remote.
fn git_slug(root: &Path) -> Option<String> {
    let toplevel = git_output(root, &["rev-parse", "--show-toplevel"])?;
    let path = normalize_remote(&git_output(
        root,
        &["config", "--get", "remote.origin.url"],
    )?)?;
    let rel = root
        .strip_prefix(&toplevel)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty());
    Some(match rel {
        Some(r) => format!("{path}/{r}"),
        None => path,
    })
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Normalize a git remote URL to its host-less `owner/repo` path. `git@h:o/r.git`,
/// `https://h/o/r`, and `ssh://git@h/o/r.git` all collapse to `o/r`.
fn normalize_remote(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s.strip_prefix("git+").unwrap_or(s);
    let hostpath = if let Some(rest) = s.split_once("://").map(|x| x.1) {
        rest.split_once('@')
            .map(|x| x.1)
            .unwrap_or(rest)
            .to_string()
    } else if let Some((before, after)) = s.split_once(':') {
        let host = before.rsplit('@').next().unwrap_or(before);
        format!("{host}/{after}")
    } else {
        s.to_string()
    };
    let hostpath = hostpath.trim_end_matches('/');
    let hostpath = hostpath.strip_suffix(".git").unwrap_or(hostpath);
    let path = hostpath.split_once('/').map(|x| x.1).unwrap_or(hostpath);
    let path = path.trim_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

pub const EXTS: &[&str] = &[
    ".py", ".pyi", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".php", ".go", ".rs", ".java",
    ".kt", ".swift", ".c", ".h", ".cc", ".cpp", ".hpp", ".rb", ".ex", ".exs", ".cs", ".nix",
    ".sql", ".sh", ".bash", ".zsh", ".md", ".mdx", ".rst", ".toml", ".yaml", ".yml", ".json",
    ".jsonc", ".vue", ".svelte", ".html", ".css", ".scss",
];

pub const SPECIAL_NAMES: &[&str] = &["Makefile", "Dockerfile", "Justfile", ".envrc"];

pub const EXTRA_IGNORE: &[&str] = &[
    ".git/",
    ".chroma/",
    ".direnv/",
    ".venv/",
    "venv/",
    "node_modules/",
    "vendor/",
    "dist/",
    "build/",
    "out/",
    "target/",
    "result",
    ".next/",
    ".nuxt/",
    ".cache/",
    ".parcel-cache/",
    "__pycache__/",
    "*.pyc",
    "*.min.js",
    "*.min.css",
    "*.map",
    "storage/",
    "bootstrap/cache/",
    "*.lock",
    "package-lock.json",
    "composer.lock",
    "uv.lock",
    "yarn.lock",
    "pnpm-lock.yaml",
];

pub fn ext_to_lang(ext: &str) -> Option<&'static str> {
    match ext {
        ".php" => Some("php"),
        ".go" => Some("go"),
        ".js" | ".jsx" | ".mjs" | ".cjs" => Some("javascript"),
        ".ts" => Some("typescript"),
        ".tsx" => Some("tsx"),
        ".py" | ".pyi" => Some("python"),
        ".rs" => Some("rust"),
        ".sh" | ".bash" | ".zsh" => Some("bash"),
        ".java" => Some("java"),
        ".c" | ".h" => Some("c"),
        ".cc" | ".cpp" | ".hpp" => Some("cpp"),
        ".cs" => Some("csharp"),
        ".rb" => Some("ruby"),
        _ => None,
    }
}

pub fn semantic_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "php" => &["function_definition", "method_declaration"],
        "go" => &[
            "function_declaration",
            "method_declaration",
            "type_declaration",
        ],
        "javascript" => &["function_declaration", "method_definition"],
        "typescript" => &[
            "function_declaration",
            "method_definition",
            "type_alias_declaration",
            "interface_declaration",
        ],
        "tsx" => &[
            "function_declaration",
            "method_definition",
            "type_alias_declaration",
            "interface_declaration",
        ],
        "python" => &["function_definition"],
        "rust" => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "macro_definition",
        ],
        "bash" => &["function_definition"],
        "java" => &[
            "method_declaration",
            "constructor_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
        "c" => &["function_definition"],
        "cpp" => &["function_definition"],
        "csharp" => &[
            "method_declaration",
            "constructor_declaration",
            "interface_declaration",
            "enum_declaration",
            "struct_declaration",
        ],
        "ruby" => &["method", "singleton_method"],
        _ => &[],
    }
}

pub fn scope_types(lang: &str) -> &'static [&'static str] {
    match lang {
        "php" => &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "enum_declaration",
        ],
        "go" => &[],
        "javascript" => &["class_declaration"],
        "typescript" => &["class_declaration"],
        "tsx" => &["class_declaration"],
        "python" => &["class_definition"],
        "rust" => &["impl_item", "trait_item"],
        "bash" => &[],
        "java" => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "record_declaration",
        ],
        "c" => &[],
        "cpp" => &[
            "class_specifier",
            "struct_specifier",
            "namespace_definition",
        ],
        "csharp" => &[
            "class_declaration",
            "struct_declaration",
            "interface_declaration",
            "namespace_declaration",
        ],
        "ruby" => &["class", "module"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exts_and_special_names() {
        assert!(EXTS.contains(&".rs"));
        assert!(EXTS.contains(&".tsx"));
        assert!(!EXTS.contains(&".png"));
        assert!(SPECIAL_NAMES.contains(&"Makefile"));
        assert!(SPECIAL_NAMES.contains(&".envrc"));
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

    #[test]
    fn ext_to_lang_new_languages() {
        assert_eq!(ext_to_lang(".java"), Some("java"));
        assert_eq!(ext_to_lang(".c"), Some("c"));
        assert_eq!(ext_to_lang(".hpp"), Some("cpp"));
        assert_eq!(ext_to_lang(".cs"), Some("csharp"));
        assert_eq!(ext_to_lang(".rb"), Some("ruby"));
    }

    #[test]
    fn collection_name_disambiguates_by_path() {
        let a = collection_name(Path::new("/work/alpha/frontend"));
        let b = collection_name(Path::new("/work/beta/frontend"));
        assert!(a.starts_with("code-frontend-"), "got {a}");
        assert!(b.starts_with("code-frontend-"), "got {b}");
        assert_ne!(a, b, "same basename + different path must not collide");
        assert_eq!(a, collection_name(Path::new("/work/alpha/frontend")));
    }

    #[test]
    fn normalize_remote_forms_collapse() {
        let want = Some("labi-le/index-repo".to_string());
        assert_eq!(
            normalize_remote("git@github.com:labi-le/index-repo.git"),
            want
        );
        assert_eq!(
            normalize_remote("https://github.com/labi-le/index-repo.git"),
            want
        );
        assert_eq!(
            normalize_remote("https://github.com/labi-le/index-repo"),
            want
        );
        assert_eq!(
            normalize_remote("ssh://git@github.com/labi-le/index-repo.git"),
            want
        );
        assert_eq!(
            normalize_remote("https://user@github.com/labi-le/index-repo/"),
            want
        );
        assert_eq!(
            normalize_remote("git+https://github.com/labi-le/index-repo.git"),
            want
        );
    }

    #[test]
    fn normalize_remote_keeps_nested_group() {
        assert_eq!(
            normalize_remote("https://gitlab.com/group/sub/proj.git"),
            Some("group/sub/proj".to_string())
        );
    }

    #[test]
    fn sanitize_slug_rules() {
        assert_eq!(sanitize_slug("labi-le/index-repo"), "labi-le-index-repo");
        assert_eq!(sanitize_slug("Foo/Bar Baz"), "foo-bar-baz");
        assert_eq!(sanitize_slug("--a//b--"), "a-b");
    }

    #[test]
    fn finalize_collection_caps_at_63() {
        let name = finalize_collection(&"x".repeat(200));
        assert!(name.len() <= 63, "len {} > 63: {name}", name.len());
        assert!(name.starts_with("code-x"));
    }

    #[test]
    fn collection_name_uses_git_remote() {
        if Command::new("git").arg("--version").output().is_err() {
            return; // git unavailable — skip
        }
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(root).args(args).output();
        assert!(git(&["init", "-q"]).unwrap().status.success());
        git(&["remote", "add", "origin", "git@github.com:acme/Widgets.git"]).unwrap();
        let canon = std::fs::canonicalize(root).unwrap();
        assert_eq!(collection_name(&canon), "code-acme-widgets");
    }

    #[test]
    fn ttl_secs_from_defaults_and_parses() {
        assert_eq!(ttl_secs_from(None), 30 * 86_400, "unset → 30 days");
        assert_eq!(
            ttl_secs_from(Some("garbage")),
            30 * 86_400,
            "invalid → default"
        );
        assert_eq!(ttl_secs_from(Some("7")), 7 * 86_400);
        assert_eq!(ttl_secs_from(Some("0")), 0, "0 → GC disabled");
    }

    #[test]
    fn flag_enabled_recognizes_truthy() {
        assert!(flag_enabled(Some("1")));
        assert!(flag_enabled(Some("true")));
        assert!(flag_enabled(Some("TRUE")));
        assert!(!flag_enabled(Some("0")));
        assert!(!flag_enabled(Some("no")));
        assert!(!flag_enabled(None));
    }
}
