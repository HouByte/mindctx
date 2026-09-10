// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-module closed loop: the lines a search page shows are directly retrievable by
//! read with identical content. Minimal automated acceptance of "precise and cheap
//! retrieval" — search output is directly consumable, not a dead end.

#[path = "../common/mod.rs"]
mod common;

use mindctx_core::budget::DEFAULT_TOKEN_BUDGET;
use mindctx_core::envelope::Envelope;
use mindctx_core::retrieve::glob_args::GlobArgs;
use mindctx_core::retrieve::read::ReadParams;
use mindctx_core::retrieve::{self, SearchOutput, SearchParams};
use mindctx_core::symbol::{OutlineParams, outline_with_budget};

fn read_span(root: &std::path::Path, path: &str, start: u32, end: u32) -> Envelope {
    retrieve::read::read_with_budget(
        root,
        &ReadParams {
            file_path: Some(path.to_string()),
            files: None,
            offset: Some(u64::from(start)),
            limit: Some(u64::from(end) - u64::from(start) + 1),
            encoding: None,
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .unwrap_or_else(|e| panic!("read {path}:{start}-{end} failed: {e}"))
}

/// The envelope page subset is fully usable outside the crate boundary (dynamic
/// version of the contract smoke test): every match line a search page shows must be
/// retrievable by read with identical content.
#[test]
fn search_page_lines_are_readable() {
    let root = common::fixture_root();
    let env = retrieve::search_with_budget(
        &root,
        &SearchParams {
            pattern: "retry_with_backoff".to_string(),
            output_mode: SearchOutput::Content,
            glob: GlobArgs(vec![]),
            ..SearchParams::default()
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("search succeeds");
    assert!(env.token_usage.returned > 0);
    let text = env.text.as_deref().expect("content mode renders a page");

    // Parse the page: blank-line separated groups, each headed by the bare path;
    // match lines render as `N:content` (context lines use `N-context`).
    let body = text.split("\n\n").next().unwrap();
    let mut checked = 0usize;
    let mut path = String::new();
    for line in body.lines() {
        if line == "--" {
            continue;
        }
        let looks_like_path = !line.chars().next().is_some_and(|c| c.is_ascii_digit());
        if looks_like_path {
            path = line.to_string();
            continue;
        }
        let (num, content) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("match line must be `N:content`: {line}"));
        let offset: u32 = num.parse().unwrap_or_else(|_| {
            panic!("match line number must be numeric: {line}");
        });
        let page = read_span(&root, &path, offset, offset);
        let read_text = page.text.as_deref().expect("read must render a page");
        let shown = read_text
            .lines()
            .find_map(|l| l.split_once('\t'))
            .map(|(_, content)| content.trim())
            .expect("read page must show the requested line");
        assert_eq!(
            shown,
            content.trim(),
            "read must return content identical to the search page ({path}:{offset})"
        );
        checked += 1;
    }
    assert!(checked > 0, "fixtures must produce match lines: {text}");
}

/// The outline line span is likewise consumable by read (tool relay from skeleton to
/// implementation): the tree page names a `{start}-{end}` range, reading that span
/// must show the class's member lines.
#[test]
fn outline_span_is_readable() {
    let root = common::fixture_root();
    let env = outline_with_budget(
        &OutlineParams {
            root: &root,
            path: "index.ts",
            depth: None,
        },
        DEFAULT_TOKEN_BUDGET,
    )
    .expect("outline succeeds");
    let page = env
        .text
        .as_deref()
        .expect("outline must render a tree page");
    let span_line = page
        .lines()
        .find(|l| l.contains("class RetryPolicy"))
        .expect("outline contains RetryPolicy");
    let (range, _) = span_line
        .split_once('\t')
        .expect("symbol line must be `start-end\tsig`");
    let (start, end) = range
        .split_once('-')
        .expect("symbol range must be `start-end`");
    let (start, end): (u32, u32) = (start.parse().unwrap(), end.parse().unwrap());

    let read_env = read_span(&root, "index.ts", start, end);
    let read_text = read_env.text.as_deref().expect("read must render a page");
    assert!(
        read_text.contains("delayMs"),
        "method members should appear inside the class span: {read_text}"
    );
}

/// The envelope serde round-trip also holds from this crate's perspective (complements envelope_smoke).
#[test]
fn envelope_roundtrips_outside_core() {
    let env = Envelope::empty(Some(100));
    let json = serde_json::to_value(&env).unwrap();
    let back: Envelope = serde_json::from_value(json).unwrap();
    assert_eq!(env, back);
}
