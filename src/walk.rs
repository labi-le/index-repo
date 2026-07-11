use crate::config::{EXTRA_IGNORE, EXTS, MAX_FILE_BYTES, SPECIAL_NAMES};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Wrapper so callers don't need to import `ignore` directly.
pub struct Ignore(Gitignore);

impl Ignore {
    /// Return true if `rel` (POSIX path relative to root) is ignored.
    pub(crate) fn is_ignored(&self, rel: &Path) -> bool {
        self.0.matched_path_or_any_parents(rel, false).is_ignore()
    }

    /// Directory-aware variant: matches with `is_dir = true` so directory-only
    /// patterns (`output/`, `node_modules/`) prune a dir that `is_ignored`
    /// (is_dir = false) would not.
    pub(crate) fn is_ignored_dir(&self, rel: &Path) -> bool {
        self.0.matched_path_or_any_parents(rel, true).is_ignore()
    }
}

/// Build one matcher from `EXTRA_IGNORE` + root `.gitignore` (spec §5.1).
///
/// Patterns are added via `add_line(None, pat)` with gitwildmatch semantics,
/// built relative to `root`. Only the root `.gitignore` is consulted (no nested
/// `.gitignore`s, no global excludes — matching Python's single-file approach).
pub fn load_ignore(root: &Path) -> Ignore {
    let mut builder = GitignoreBuilder::new(root);

    for pat in EXTRA_IGNORE {
        let _ = builder.add_line(None, pat);
    }

    let gitignore_path = root.join(".gitignore");
    if gitignore_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&gitignore_path) {
            for line in content.lines() {
                let _ = builder.add_line(None, line);
            }
        }
    }

    let gi = builder
        .build()
        .unwrap_or_else(|_| GitignoreBuilder::new(root).build().unwrap());
    Ignore(gi)
}

/// Return all indexable files under `root` (spec §5.2).
///
/// Rules (ALL must be satisfied):
/// 1. Lowercased extension ∈ EXTS, or file name ∈ SPECIAL_NAMES.
/// 2. Not matched by the ignore spec (EXTRA_IGNORE + root .gitignore),
///    including files inside an ignored directory.
/// 3. File size ≤ MAX_FILE_BYTES (skip on stat error).
///
/// PARITY-CRITICAL: uses a plain `walkdir` walk with NO hidden-file or
/// standard-filter suppression.  Dotfiles are included.  Only the single
/// root `.gitignore` + EXTRA_IGNORE apply.
pub fn iter_files(root: &Path, spec: &Ignore) -> Vec<PathBuf> {
    let mut result = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        if !entry.file_type().is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let ext_lower = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();

        let indexable = EXTS.contains(&ext_lower.as_str()) || SPECIAL_NAMES.contains(&file_name);

        if !indexable {
            continue;
        }

        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let rel_posix = posix_str(rel);

        if spec
            .0
            .matched_path_or_any_parents(Path::new(&rel_posix), false)
            .is_ignore()
        {
            continue;
        }

        match entry.metadata() {
            Ok(m) if m.len() <= *MAX_FILE_BYTES => {}
            _ => continue,
        }

        result.push(path.to_path_buf());
    }

    result
}

/// Non-ignored directories in the subtree rooted at `start`, including `start`,
/// pruning descent into ignored directories so their contents are never
/// watched. `root` anchors the relative-path ignore match; `start` may be
/// `root` or a subdirectory (e.g. a freshly created one).
///
/// PARITY-CRITICAL: plain `walkdir` + our own matcher, NOT the `ignore` crate's
/// standard-filter walker — the pruned set stays consistent with `iter_files`.
pub fn watch_dirs_under(start: &Path, root: &Path, spec: &Ignore) -> Vec<PathBuf> {
    WalkDir::new(start)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| keep_dir(e, root, spec))
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_path_buf())
        .collect()
}

pub fn watch_dirs(root: &Path, spec: &Ignore) -> Vec<PathBuf> {
    watch_dirs_under(root, root, spec)
}

fn keep_dir(entry: &walkdir::DirEntry, root: &Path, spec: &Ignore) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    match entry.path().strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => !spec.is_ignored_dir(Path::new(&posix_str(rel))),
        _ => true,
    }
}

