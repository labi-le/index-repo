pub const CHUNK_LINES: usize = 120;
pub const OVERLAP: usize = 20;
pub const MAX_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_SEMANTIC_LINES: usize = 200;
pub const BATCH: usize = 2000;

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
}
