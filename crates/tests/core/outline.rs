// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-crate integration assertions for the outline tool.
//!
//! Discipline: envelope shape and per-language symbols are contract-locked.
//! Envelope expectations live as inline `serde_json::json!` literals below.
//! Per-language outline coverage is expressed as structural assertions: each
//! polyglot fixture must produce a tree page that mentions the language's
//! `RetryPolicy`-shaped symbol, and every rendered symbol line must carry a
//! valid `{start}-{end}` range with `start <= end`.

#[path = "../common/mod.rs"]
mod common;

use mindctx_core::budget::DEFAULT_TOKEN_BUDGET;
use mindctx_core::envelope::Envelope;
use mindctx_core::symbol::{OutlineParams, outline_with_budget};

fn outline(rel: &str) -> Envelope {
    outline_with_budget(
        &OutlineParams {
            root: &common::fixture_root(),
            path: rel,
            depth: None,
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .unwrap_or_else(|e| panic!("outline {rel} failed: {e}"))
}

/// Parsed tree page: one `(start, end, signature)` triple per rendered symbol line
/// the page body is the first blank-line-separated chunk; the status note follows).
fn tree_lines(env: &Envelope) -> Vec<(u32, u32, &str)> {
    let text = env
        .text
        .as_deref()
        .expect("outline must render a tree page");
    text.split("\n\n")
        .next()
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let (range, signature) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("symbol line must be `start-end\tsig`: {line}"));
            let (start, end) = range
                .split_once('-')
                .unwrap_or_else(|| panic!("symbol range must be `start-end`: {line}"));
            (
                start.parse().expect("start line number"),
                end.parse().expect("end line number"),
                signature,
            )
        })
        .collect()
}

/// Every rendered symbol line must carry a `start <= end` range invariant and a
/// non-empty signature — catches the most common regression mode (the skeleton
/// formatter dropping or inverting line numbers).
fn assert_valid_tree_page(env: &Envelope) {
    let lines = tree_lines(env);
    assert!(!lines.is_empty(), "outline must render symbols");
    for (start, end, signature) in lines {
        assert!(start <= end, "range start must be <= end: {start}-{end}");
        assert!(!signature.trim().is_empty(), "signature must be non-empty");
    }
}

/// ≥20 symbols per language ( fixtures convention).
#[test]
fn fixtures_have_twenty_symbols_per_language() {
    let cases = [
        ("src/lib.rs", 20),
        ("app.py", 20),
        ("index.ts", 20),
        ("App.java", 20),
        ("main.cpp", 20),
        ("main.go", 20),
    ];
    for (rel, min) in cases {
        let count = tree_lines(&outline(rel)).len();
        assert!(
            count >= min,
            "{rel} has only {count} symbols (requires >= {min})"
        );
    }
}

#[test]
fn outline_rust_has_retry_policy_struct_and_new_ctor() {
    let env = outline("src/lib.rs");
    assert_valid_tree_page(&env);
    let page = env.text.as_deref().unwrap();
    assert!(
        page.contains("pub struct RetryPolicy"),
        "rust outline must carry `pub struct RetryPolicy`: {page}"
    );
    assert!(
        page.contains("pub fn new"),
        "rust outline must carry a `pub fn new` constructor: {page}"
    );
}

#[test]
fn outline_go_has_retry_policy_struct() {
    let env = outline("main.go");
    assert_valid_tree_page(&env);
    assert!(
        env.text.as_deref().unwrap().contains("RetryPolicy struct"),
        "go outline must carry `RetryPolicy struct`: {env:?}"
    );
}

#[test]
fn outline_java_has_retry_policy_class() {
    let env = outline("App.java");
    assert_valid_tree_page(&env);
    assert!(
        env.text.as_deref().unwrap().contains("class RetryPolicy"),
        "java outline must carry `class RetryPolicy`: {env:?}"
    );
}

