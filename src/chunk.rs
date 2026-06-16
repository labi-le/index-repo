use crate::config::{
    ext_to_lang, scope_types, semantic_types, CHUNK_LINES, MAX_SEMANTIC_LINES, OVERLAP,
};
use crate::grammar::language_for;
use crate::splitlines::{is_py_blank, py_splitlines};
use std::path::Path;
use tree_sitter::{Node, Parser};

/// A chunk: (line_no 1-based, body, node_type, scope).
pub type Chunk = (usize, String, String, String);

// ---------------------------------------------------------------------------
// 3.1  line_window — sliding window, Python splitlines semantics
// ---------------------------------------------------------------------------

/// Sliding-window line chunking (spec §3.1 / Python _line_window).
///
/// Uses `py_splitlines` (Python `str.splitlines()` semantics).
/// step = max(1, CHUNK_LINES - OVERLAP) = 100.
pub fn line_window(text: &str) -> Vec<(usize, String)> {
    let lines = py_splitlines(text);
    if lines.is_empty() {
        return vec![];
    }
    let len = lines.len();
    let step = (CHUNK_LINES - OVERLAP).max(1);
    let mut result = Vec::new();

    for i in (0..len).step_by(step) {
        let end = (i + CHUNK_LINES).min(len);
        let body = lines[i..end].join("\n");
        if !is_py_blank(&body) {
            result.push((i + 1, body));
        }
        if i + CHUNK_LINES >= len {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// 3.2  collect_semantic — pre-order walk, stop at targets
// ---------------------------------------------------------------------------

/// Pre-order walk: collect nodes whose kind is in semantic_types(lang).
/// When a match is found, do NOT descend into it (Python: return after append).
/// Iterates ALL children (named + unnamed) matching Python `n.children`.
fn collect_semantic<'a>(node: Node<'a>, lang: &str) -> Vec<Node<'a>> {
    let targets = semantic_types(lang);
    let mut results = Vec::new();
    walk_semantic(node, targets, &mut results);
    results
}

fn walk_semantic<'a>(node: Node<'a>, targets: &[&str], out: &mut Vec<Node<'a>>) {
    if targets.contains(&node.kind()) {
        out.push(node);
        return; // do not descend
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_semantic(child, targets, out);
        }
    }
}

// ---------------------------------------------------------------------------
// 3.3  get_scope — climb parents, collect container names
// ---------------------------------------------------------------------------

/// Climb parents to build scope string (e.g. class name).
/// Matches Python _get_scope exactly:
///  - containers = scope_types(lang)
///  - for each ancestor ∈ containers: take "name" field text
///    ELIF ancestor.kind() == "impl_item": take "type" field text
///  - reverse collected parts, join with "."
fn get_scope(node: Node<'_>, lang: &str, source: &[u8]) -> String {
    let containers = scope_types(lang);
    let mut parts: Vec<String> = Vec::new();
    let mut parent = node.parent();
    while let Some(p) = parent {
        if containers.contains(&p.kind()) {
            if let Some(name_node) = p.child_by_field_name("name") {
                parts.push(node_text(name_node, source));
            } else if p.kind() == "impl_item" {
                if let Some(type_node) = p.child_by_field_name("type") {
                    parts.push(node_text(type_node, source));
                }
            }
        }
        parent = p.parent();
    }
    parts.reverse();
    parts.join(".")
}

/// Decode a node's source text with UTF-8 replacement (Python decode(..., errors="replace")).
fn node_text(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.byte_range()]).into_owned()
}

// ---------------------------------------------------------------------------
// 3.4  ts_chunk — tree-sitter semantic chunking
// ---------------------------------------------------------------------------

/// Semantic chunking via tree-sitter (spec §3.4 / Python _ts_chunk).
///
/// LINE SPLITTER NOTE (parity-critical):
///  - Internal `lines` array for gap computation: `text.split('\n')` (plain, NOT splitlines).
///    Matches Python line 272: `lines = text.split("\n")`.
///  - `line_window` (called for gaps / oversized nodes) uses `py_splitlines` internally.
///  - MAX_SEMANTIC_LINES check: `py_splitlines(&chunk_text).len()` (Python line 296 .splitlines()).
pub fn ts_chunk(text: &str, lang: &str) -> Vec<Chunk> {
    // language_for also records the grammar as "used" for the grammars= log.
    let language = match language_for(lang) {
        Some(l) => l,
        None => return vec![],
    };

    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return vec![];
    }

    let tree = match parser.parse(text.as_bytes(), None) {
        Some(t) => t,
        None => return vec![],
    };

    let root = tree.root_node();
    let mut nodes = collect_semantic(root, lang);
    if nodes.is_empty() {
        return vec![];
    }

    // Sort by start byte (ascending) — matches Python nodes.sort(key=lambda n: n.start_byte)
    nodes.sort_by_key(|n| n.start_byte());

    // Plain '\n' split — matches Python `text.split("\n")` at line 272
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let source = text.as_bytes();

    let mut results: Vec<Chunk> = Vec::new();
    let mut cursor: usize = 0;

    for node in &nodes {
        let node_start = node.start_position().row;
        let mut node_end = node.end_position().row;

        // end_point col==0 means the node doesn't occupy that line
        if node.end_position().column == 0 && node_end > node_start {
            node_end -= 1;
        }

        // Gap before this node → line-window as "preamble"
        if cursor < node_start {
            let gap = lines[cursor..node_start].join("\n");
            if !is_py_blank(&gap) {
                for (off, body) in line_window(&gap) {
                    results.push((cursor + off, body, "preamble".to_string(), String::new()));
                }
            }
        }

        // The semantic node itself
        let chunk_text = node_text(*node, source);
        let scope = get_scope(*node, lang, source);
        let start_line = node_start + 1;

        // MAX_SEMANTIC_LINES check uses py_splitlines (Python line 296: .splitlines())
        if py_splitlines(&chunk_text).len() > MAX_SEMANTIC_LINES {
            for (off, body) in line_window(&chunk_text) {
                results.push((
                    start_line + off - 1,
                    body,
                    node.kind().to_string(),
                    scope.clone(),
                ));
            }
        } else {
            results.push((start_line, chunk_text, node.kind().to_string(), scope));
        }

        cursor = cursor.max(node_end + 1);
    }

    // Trailing gap
    if cursor < total {
        let gap = lines[cursor..].join("\n");
        if !is_py_blank(&gap) {
            for (off, body) in line_window(&gap) {
                results.push((cursor + off, body, "preamble".to_string(), String::new()));
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// 3.5  detect_lang
// ---------------------------------------------------------------------------

/// Map file path → tree-sitter language key (spec §3.5 / Python _detect_lang).
///
/// Returns None for `.blade.php` files.
/// Extension lookup uses lowercase with leading dot (matching EXT_TO_LANG keys).
pub fn detect_lang(path: &Path) -> Option<&'static str> {
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(".blade.php"))
        .unwrap_or(false)
    {
        return None;
    }
    let ext_lower = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))?;
    ext_to_lang(&ext_lower)
}

