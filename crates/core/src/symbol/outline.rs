// SPDX-License-Identifier: MIT OR Apache-2.0

//! outline: per-file symbol skeleton — see the structure without reading the whole file.
//!
//! Output is the unified envelope: the rendered tree page in `text` — one
//! `{start}-{end}\t{indent}{signature}` line per shown symbol, in source order,
//! indentation (two spaces per level) expressing nesting — plus the machine fields
//! `terminal` tallies the shown symbols, `token_usage` counts the page). The `start-end`
//! line ranges feed straight into read to fetch the implementation body.
//!
//! depth = the number of ancestors of the symbol's definition node that are themselves
//! definition nodes in this round's hit set — language-agnostic: classes/impls/namespaces
//! that are themselves symbols naturally give their members +1.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use tree_sitter::{Query, QueryCursor, QueryMatch, StreamingIterator};

use crate::budget::{self, ToolKind};
use crate::envelope::{Envelope, Terminal};
use crate::error::{Error, Result};
use crate::index;
use crate::retrieve::snapshot;
use crate::tokenize::count_tokens;

use super::LangSpec;

/// Skeleton indent unit: two spaces per level (the envelope is a flat list; indentation expresses nesting).
const INDENT: &str = "  ";
/// Signature first-line cap: real repos have overlong declaration lines; truncate beyond it (the skeleton is not the full text).
const MAX_SIGNATURE_CHARS: usize = 200;

/// The compiled tree-sitter [`Query`] for one language lives for the whole process
/// so an outline call only pays parse + match cost (query compilation is the
/// expensive step; parsers are built per-call because they are cheap).
type CompiledQuery = Arc<Query>;

fn compiled_query(spec: &LangSpec) -> Result<CompiledQuery> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<QueryKey, CompiledQuery>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = QueryKey::new(spec.id, spec.query);
    let mut locked = cache.lock().expect("query cache lock poisoned");
    // Build the query once, outside the critical section. If another thread has
    // already inserted the same key we discard our build; if the key is new we
    // insert it. This keeps the critical section short while still making
    // Query::new (the expensive step) safe under contention.
    let new_query = {
        let language = (spec.language)();
        Arc::new(
            Query::new(&language, spec.query)
                .map_err(|e| Error::Index(format!("failed to compile {} query: {e}", spec.id)))?,
        )
    };
    let query = locked.entry(key).or_insert(new_query).clone();
    Ok(query)
}

/// Cache key that discriminates by language id AND query content so a mutated spec
/// cannot reuse a stale entry (the id alone was a proxy for content under the old
/// const-LangSpec assumption).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct QueryKey {
    id: &'static str,
    query_ptr: usize,
    query_len: usize,
}

impl QueryKey {
    fn new(id: &'static str, query: &str) -> Self {
        Self {
            id,
            query_ptr: query.as_ptr() as usize,
            query_len: query.len(),
        }
    }
}

/// One symbol: name + skeleton label + nesting depth (top level = 1) + 1-based closed line range + signature first line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub label: &'static str,
    pub depth: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
}

/// outline params: `depth` caps the skeleton levels (`None` = all levels).
pub struct OutlineParams<'a> {
    pub root: &'a Path,
    /// Project-relative path
    pub path: &'a str,
    pub depth: Option<u32>,
}

/// Parse source text and extract the symbol skeleton (entry point for tests and reuse; file reading goes through [`outline_with_budget`]).
pub fn outline_source(source: &str, spec: &LangSpec) -> Result<Vec<Symbol>> {
    let language = (spec.language)();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| Error::Index(format!("failed to load {} grammar: {e}", spec.id)))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| Error::Index(format!("failed to parse {}", spec.id)))?;
    let query = compiled_query(spec)?;
    let query = query.as_ref();

    // Collect all hits: (definition node, name text, label). matches order is not source order; sort explicitly.
    let mut matched: Vec<(&'static str, tree_sitter::Node<'_>, String)> = Vec::new();
    let mut cursor = QueryCursor::new();
    // On tree-sitter 0.25+, QueryMatches is a StreamingIterator.
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        let Some((def, name_node)) = captures_of(query, m) else {
            continue;
        };
        let Some(label) = spec.labels.get(m.pattern_index) else {
            continue;
        };
        let name = source[name_node.start_byte()..name_node.end_byte()].trim();
        if name.is_empty() {
            continue;
        }
        matched.push((label, def, name.to_string()));
    }
    matched.sort_by(|a, b| {
        (a.1.start_byte(), a.1.end_byte(), a.2.as_str()).cmp(&(
            b.1.start_byte(),
            b.1.end_byte(),
            b.2.as_str(),
        ))
    });

    // Depth: how many of the definition node's ancestors are themselves definition nodes. A node
    // may be hit by multiple patterns (e.g. a function declaration matching alongside its name
    // node); dedup by node id, keeping the first.
    let def_ids: HashSet<usize> = matched.iter().map(|(_, d, _)| d.id()).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (label, def, name) in matched {
        if !seen.insert(def.id()) {
            continue;
        }
        out.push(Symbol {
            name,
            label,
            depth: depth_of(def, &def_ids),
            start_line: def.start_position().row as u32 + 1,
            end_line: def.end_position().row as u32 + 1,
            signature: signature_of(source, def),
        });
    }
    Ok(out)
}

