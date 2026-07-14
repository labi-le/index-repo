use std::collections::BTreeSet;
use std::sync::Mutex;
use tree_sitter::Language;

static USED_GRAMMARS: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

pub fn language_for(key: &str) -> Option<Language> {
    let lang: Language = match key {
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        _ => return None,
    };

    if let Ok(mut set) = USED_GRAMMARS.lock() {
        let static_key: &'static str = match key {
            "php" => "php",
            "go" => "go",
            "javascript" => "javascript",
            "typescript" => "typescript",
            "tsx" => "tsx",
            "python" => "python",
            "rust" => "rust",
            "bash" => "bash",
            "java" => "java",
            "c" => "c",
            "cpp" => "cpp",
            "csharp" => "csharp",
            "ruby" => "ruby",
            _ => unreachable!(),
        };
        set.insert(static_key);
    }

    Some(lang)
}

pub fn used_grammars_str() -> String {
    match USED_GRAMMARS.lock() {
        Ok(set) if set.is_empty() => "none".to_string(),
        Ok(set) => set.iter().copied().collect::<Vec<_>>().join(", "),
        Err(_) => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    #[test]
    fn loads_all_langs() {
        for k in [
            "php",
            "go",
            "javascript",
            "typescript",
            "tsx",
            "python",
            "rust",
            "bash",
            "java",
            "c",
            "cpp",
            "csharp",
            "ruby",
        ] {
            assert!(language_for(k).is_some(), "missing grammar {k}");
        }
        assert!(language_for("nope").is_none());
    }

    #[test]
    fn each_lang_parses_trivial_snippet() {
        let snippets: &[(&str, &str)] = &[
            ("php", "<?php echo 1; ?>"),
            ("go", "package main\nfunc main() {}"),
            ("javascript", "function f() {}"),
            ("typescript", "function f(): void {}"),
            ("tsx", "const A = () => <div/>;"),
            ("python", "def f():\n    pass"),
            ("rust", "fn main() {}"),
            ("bash", "f() { echo hi; }"),
            ("java", "class A { void f() {} }"),
            ("c", "int f(void) { return 0; }"),
            ("cpp", "int f() { return 0; }"),
            ("csharp", "class A { void F() {} }"),
            ("ruby", "def f; 1; end"),
        ];
        for (lang, snippet) in snippets {
            let language = language_for(lang).expect(lang);
            let mut parser = Parser::new();
            parser.set_language(&language).expect(lang);
            let tree = parser.parse(snippet.as_bytes(), None).expect(lang);
            assert!(
                !tree.root_node().is_error(),
                "parse error for {lang}: root is error node"
            );
        }
    }

    #[test]
    fn unknown_key_returns_none() {
        assert!(language_for("cobol").is_none());
        assert!(language_for("").is_none());
    }

    #[test]
    fn used_grammars_tracking() {
        // `USED_GRAMMARS` is a process-global set shared with other (parallel)
        // tests, so we assert only that this test's own grammars are present
        // (monotonic inserts) — never emptiness, which would race.
        language_for("rust");
        language_for("python");
        let s = used_grammars_str();
        assert!(s.contains("python"), "got: {s}");
        assert!(s.contains("rust"), "got: {s}");
    }
}