// ---------------------------------------------------------------------------
// 3.6  chunk_file
// ---------------------------------------------------------------------------

/// Chunk file contents — tree-sitter first, line-window fallback (spec §3.6).
pub fn chunk_file(text: &str, path: &Path) -> Vec<Chunk> {
    if let Some(lang) = detect_lang(path) {
        let ts = ts_chunk(text, lang);
        if !ts.is_empty() {
            return ts;
        }
    }
    line_window(text)
        .into_iter()
        .map(|(line_no, body)| (line_no, body, "window".to_string(), String::new()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ---- Task 5: line_window ----

    #[test]
    fn windows_with_overlap() {
        let text = (1..=250)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let w = line_window(&text);
        assert_eq!(w[0].0, 1);
        assert_eq!(w[1].0, 101);
        assert_eq!(w[2].0, 201);
        assert_eq!(w.len(), 3);
        assert!(w[0].1.starts_with("1\n2\n"));
    }

    #[test]
    fn skips_blank_only_window() {
        assert!(line_window("   \n  \n").is_empty());
    }

    #[test]
    fn single_short() {
        let w = line_window("a\nb");
        assert_eq!(w, vec![(1, "a\nb".to_string())]);
    }

    #[test]
    fn us_separator_only_window_is_suppressed() {
        // U+001F (US) is NOT a splitlines boundary but IS whitespace to Python
        // str.strip(); such a window must be suppressed to match Python (it would
        // otherwise emit an extra chunk and diverge the chunk-ID set).
        assert!(line_window("\u{1f}\u{1f}\u{1f}").is_empty());
    }

    // ---- Task 6: semantic chunking ----

    #[test]
    fn python_semantic_scope_and_preamble() {
        crate::grammar::reset_used_grammars();
        let src = include_str!("../tests/fixtures/sample.py");
        let chunks = chunk_file(src, Path::new("sample.py"));
        let types: Vec<&str> = chunks.iter().map(|c| c.2.as_str()).collect();
        assert!(
            types.contains(&"preamble"),
            "expected preamble, got: {:?}",
            types
        );
        assert!(
            types.contains(&"function_definition"),
            "expected function_definition, got: {:?}",
            types
        );
        // method hello has scope "Greeter"
        let hello = chunks
            .iter()
            .find(|c| c.1.contains("hi {name}"))
            .expect("hello chunk not found");
        assert_eq!(hello.3, "Greeter", "hello scope should be Greeter");
        // top_level has empty scope
        let top = chunks
            .iter()
            .find(|c| c.1.contains("return 1"))
            .expect("top_level chunk not found");
        assert_eq!(top.3, "", "top_level scope should be empty");
    }

    #[test]
    fn blade_php_is_not_semantic() {
        assert_eq!(detect_lang(Path::new("x.blade.php")), None);
    }

    #[test]
    fn unknown_ext_falls_back_to_window() {
        let chunks = chunk_file("line one\nline two", Path::new("notes.txt"));
        assert!(chunks.iter().all(|c| c.2 == "window"), "{:?}", chunks);
    }

    #[test]
    fn rust_impl_item_scope() {
        crate::grammar::reset_used_grammars();
        let src = include_str!("../tests/fixtures/sample.rs");
        let chunks = chunk_file(src, Path::new("sample.rs"));
        // method bar inside impl Foo should have scope "Foo"
        let bar = chunks
            .iter()
            .find(|c| c.1.contains("42"))
            .expect("bar chunk not found");
        assert_eq!(bar.3, "Foo", "bar scope should be Foo, got: {:?}", bar);
    }

    #[test]
    fn detect_lang_known_exts() {
        assert_eq!(detect_lang(Path::new("foo.py")), Some("python"));
        assert_eq!(detect_lang(Path::new("foo.rs")), Some("rust"));
        assert_eq!(detect_lang(Path::new("foo.tsx")), Some("tsx"));
        assert_eq!(detect_lang(Path::new("foo.ts")), Some("typescript"));
        assert_eq!(detect_lang(Path::new("foo.mjs")), Some("javascript"));
        assert_eq!(detect_lang(Path::new("README.md")), None); // not in EXT_TO_LANG
        assert_eq!(detect_lang(Path::new("x.blade.php")), None);
    }
}