/// Read and outline one file in the project, producing the unified envelope. The caller
/// resolves the budget at the process boundary (core never reads env).
pub fn outline_with_budget(params: &OutlineParams<'_>, budget: u64) -> Result<Envelope> {
    let resolved = index::resolve_in_root(params.root, params.path)?;
    let spec = super::detect(&resolved).ok_or_else(|| {
        Error::Config(format!(
            "unrecognized language (unsupported extension): {}",
            params.path
        ))
    })?;
    let snap = snapshot::Snapshot::open(&resolved, ToolKind::Outline).map_err(|e| match e {
        Error::Io(io) => Error::Index(format!("failed to read {}: {io}", params.path)),
        other => other,
    })?;
    // UTF-8 fast path: skip the full encoding ladder for valid UTF-8 (the common case).
    let source = if let Ok(text) = std::str::from_utf8(&snap.bytes) {
        std::borrow::Cow::Borrowed(text)
    } else {
        match snap.validate_encoding()? {
            crate::encoding::EncodingOutcome::Text { decoded, .. } => decoded,
            crate::encoding::EncodingOutcome::Binary => {
                return Err(Error::Encoding(format!(
                    "Cannot outline binary file: {}",
                    params.path
                )));
            }
            crate::encoding::EncodingOutcome::Skipped { reason } => {
                return Err(Error::Encoding(reason));
            }
        }
    };
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);

    let mut env = Envelope::empty(Some(budget));
    let mut tree: Vec<String> = Vec::new();
    // The wire is a compact tree page: one `{start}-{end}\t{indent}{signature}` line
    // per shown symbol — self-sufficient for follow-up reads.
    let mut shown = 0u64;
    for symbol in outline_source(source, spec)? {
        if let Some(max) = params.depth
            && symbol.depth > max
        {
            continue;
        }
        shown += 1;
        tree.push(format!(
            "{}-{}\t{}{}",
            symbol.start_line,
            symbol.end_line,
            INDENT.repeat((symbol.depth - 1) as usize),
            symbol.signature
        ));
    }

    // The wire is the tree page. Outline always shows complete results (no pagination),
    // so a tree that cannot fit the budget fails with the frozen ladder — raise the
    // budget or lower depth; never a silently overflowing page.
    let page = if tree.is_empty() {
        "(No symbols found.)".to_string()
    } else {
        tree.join("\n")
    };
    let cost = count_tokens(&page);
    if cost > budget {
        return Err(Error::Config(budget::budget_too_small(
            ToolKind::Outline.env_var(),
            budget,
            budget::what::OUTLINE_TREE,
        )));
    }
    env.token_usage.returned = budget::finish_wire(&page, Some(cost), budget)?;
    env.text = Some(page);

    let total = shown;
    env.terminal = Some(Terminal {
        state: "complete".to_string(),
        unit: "entries".to_string(),
        shown_from: if total == 0 { 0 } else { 1 },
        shown_to: total,
        total: Some(total),
        note: None,
    });

    Ok(env)
}

fn captures_of<'a, 'tree>(
    query: &'a tree_sitter::Query,
    m: &'a QueryMatch<'a, 'tree>,
) -> Option<(tree_sitter::Node<'tree>, tree_sitter::Node<'tree>)> {
    let mut def = None;
    let mut name = None;
    for capture in m.captures {
        let capture_name = query.capture_names().get(capture.index as usize)?;
        match *capture_name {
            "def" => def = Some(capture.node),
            "name" => name = Some(capture.node),
            _ => {}
        }
    }
    Some((def?, name?))
}

fn depth_of(def: tree_sitter::Node<'_>, def_ids: &HashSet<usize>) -> u32 {
    let mut depth = 1u32;
    let mut ancestor = def.parent();
    while let Some(node) = ancestor {
        if def_ids.contains(&node.id()) {
            depth += 1;
        }
        ancestor = node.parent();
    }
    depth
}