/// Convert a `Path` to a POSIX-style string (forward slashes).
fn posix_str(p: &Path) -> String {
    #[cfg(target_os = "windows")]
    {
        p.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(target_os = "windows"))]
    {
        p.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.rs"), "fn x(){}").unwrap();
        fs::write(d.path().join("hello.py"), "x=1").unwrap();
        fs::write(d.path().join(".envrc"), "export X=1").unwrap();
        fs::create_dir_all(d.path().join("node_modules")).unwrap();
        fs::write(d.path().join("node_modules/b.js"), "1").unwrap();
        fs::write(d.path().join("photo.png"), "x").unwrap();
        fs::write(d.path().join(".foo.py"), "# hidden").unwrap();
        fs::write(d.path().join("big.rs"), "x".repeat(600 * 1024)).unwrap();
        fs::write(d.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(d.path().join("ignored.rs"), "fn y(){}").unwrap();
        d
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect()
    }

    fn rel_set(dirs: &[PathBuf], root: &Path) -> std::collections::HashSet<String> {
        dirs.iter()
            .map(|p| {
                let rel = p.strip_prefix(root).unwrap_or(p);
                rel.to_string_lossy().replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn selection() {
        let d = make_tree();
        let spec = load_ignore(d.path());
        let files = iter_files(d.path(), &spec);
        let ns = names(&files);

        assert!(ns.contains(&"a.rs".to_string()), "a.rs should be included");
        assert!(
            ns.contains(&"hello.py".to_string()),
            "hello.py should be included"
        );
        assert!(
            ns.contains(&".envrc".to_string()),
            ".envrc should be included"
        );
        assert!(
            ns.contains(&".foo.py".to_string()),
            "dotfile .foo.py should be included"
        );
        assert!(
            !ns.contains(&"b.js".to_string()),
            "b.js inside node_modules/ should be excluded"
        );
        assert!(
            !ns.contains(&"photo.png".to_string()),
            ".png should be excluded"
        );
        assert!(
            !ns.contains(&"big.rs".to_string()),
            "oversize file should be excluded"
        );
        assert!(
            !ns.contains(&"ignored.rs".to_string()),
            ".gitignore match should be excluded"
        );
    }

    #[test]
    fn makefile_is_included() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Makefile"), "all:").unwrap();
        fs::write(d.path().join("Dockerfile"), "FROM scratch").unwrap();
        let spec = load_ignore(d.path());
        let files = iter_files(d.path(), &spec);
        let ns = names(&files);
        assert!(ns.contains(&"Makefile".to_string()));
        assert!(ns.contains(&"Dockerfile".to_string()));
    }

    #[test]
    fn extra_ignore_patterns_exclude_vendor_and_dist() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("vendor")).unwrap();
        fs::write(d.path().join("vendor/lib.rs"), "x").unwrap();
        fs::create_dir_all(d.path().join("dist")).unwrap();
        fs::write(d.path().join("dist/bundle.js"), "x").unwrap();
        let spec = load_ignore(d.path());
        let files = iter_files(d.path(), &spec);
        let ns = names(&files);
        assert!(
            !ns.contains(&"lib.rs".to_string()),
            "vendor/lib.rs should be excluded"
        );
        assert!(
            !ns.contains(&"bundle.js".to_string()),
            "dist/bundle.js should be excluded"
        );
    }

    #[test]
    fn lock_files_excluded() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Cargo.lock"), "# lock").unwrap();
        let spec = load_ignore(d.path());
        let files = iter_files(d.path(), &spec);
        let ns = names(&files);
        assert!(
            !ns.contains(&"Cargo.lock".to_string()),
            "*.lock should be excluded"
        );
    }

    #[test]
    fn watch_dirs_includes_root_and_subdirs() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("src/inner")).unwrap();
        fs::create_dir_all(d.path().join("docs")).unwrap();
        let spec = load_ignore(d.path());

        let dirs = watch_dirs(d.path(), &spec);
        let rels = rel_set(&dirs, d.path());

        assert!(rels.contains(""), "root itself must be watched");
        assert!(rels.contains("src"));
        assert!(rels.contains("src/inner"));
        assert!(rels.contains("docs"));
    }

    #[test]
    fn watch_dirs_prunes_gitignored_dir_at_any_depth() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".gitignore"), "output/\nbuild/\n").unwrap();
        fs::create_dir_all(d.path().join("output/checkpoint")).unwrap();
        fs::create_dir_all(d.path().join("src/build")).unwrap();
        fs::create_dir_all(d.path().join("src/keep")).unwrap();
        let spec = load_ignore(d.path());

        let dirs = watch_dirs(d.path(), &spec);
        let rels = rel_set(&dirs, d.path());

        assert!(rels.contains("src"));
        assert!(rels.contains("src/keep"));
        assert!(!rels.contains("output"), "gitignored output/ pruned");
        assert!(
            !rels.contains("output/checkpoint"),
            "children of a pruned dir are absent"
        );
        assert!(
            !rels.contains("src/build"),
            "build/ matches at any depth (gitignore semantics)"
        );
    }

    #[test]
    fn watch_dirs_prunes_extra_ignore_dirs() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(d.path().join("target/debug")).unwrap();
        fs::create_dir_all(d.path().join("src")).unwrap();
        let spec = load_ignore(d.path());

        let dirs = watch_dirs(d.path(), &spec);
        let rels = rel_set(&dirs, d.path());

        assert!(rels.contains("src"));
        assert!(!rels.contains("node_modules"));
        assert!(!rels.contains("node_modules/pkg"));
        assert!(!rels.contains("target"));
        assert!(!rels.contains("target/debug"));
    }

    #[test]
    fn watch_dirs_includes_dotfile_dirs_but_not_extra_ignored_dotgit() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".config/sub")).unwrap();
        fs::create_dir_all(d.path().join(".git/objects")).unwrap();
        let spec = load_ignore(d.path());

        let dirs = watch_dirs(d.path(), &spec);
        let rels = rel_set(&dirs, d.path());

        assert!(
            rels.contains(".config"),
            "dotfile dir must be watched (dotfiles are included, parity)"
        );
        assert!(rels.contains(".config/sub"));
        assert!(!rels.contains(".git"), ".git/ is in EXTRA_IGNORE");
        assert!(!rels.contains(".git/objects"));
    }

    #[test]
    fn watch_dirs_under_starts_from_subdir_and_still_applies_root_ignore() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".gitignore"), "skip/\n").unwrap();
        fs::create_dir_all(d.path().join("newmod/deep")).unwrap();
        fs::create_dir_all(d.path().join("newmod/skip")).unwrap();
        let spec = load_ignore(d.path());

        let dirs = watch_dirs_under(&d.path().join("newmod"), d.path(), &spec);
        let rels = rel_set(&dirs, d.path());

        assert!(rels.contains("newmod"));
        assert!(rels.contains("newmod/deep"));
        assert!(
            !rels.contains("newmod/skip"),
            "ignore applies relative to root even when walking a subtree"
        );
    }
}
