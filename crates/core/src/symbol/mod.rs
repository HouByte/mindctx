// SPDX-License-Identifier: MIT OR Apache-2.0

//! Symbol layer: tree-sitter grammar registry with per-language cargo feature gating.
//!
//! Supported languages: rust / go / typescript / python / java / c++.
//! Only outline is supported (per-file symbol skeleton — see the structure before reading the
//! whole file, see [outline]); LSP reference lookup is explicitly out of scope (non-goal,
//! serena's moat), with a bridging backdoor left on the trait.
//!
//! One query config per language: the query captures `@def` (definition node) and
//! `@name` (name node); `labels` maps 1:1 to the query's top-level patterns. Names are
//! extracted declaratively from any nesting depth (e.g. C++ `function_definition → declarator
//! → identifier`), with no per-language code to write.
//!
//! grammar bridging: each official grammar crate exposes `LANGUAGE: LanguageFn` uniformly
//! decoupled via `tree-sitter-language`), loaded through `tree_sitter::Language::from` — at
//! runtime the whole repo has a single tree-sitter 0.26 version, never a second copy dragged in by grammars.

pub mod outline;

pub use outline::{OutlineParams, Symbol, outline_source, outline_with_budget};

use std::path::Path;

use crate::error::{Error, Result};

/// Symbol config for one language.
pub struct LangSpec {
    pub id: &'static str,
    pub extensions: &'static [&'static str],
    pub language: fn() -> tree_sitter::Language,
    /// Each top-level pattern captures one definition: `... @name) @def`.
    pub query: &'static str,
    /// Skeleton labels aligned 1:1 with the query's top-level pattern order.
    pub labels: &'static [&'static str],
}

#[cfg(feature = "lang-rust")]
const RUST: LangSpec = LangSpec {
    id: "rust",
    extensions: &["rs"],
    language: || tree_sitter::Language::from(tree_sitter_rust::LANGUAGE),
    query: "
        (function_item name: (_) @name) @def
        (function_signature_item name: (_) @name) @def
        (struct_item name: (_) @name) @def
        (enum_item name: (_) @name) @def
        (union_item name: (_) @name) @def
        (trait_item name: (_) @name) @def
        (impl_item type: (_) @name) @def
        (mod_item name: (_) @name) @def
        (type_item name: (_) @name) @def
        (const_item name: (_) @name) @def
        (static_item name: (_) @name) @def
    ",
    labels: &[
        "fn", "fn", "struct", "enum", "union", "trait", "impl", "mod", "type", "const", "static",
    ],
};

#[cfg(feature = "lang-go")]
const GO: LangSpec = LangSpec {
    id: "go",
    extensions: &["go"],
    language: || tree_sitter::Language::from(tree_sitter_go::LANGUAGE),
    query: "
        (function_declaration name: (_) @name) @def
        (method_declaration name: (_) @name) @def
        (type_spec name: (_) @name) @def
    ",
    labels: &["func", "method", "type"],
};

#[cfg(feature = "lang-typescript")]
const TYPESCRIPT_QUERY: &str = "
    (function_declaration name: (_) @name) @def
    (generator_function_declaration name: (_) @name) @def
    (class_declaration name: (_) @name) @def
    (abstract_class_declaration name: (_) @name) @def
    (method_definition name: (_) @name) @def
    (interface_declaration name: (_) @name) @def
    (type_alias_declaration name: (_) @name) @def
    (enum_declaration name: (_) @name) @def
    (module name: (_) @name) @def
    (internal_module name: (_) @name) @def
";
#[cfg(feature = "lang-typescript")]
const TYPESCRIPT_LABELS: &[&str] = &[
    "function",
    "function",
    "class",
    "class",
    "method",
    "interface",
    "type",
    "enum",
    "module",
    "namespace",
];

