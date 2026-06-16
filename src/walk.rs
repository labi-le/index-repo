use crate::config::{EXTRA_IGNORE, EXTS, MAX_FILE_BYTES, SPECIAL_NAMES};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Ignore matcher (spec §5.1)
// ---------------------------------------------------------------------------

/// Wrapper so callers don't need to import `ignore` directly.
pub struct Ignore(Gitignore);

impl Ignore {
    /// Return true if `rel` (POSIX path relative to root) is ignored.
    pub(crate) fn is_ignored(&self, rel: &Path) -> bool {
        self.0.matched_path_or_any_parents(rel, false).is_ignore()
    }
}

/// Build one matcher from `EXTRA_IGNORE` + root `.gitignore` (spec §5.1).
///
/// Patterns are added via `add_line(None, pat)` with gitwildmatch semantics,
/// built relative to `root`. Only the root `.gitignore` is consulted (no nested
/// `.gitignore`s, no global excludes — matching Python's single-file approach).
pub fn load_ignore(root: &Path) -> Ignore {
    let mut builder = GitignoreBuilder::new(root);

    // 1. Prepend EXTRA_IGNORE patterns
    for pat in EXTRA_IGNORE {
        // Ignore builder errors — pattern just won't apply
        let _ = builder.add_line(None, pat);
    }

    // 2. Append lines from root/.gitignore (if it exists)
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&gitignore_path) {
            for line in content.lines() {
                let _ = builder.add_line(None, line);
            }
        }
    }

    let gi = builder.build().unwrap_or_else(|_| {
        // Fall back to an empty matcher on build error
        GitignoreBuilder::new(root).build().unwrap()
    });
    Ignore(gi)
}

// ---------------------------------------------------------------------------
// File iterator (spec §5.2)
// ---------------------------------------------------------------------------

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

        // We only care about regular files.
        if !entry.file_type().is_file() {
            continue;
        }

        // Rule 1: extension or special name check
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

        // Rule 2: not ignored — check relative POSIX path against matcher
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Convert to a POSIX-style string for the matcher
        let rel_posix = posix_str(rel);

        // matched_path_or_any_parents checks both the path itself and all
        // ancestor dirs, so a file inside node_modules/ will be caught even
        // if the pattern is "node_modules/" (directory pattern).
        if spec
            .0
            .matched_path_or_any_parents(Path::new(&rel_posix), false)
            .is_ignore()
        {
            continue;
        }

        // Rule 3: file size
        match entry.metadata() {
            Ok(m) if m.len() <= MAX_FILE_BYTES => {}
            _ => continue,
        }

        result.push(path.to_path_buf());
    }

    result
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        // Normal indexable files
        fs::write(d.path().join("a.rs"), "fn x(){}").unwrap();
        fs::write(d.path().join("hello.py"), "x=1").unwrap();
        // Special name
        fs::write(d.path().join(".envrc"), "export X=1").unwrap();
        // Ignored via EXTRA_IGNORE (node_modules/)
        fs::create_dir_all(d.path().join("node_modules")).unwrap();
        fs::write(d.path().join("node_modules/b.js"), "1").unwrap();
        // Non-indexable extension
        fs::write(d.path().join("photo.png"), "x").unwrap();
        // Dotfile with indexable extension — must be INCLUDED (not hidden-filtered)
        fs::write(d.path().join(".foo.py"), "# hidden").unwrap();
        // Oversize file
        fs::write(d.path().join("big.rs"), "x".repeat(600 * 1024)).unwrap();
        // .gitignore excludes ignored.rs
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
}