/// Use the definition's first line as the signature: strip trailing brace/semicolon/colon decorations, truncate if overlong.
fn signature_of(source: &str, def: tree_sitter::Node<'_>) -> String {
    let raw = &source[def.start_byte()..def.end_byte().min(source.len())];
    let first = raw.split('\n').next().unwrap_or("").trim_end();
    let mut s = first;
    while let Some('{') | Some(';') | Some(':') = s.chars().next_back() {
        s = &s[..s.len() - s.chars().next_back().unwrap().len_utf8()];
        s = s.trim_end();
    }
    truncate_chars(s, MAX_SIGNATURE_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::require_id;

    #[test]
    fn rust_depth_and_span() {
        let spec = require_id("rust").unwrap();
        let source = "mod inner {\n    pub fn helper() {}\n}\nfn top() {}\n";
        let symbols = outline_source(source, spec).unwrap();
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "inner");
        assert_eq!(symbols[0].depth, 1);
        assert_eq!(symbols[0].signature, "mod inner");
        assert_eq!(symbols[1].name, "helper");
        assert_eq!(symbols[1].depth, 2);
        assert_eq!(symbols[1].start_line, 2);
        assert_eq!(symbols[2].depth, 1);
    }

    #[test]
    fn rust_impl_nesting() {
        let spec = require_id("rust").unwrap();
        let source = "struct S;\nimpl S {\n    fn m(&self) {}\n}\n";
        let symbols = outline_source(source, spec).unwrap();
        assert_eq!(symbols[0].label, "struct");
        assert_eq!(symbols[1].label, "impl");
        assert_eq!(symbols[1].name, "S");
        assert_eq!(symbols[2].label, "fn");
        assert_eq!(symbols[2].depth, 2);
    }

    #[test]
    fn python_nested_class_methods() {
        let spec = require_id("python").unwrap();
        let source = "class C:\n    def m(self):\n        pass\n\ndef top():\n    pass\n";
        let symbols = outline_source(source, spec).unwrap();
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].depth, 1);
        assert_eq!(symbols[1].depth, 2);
        assert_eq!(symbols[1].signature, "def m(self)");
        assert_eq!(symbols[2].depth, 1);
    }

    #[test]
    fn java_class_method_depth() {
        let spec = require_id("java").unwrap();
        let source = "public class A {\n    public int f(int x) {\n        return x;\n    }\n}\n";
        let symbols = outline_source(source, spec).unwrap();
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].signature, "public class A");
        assert_eq!(symbols[1].label, "method");
        assert_eq!(symbols[1].depth, 2);
        assert_eq!(symbols[1].end_line, 4);
    }

    #[test]
    fn cpp_forward_declaration_skipped() {
        let spec = require_id("cpp").unwrap();
        let source = "struct Fwd;\nstruct Real {\n    void m();\n    void defined() {}\n};\nnamespace n {\n    void f() {}\n}\n";
        let symbols = outline_source(source, spec).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Real", "m", "defined", "n", "f"]);
        assert_eq!(symbols[0].label, "struct");
        assert_eq!(symbols[1].label, "method");
        assert_eq!(symbols[1].depth, 2);
        assert_eq!(symbols[2].label, "function");
        assert_eq!(symbols[2].depth, 2);
        assert_eq!(symbols[4].depth, 2);
    }

    #[test]
    fn go_types_and_methods() {
        let spec = require_id("go").unwrap();
        let source = "package p\n\ntype C struct {\n\tU string\n}\n\nfunc (c *C) Get() string {\n\treturn c.U\n}\n\nfunc Top() {}\n";
        let symbols = outline_source(source, spec).unwrap();
        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].label, "type");
        assert_eq!(symbols[1].label, "method");
        assert_eq!(symbols[1].depth, 1);
        assert!(symbols[1].signature.contains("func (c *C) Get()"));
        assert_eq!(symbols[2].label, "func");
    }

    #[test]
    fn typescript_class_interface_namespace() {
        let spec = require_id("typescript").unwrap();
        let source = "interface I { a: number }\nnamespace N {\n  export class C {\n    m(): void {}\n  }\n}\nfunction top() {}\n";
        let symbols = outline_source(source, spec).unwrap();
        let labels: Vec<&str> = symbols.iter().map(|s| s.label).collect();
        assert_eq!(
            labels,
            ["interface", "namespace", "class", "method", "function"]
        );
        assert_eq!(symbols[3].depth, 3);
    }

    #[test]
    fn syntax_errors_still_yield_symbols() {
        let spec = require_id("rust").unwrap();
        let source = "fn ok() {}\nfn broken( {{{\nfn also_ok() {}\n";
        let symbols = outline_source(source, spec).unwrap();
        assert!(symbols.iter().any(|s| s.name == "ok"));
        assert!(symbols.iter().any(|s| s.name == "also_ok"));
    }

    #[test]
    fn tree_page_shape_and_depth_filter() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "mod inner {\n    pub fn helper() {}\n}\n",
        )
        .unwrap();
        let env = outline_with_budget(
            &OutlineParams {
                root: tmp.path(),
                path: "a.rs",
                depth: None,
            },
            budget::DEFAULT_TOKEN_BUDGET,
        )
        .unwrap();
        let text = env
            .text
            .as_deref()
            .expect("outline must render a tree page");
        assert!(text.contains("1-3\tmod inner"), "tree page: {text}");
        assert!(
            text.contains("2-2\t  pub fn helper() {}"),
            "indentation expresses nesting: {text}"
        );
        assert!(env.token_usage.returned > 0);

        let shallow = outline_with_budget(
            &OutlineParams {
                root: tmp.path(),
                path: "a.rs",
                depth: Some(1),
            },
            budget::DEFAULT_TOKEN_BUDGET,
        )
        .unwrap();
        let shallow_text = shallow.text.as_deref().unwrap();
        assert!(
            shallow_text.contains("mod inner") && !shallow_text.contains("helper"),
            "depth=1 must show top level only: {shallow_text}"
        );
    }
}