#[cfg(feature = "lang-typescript")]
const TYPESCRIPT: LangSpec = LangSpec {
    id: "typescript",
    extensions: &["ts", "mts", "cts"],
    language: || tree_sitter::Language::from(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
    query: TYPESCRIPT_QUERY,
    labels: TYPESCRIPT_LABELS,
};

#[cfg(feature = "lang-typescript")]
const TSX: LangSpec = LangSpec {
    id: "tsx",
    extensions: &["tsx"],
    language: || tree_sitter::Language::from(tree_sitter_typescript::LANGUAGE_TSX),
    query: TYPESCRIPT_QUERY,
    labels: TYPESCRIPT_LABELS,
};

#[cfg(feature = "lang-python")]
const PYTHON: LangSpec = LangSpec {
    id: "python",
    extensions: &["py"],
    language: || tree_sitter::Language::from(tree_sitter_python::LANGUAGE),
    query: "
        (function_definition name: (_) @name) @def
        (class_definition name: (_) @name) @def
    ",
    labels: &["def", "class"],
};

#[cfg(feature = "lang-java")]
const JAVA: LangSpec = LangSpec {
    id: "java",
    extensions: &["java"],
    language: || tree_sitter::Language::from(tree_sitter_java::LANGUAGE),
    query: "
        (class_declaration name: (_) @name) @def
        (interface_declaration name: (_) @name) @def
        (enum_declaration name: (_) @name) @def
        (record_declaration name: (_) @name) @def
        (method_declaration name: (_) @name) @def
        (constructor_declaration name: (_) @name) @def
        (annotation_type_declaration name: (_) @name) @def
    ",
    labels: &[
        "class",
        "interface",
        "enum",
        "record",
        "method",
        "constructor",
        "@interface",
    ],
};

#[cfg(feature = "lang-cpp")]
const CPP: LangSpec = LangSpec {
    id: "cpp",
    extensions: &["c", "h", "cpp", "cc", "cxx", "hpp", "hh"],
    language: || tree_sitter::Language::from(tree_sitter_cpp::LANGUAGE),
    query: "
        (function_definition declarator: (function_declarator declarator: (_) @name)) @def
        (struct_specifier name: (type_identifier) @name body: (_) @body) @def
        (class_specifier name: (type_identifier) @name body: (_) @body) @def
        (union_specifier name: (type_identifier) @name body: (_) @body) @def
        (enum_specifier name: (type_identifier) @name body: (_) @body) @def
        (namespace_definition name: (namespace_identifier) @name) @def
        (alias_declaration name: (type_identifier) @name) @def
        (field_declaration declarator: (function_declarator declarator: (_) @name)) @def
        (declaration declarator: (function_declarator declarator: (_) @name)) @def
    ",
    // class/struct/union/enum carry a body constraint: forward declarations (struct Foo;) stay out of the skeleton.
    // field_declaration: in-class method declarations; declaration: in-class constructor declarations and header-file prototypes.
    labels: &[
        "function",
        "struct",
        "class",
        "union",
        "enum",
        "namespace",
        "using",
        "method",
        "function",
    ],
};

/// All enabled languages (decided by cargo features per-language gating).
pub fn languages() -> &'static [&'static LangSpec] {
    #[cfg(any(
        feature = "lang-rust",
        feature = "lang-go",
        feature = "lang-typescript",
        feature = "lang-python",
        feature = "lang-java",
        feature = "lang-cpp"
    ))]
    {
        LANGS
    }
    #[cfg(not(any(
        feature = "lang-rust",
        feature = "lang-go",
        feature = "lang-typescript",
        feature = "lang-python",
        feature = "lang-java",
        feature = "lang-cpp"
    )))]
    {
        &[]
    }
}

#[cfg(any(
    feature = "lang-rust",
    feature = "lang-go",
    feature = "lang-typescript",
    feature = "lang-python",
    feature = "lang-java",
    feature = "lang-cpp"
))]
static LANGS: &[&LangSpec] = &[
    #[cfg(feature = "lang-rust")]
    &RUST,
    #[cfg(feature = "lang-go")]
    &GO,
    #[cfg(feature = "lang-typescript")]
    &TYPESCRIPT,
    #[cfg(feature = "lang-typescript")]
    &TSX,
    #[cfg(feature = "lang-python")]
    &PYTHON,
    #[cfg(feature = "lang-java")]
    &JAVA,
    #[cfg(feature = "lang-cpp")]
    &CPP,
];

/// Detect the language by extension; returns `None` when nothing matches (e.g. markdown, unknown binaries).
pub fn detect(path: &Path) -> Option<&'static LangSpec> {
    let ext = path.extension()?.to_str()?;
    languages()
        .iter()
        .copied()
        .find(|spec| spec.extensions.contains(&ext))
}

/// Look up by id (for tests and diagnostics).
pub fn by_id(id: &str) -> Option<&'static LangSpec> {
    languages().iter().copied().find(|spec| spec.id == id)
}

/// Require a language to be enabled: yields an explainable error when it is not (feature off or unknown language).
pub fn require_id(id: &str) -> Result<&'static LangSpec> {
    by_id(id).ok_or_else(|| Error::Config(format!("language not enabled or unsupported: {id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every enabled language's query must compile, and pattern count must align with labels —
    /// the first line of defense when upgrading grammars.
    #[test]
    fn queries_compile_and_labels_align() {
        for spec in languages() {
            let language = (spec.language)();
            let query = tree_sitter::Query::new(&language, spec.query)
                .unwrap_or_else(|e| panic!("{} query failed to compile: {e}", spec.id));
            assert_eq!(
                query.pattern_count(),
                spec.labels.len(),
                "pattern/labels misaligned for {}",
                spec.id
            );
            assert!(
                query.capture_names().contains(&"def"),
                "{} is missing the @def capture",
                spec.id
            );
            assert!(
                query.capture_names().contains(&"name"),
                "{} is missing the @name capture",
                spec.id
            );
        }
    }

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect(Path::new("a/b/lib.rs")).map(|s| s.id), Some("rust"));
        assert_eq!(detect(Path::new("main.go")).map(|s| s.id), Some("go"));
        assert_eq!(detect(Path::new("x.tsx")).map(|s| s.id), Some("tsx"));
        assert_eq!(detect(Path::new("x.ts")).map(|s| s.id), Some("typescript"));
        assert_eq!(detect(Path::new("app.py")).map(|s| s.id), Some("python"));
        assert_eq!(detect(Path::new("App.java")).map(|s| s.id), Some("java"));
        assert_eq!(detect(Path::new("main.cpp")).map(|s| s.id), Some("cpp"));
        assert_eq!(detect(Path::new("header.h")).map(|s| s.id), Some("cpp"));
        assert!(detect(Path::new("README.md")).is_none());
        assert!(detect(Path::new("no_ext")).is_none());
    }

    #[test]
    fn languages_cover_six_by_default() {
        // All features on by default (true for CI and release binaries alike).
        let ids: Vec<&str> = languages().iter().map(|s| s.id).collect();
        for expected in ["rust", "go", "typescript", "python", "java", "cpp"] {
            assert!(
                ids.contains(&expected),
                "missing language {expected}: {ids:?}"
            );
        }
    }
}