#[test]
fn outline_python_has_retry_policy_class() {
    let env = outline("app.py");
    assert_valid_tree_page(&env);
    assert!(
        env.text.as_deref().unwrap().contains("class RetryPolicy"),
        "python outline must carry `class RetryPolicy`: {env:?}"
    );
}

#[test]
fn outline_typescript_has_retry_policy_class() {
    let env = outline("index.ts");
    assert_valid_tree_page(&env);
    let page = env.text.as_deref().unwrap();
    assert!(
        page.contains("class RetryPolicy") || page.contains("export class RetryPolicy"),
        "typescript outline must carry `class RetryPolicy`: {page}"
    );
}

#[test]
fn outline_cpp_has_retry_policy_struct() {
    let env = outline("main.cpp");
    assert_valid_tree_page(&env);
    assert!(
        env.text.as_deref().unwrap().contains("RetryPolicy"),
        "cpp outline must mention `RetryPolicy`: {env:?}"
    );
}

// ---------------------------------------------------------------------------
// Envelope goldens (inline).
// ---------------------------------------------------------------------------

#[test]
fn envelope_empty_shape() {
    let env = Envelope::empty(None);
    let actual = serde_json::to_value(&env).expect("envelope must serialize");
    let expected = serde_json::json!({
        "token_usage": {"returned": 0},
        "truncated": false,
    });
    assert_eq!(
        actual, expected,
        "empty envelope must match the locked shape"
    );
}

/// Envelope subset for a real search call: text/token_usage/truncated
/// the locked envelope shape per ; the full v3 shape is exercised in
/// `envelope_v3_full_shape`).
#[test]
fn envelope_search_shape() {
    use mindctx_core::budget::DEFAULT_TOKEN_BUDGET;
    use mindctx_core::retrieve::glob_args::GlobArgs;
    use mindctx_core::retrieve::{SearchParams, search_with_budget};
    let env = search_with_budget(
        &common::fixture_root(),
        &SearchParams {
            pattern: "BackoffKind".to_string(),
            glob: GlobArgs(vec![]),
            ..SearchParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("search should not fail");
    assert!(
        env.text.as_deref().is_some_and(|t| !t.is_empty()),
        "search must render a page: {env:?}"
    );
    assert!(
        env.token_usage.returned > 0,
        "token_usage.returned must be > 0 for a populated search: {env:?}"
    );
}

/// Envelope v3: every field of the contract populated once, locking the shape
/// as an inline literal.
#[test]
fn envelope_v3_full_shape() {
    let json = r#"{
        "text": "src/lib.rs\n  10: pub fn handle()",
        "token_usage": {"returned": 2380, "budget": 4000},
        "truncated": true,
        "next_call": {"tool": "search", "arguments": {"offset": 100}},
        "violations": [
            {
                "name": "read_without_line_range_truncated",
                "message": "pass a line_range to keep responses bounded"
            }
        ],
        "terminal": {
            "state": "partial",
            "unit": "matches",
            "shown_from": 1,
            "shown_to": 100,
            "total": 812,
            "note": "(Shown: matches 1-100 of 812. More remain — resume from offset=100.)"
        },
        "skip_report": {
            "files": 3,
            "unreachable": 1,
            "details": [
                {"path": "vendor/big.bin", "reason": "matching content and context exceed the 64 MiB safety limit"},
                {"path": "etc/hosts", "reason": "outside the workspace"}
            ],
            "unlisted": 1
        }
    }"#;
    let parsed: Envelope = serde_json::from_str(json).expect("v3 envelope must parse");
    assert!(parsed.truncated);
    assert!(parsed.next_call.is_some());
    assert!(!parsed.violations.is_empty());
    let skip = parsed.skip_report.expect("skip_report must be present");
    assert_eq!(skip.unreachable, 1);
    assert_eq!(skip.unlisted, 1);
    assert_eq!(skip.details.len(), 2);
}
