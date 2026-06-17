use crate::chunk::chunk_file;
use crate::store::{Meta, Record};
use sha1::{Digest, Sha1};
use std::path::Path;

pub fn chunk_id(rel: &str, line: usize, body: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("{rel}:{line}:{body}").as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn lang_field(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => ext.to_lowercase(),
        _ => path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
    }
}

pub fn build_meta(rel: &str, line: usize, lang: &str, node_type: &str, scope: &str) -> Meta {
    Meta {
        path: rel.to_string(),
        line,
        lang: lang.to_string(),
        node_type: node_type.to_string(),
        scope: scope.to_string(),
    }
}

pub fn chunks_for_file(path: &Path, root: &Path) -> (String, Vec<Record>, usize, usize, bool) {
    let rel = match path.strip_prefix(root) {
        Ok(r) => posix_str(r),
        Err(_) => path.to_string_lossy().into_owned(),
    };

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return (rel, vec![], 0, 0, true),
    };

    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return (rel, vec![], 0, 0, false),
    };

    let lang = lang_field(path);
    let mut records = Vec::new();
    let mut ts_count: usize = 0;
    let mut win_count: usize = 0;

    for (line_no, body, node_type, scope) in chunk_file(&text, path) {
        let id = chunk_id(&rel, line_no, &body);
        let meta = build_meta(&rel, line_no, &lang, &node_type, &scope);
        if node_type == "window" {
            win_count += 1;
        } else {
            ts_count += 1;
        }
        records.push(Record { id, body, meta });
    }

    (rel, records, ts_count, win_count, true)
}

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

    const SHA1_GOLDEN: &str = "4a679fdf6dfaf9eb3065579128135bd28632f800";

    #[test]
    fn id_matches_python_sha1() {
        assert_eq!(chunk_id("a.py", 1, "x = 1"), SHA1_GOLDEN);
    }

    #[test]
    fn meta_scope_present_when_non_empty() {
        let m = build_meta("src/foo.rs", 10, "rs", "function_item", "Greeter");
        assert_eq!(m.scope, "Greeter");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["scope"], "Greeter");
        assert_eq!(v["type"], "function_item");
    }

    #[test]
    fn meta_scope_absent_when_empty() {
        let m = build_meta("src/bar.py", 1, "py", "window", "");
        assert_eq!(m.scope, "");
        let v = serde_json::to_value(&m).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("scope"),
            "scope should be omitted, got: {v}"
        );
    }

    #[test]
    fn lang_field_rules() {
        assert_eq!(lang_field(Path::new("a.py")), "py");
        assert_eq!(lang_field(Path::new("Makefile")), "Makefile");
        assert_eq!(lang_field(Path::new(".envrc")), ".envrc");
        assert_eq!(lang_field(Path::new("x.tar.gz")), "gz");
        assert_eq!(lang_field(Path::new("script.TS")), "ts");
    }

    #[test]
    fn chunks_for_file_binary_returns_not_ok() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("bin.py");
        fs::write(&p, b"\xff\xfe not utf8").unwrap();
        let (_, recs, _, _, ok) = chunks_for_file(&p, d.path());
        assert!(!ok, "binary file should have ok=false");
        assert!(recs.is_empty());
    }

    #[test]
    fn chunks_for_file_missing_returns_ok_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ghost.py");
        let (_, recs, _, _, ok) = chunks_for_file(&p, d.path());
        assert!(ok, "missing file should have ok=true");
        assert!(recs.is_empty());
    }

    #[test]
    fn chunks_for_file_window_fallback() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("notes.txt");
        fs::write(&p, "line one\nline two\n").unwrap();
        let (rel, recs, ts, win, ok) = chunks_for_file(&p, d.path());
        assert!(ok);
        assert_eq!(rel, "notes.txt");
        assert_eq!(ts, 0);
        assert_eq!(win, 1);
        assert_eq!(recs[0].meta.node_type, "window");
    }

    #[test]
    fn chunks_for_file_python_semantic() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sample.py");
        fs::write(
            &p,
            "import os\n\nclass Greeter:\n    def hello(self, name):\n        return f\"hi {name}\"\n\ndef top_level():\n    return 1\n",
        )
        .unwrap();
        let (rel, recs, ts, win, ok) = chunks_for_file(&p, d.path());
        assert!(ok);
        assert_eq!(rel, "sample.py");
        assert!(ts > 0, "should have ts chunks");
        let first = &recs[0];
        let expected_id = chunk_id(&rel, first.meta.line, &first.body);
        assert_eq!(first.id, expected_id);
        let _ = win;
    }

    #[test]
    fn chunk_id_format() {
        let id = chunk_id("sub/dir/foo.rs", 42, "fn bar() {}");
        let expected = {
            let mut h = sha1::Sha1::new();
            sha1::Digest::update(&mut h, b"sub/dir/foo.rs:42:fn bar() {}");
            format!("{:x}", sha1::Digest::finalize(h))
        };
        assert_eq!(id, expected);
    }
}
