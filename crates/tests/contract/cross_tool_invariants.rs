// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-tool contract integration tests.
//!
//! Asserts cross-tool invariants that per-tool MCP tests don't cover:
//! 1. schema_walker: every published tool's input_schema stays inside the portable subset
//! 2. terminal_note_grammar: all 4 tools' terminal note strings match frozen grammar family
//! 3. skip_report_cap: drives >256 real traversal skips through the search tool and asserts
//!    SKIP_DETAIL_CAP enforcement plus the `unlisted` overflow field (unix: mode-000 dirs)
//! 4. budget_reservation_interlock: tool-layer returned <= budget. The floor-reservation
//!    ladder itself (tiny budgets -> frozen budget_too_small errors) is exercised by the
//!    core tests referenced in each test's doc comment
//! 5. encoding_matrix_end_to_end: search/read through duplex MCP client against encoding
//!    fixtures, asserting the fallback-encoding note and error fields, not just success
//!
//! Wire mode: each invariant test family runs in BOTH modes — the envelope assertions
//! stay byte-exact on `WireMode::Envelope` (machine consumers), and parallel `_text`
//! tests assert the same contract on the LLM injection surface. The hard budget rule
//! `returned <= budget` holds in both modes because `token_usage.returned` is the exact
//! o200k count of the wire text, observed either via the envelope field or by counting
//! the visible page.
//!
//! No LLM anywhere; no business logic added.

#[path = "../common/mod.rs"]
mod common;

use mindctx_core::envelope::SKIP_DETAIL_CAP;
use mindctx_core::wire::WireMode;
use rmcp::model::{CallToolRequestParams, JsonObject, PaginatedRequestParams};
use serde_json::{Value, json};

/// Default budget observed on the text wire (no env override in this test binary, so the
/// shared `MINDCTX_TOKEN_BUDGET` default applies). The envelope's `token_usage.returned`
/// equals the exact o200k count of the visible wire, so the budget invariant reads the
/// same on both sides.
const DEFAULT_TEXT_WIRE_BUDGET: u64 = mindctx_core::budget::DEFAULT_TOKEN_BUDGET;

/// Keys allowed in a published schema (portable wire format).
const PORTABLE_SCHEMA_KEYS: [&str; 11] = [
    "type",
    "description",
    "properties",
    "required",
    "items",
    "enum",
    "default",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
];

fn text_content(result: &rmcp::model::CallToolResult) -> String {
    let Some(block) = result.content.first() else {
        panic!("tool result must carry text content: {result:?}");
    };
    block
        .as_text()
        .unwrap_or_else(|| panic!("tool result must be text content: {block:?}"))
        .text
        .clone()
}

fn envelope_of(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&text_content(result))
        .unwrap_or_else(|e| panic!("text content must be valid envelope JSON: {e}"))
}

fn arguments(value: Value) -> JsonObject {
    value
        .as_object()
        .cloned()
        .expect("tool arguments must be a JSON object")
}

fn call_request(name: &'static str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(arguments(args))
}

// ---------------------------------------------------------------------------
// Test 1: schema_walker — portable subset across all published tools
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_walker_all_tools_portable_subset() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    assert!(
        !result.tools.is_empty(),
        "the portable-subset walk must see the published tools"
    );
    for tool in &result.tools {
        let schema: Value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        walk_portable(&schema, &format!("tool {}", tool.name));
    }
    client.cancel().await.expect("client shutdown");
}

fn walk_portable(value: &Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                assert!(
                    PORTABLE_SCHEMA_KEYS.contains(&key.as_str()),
                    "non-portable schema key {key:?} at {path}"
                );
                if key == "properties" {
                    if let Some(props) = child.as_object() {
                        for (name, prop) in props {
                            walk_portable(prop, &format!("{path}.{name}"));
                        }
                    }
                } else {
                    walk_portable(child, &format!("{path}.{key}"));
                }
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk_portable(child, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Test 2: terminal_note_grammar — all 4 tools' terminal notes match frozen variants
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_search() {
    let client = common::connected_client(common::fixture_root()).await;

    // Complete from zero (no matches)
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "xyz_no_match_12345"}),
        ))
        .await
        .expect("search round trip");
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("search terminal must have note: {env}"));
    assert!(
        note.contains("(Shown: nothing; no file matched.)")
            || note.contains("(Shown: nothing; no match found.)"),
        "search complete-from-zero note must use frozen grammar: {note}"
    );

    // Partial (with continuation)
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "fn ", "output_mode": "content", "head_limit": 1}),
        ))
        .await
        .expect("search partial round trip");
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("search terminal must have note: {env}"));
    assert!(
        note.contains("(Shown:") && note.contains("More remain — resume from offset="),
        "search partial note must use frozen grammar: {note}"
    );
    client.cancel().await.expect("client shutdown");
}

/// Text-wire mirror of `terminal_note_grammar_search`: the rendered page is the only
/// thing the model sees, so the same frozen grammar fragments must show up in the
/// visible text — the envelope is invisible on the LLM injection surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_search_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;

    let text = text_content(
        &client
            .call_tool(call_request(
                "search",
                json!({"pattern": "fn ", "output_mode": "content", "head_limit": 1}),
            ))
            .await
            .expect("search partial round trip"),
    );
    assert!(
        text.contains("(Shown:") && text.contains("More remain — resume from offset="),
        "search partial text wire must carry the frozen grammar: {text}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_glob() {
    let client = common::connected_client(common::fixture_root()).await;

    // No matches
    let result = client
        .call_tool(call_request(
            "glob",
            json!({"pattern": ["**/no_such_file_xyz12345"]}),
        ))
        .await
        .expect("glob round trip");
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("glob terminal must have note: {env}"));
    assert!(
        note.contains("(Shown:")
            && (note.contains("files shown.)") || note.contains("no file matched.")),
        "glob complete note must use frozen grammar: {note}"
    );

    // Partial (with continuation) — the polyglot fixture has >= 3 matches for these
    // patterns (src/lib.rs, main.go, index.ts), so limit=1 must yield a partial deterministically.
    let result = client
        .call_tool(call_request(
            "glob",
            json!({"pattern": ["**/*.rs", "**/*.go", "**/*.ts"], "limit": 1}),
        ))
        .await
        .expect("glob partial round trip");
    let env = envelope_of(&result);
    let state = env["terminal"]["state"].as_str().unwrap_or("unknown");
    let note = env["terminal"]["note"].as_str().unwrap_or("");
    assert_eq!(
        state, "partial",
        "the polyglot fixture must exceed glob limit=1; state: {state}, note: {note}"
    );
    assert!(
        note.contains("(Shown:") && note.contains("More remain — resume from offset="),
        "glob partial note must use frozen grammar: {note}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_glob_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;

    // Partial text wire (the polyglot fixture has >= 3 matches for these patterns).
    let text = text_content(
        &client
            .call_tool(call_request(
                "glob",
                json!({"pattern": ["**/*.rs", "**/*.go", "**/*.ts"], "limit": 1}),
            ))
            .await
            .expect("glob partial round trip"),
    );
    assert!(
        text.contains("(Shown:") && text.contains("More remain — resume from offset="),
        "glob partial text wire must carry the frozen grammar: {text}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_read() {
    let client = common::connected_client(common::fixture_root()).await;

    // Complete (read with limit within bounds)
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 10}),
        ))
        .await
        .expect("read round trip");
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("read terminal must have note: {env}"));
    assert!(
        note.contains("(Complete:") || note.contains("(Partial:"),
        "read terminal note must use frozen grammar: {note}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_read_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;

    let text = text_content(
        &client
            .call_tool(call_request(
                "read",
                json!({"file_path": "main.go", "offset": 1, "limit": 10}),
            ))
            .await
            .expect("read round trip"),
    );
    assert!(
        text.contains("(Complete:") || text.contains("(Partial:"),
        "read text wire must carry the frozen terminal grammar: {text}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_note_grammar_outline() {
    let client = common::connected_client(common::fixture_root()).await;

    let result = client
        .call_tool(call_request("outline", json!({"path": "src/lib.rs"})))
        .await
        .expect("outline round trip");
    let env = envelope_of(&result);
    let state = env["terminal"]["state"]
        .as_str()
        .unwrap_or_else(|| panic!("outline terminal must have state: {env}"));
    assert!(
        state == "complete" || state == "partial",
        "outline terminal state must be complete or partial: {state}"
    );
    // note is optional for outline: outline never truncates, always complete
    client.cancel().await.expect("client shutdown");
}

// ---------------------------------------------------------------------------
// Test 3: skip_report_cap — SKIP_DETAIL_CAP=256 enforced, unlisted overflow populated
// ---------------------------------------------------------------------------

/// Drives more than [`SKIP_DETAIL_CAP`] real traversal skips through the search tool:
/// 300 mode-000 directories each produce one `unreachable:` SkipDetail during the walk,
/// so the published `skip_report` must cap `details` at 256 and count the overflow in
/// `unlisted`. Unix-only: making a directory unreadable needs POSIX permissions.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_report_cap_enforced_search() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("skip-cap tempdir");
    // One readable candidate: a zero-result page must stay a real page (the matchable
    // file keeps the skip-report accounting the only thing under test).
    std::fs::write(tmp.path().join("readable.txt"), "skip_cap_marker\n")
        .expect("write readable candidate");
    let skip_dirs: Vec<std::path::PathBuf> = (0..300)
        .map(|i| {
            let dir = tmp.path().join(format!("d{i:03}"));
            std::fs::create_dir(&dir).expect("create skip dir");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
                .expect("chmod 000 skip dir");
            dir
        })
        .collect();
    // Preflight: an environment that ignores mode 000 (e.g. running as root) cannot
    // produce skips at all — fail loudly instead of passing vacuously.
    if std::fs::read_dir(&skip_dirs[0]).is_ok() {
        panic!("test environment ignores mode 000 (running as root?); cannot drive skip details");
    }

    let client = common::connected_client(tmp.path().to_path_buf()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "skip_cap_marker"}),
        ))
        .await
        .expect("search round trip over skip-cap fixture");
    assert!(
        !result.is_error.unwrap_or(false),
        "search must succeed over the skip-cap fixture: {result:?}"
    );
    let env = envelope_of(&result);
    let skip_report = env
        .get("skip_report")
        .unwrap_or_else(|| panic!("search over 300 unreadable dirs must report skips: {env}"));
    let details = skip_report["details"]
        .as_array()
        .expect("skip_report.details must be an array");
    assert!(
        details.len() <= SKIP_DETAIL_CAP,
        "skip_report.details must never exceed SKIP_DETAIL_CAP={SKIP_DETAIL_CAP}: {} shown",
        details.len()
    );
    assert_eq!(
        skip_report["unreachable"].as_u64(),
        Some(300),
        "every unreadable dir must be counted as unreachable: {skip_report}"
    );
    // Overflow accounting is exact: unlisted = total skips - shown details (the shown
    // count may be narrowed further by the token budget, but never past the cap).
    assert_eq!(
        skip_report["unlisted"].as_u64(),
        Some(300 - details.len() as u64),
        "unlisted must count exactly the overflow beyond the shown details: {skip_report}"
    );
    assert!(
        300 > SKIP_DETAIL_CAP as u64 && skip_report["unlisted"].as_u64().unwrap_or(0) > 0,
        "300 skips overflow the cap, so unlisted must be populated: {skip_report}"
    );
    assert!(
        details.iter().all(|d| d["reason"]
            .as_str()
            .unwrap_or_default()
            .starts_with("unreachable: ")),
        "every detail must carry the frozen unreachable format: {details:?}"
    );
    client.cancel().await.expect("client shutdown");

    // Restore permissions so TempDir cleanup can remove the fixture.
    for dir in &skip_dirs {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore skip dir permissions");
    }
}

// ---------------------------------------------------------------------------
// Test 4: budget_reservation_interlock — tool layer: returned <= budget.
// The wire-tail reservation ladder itself (tiny budget -> frozen budget_too_small error,
// i.e. body + mandatory wire tail > resolve_budget) is exercised by the core tests:
// `budget::tests::wire_tail_is_the_wire_trailer_skeleton`, `search_tests::budget_too_small_error_text`,
// and `glob_tool::tests::tiny_budget_too_small_is_fatal`. These tests observe the assembled
// envelope end-to-end through the MCP layer.
// ---------------------------------------------------------------------------

fn assert_budget_reservation(env: &Value, tool: &str) {
    let returned = env["token_usage"]["returned"]
        .as_u64()
        .unwrap_or_else(|| panic!("{tool}: token_usage.returned must be present: {env}"));
    let budget = env["token_usage"]["budget"]
        .as_u64()
        .unwrap_or_else(|| panic!("{tool}: token_usage.budget must be present: {env}"));
    assert!(
        returned <= budget,
        "{tool}: returned tokens {} must not exceed budget {}: {env}",
        returned,
        budget
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_search() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "fn ", "output_mode": "content", "head_limit": 50}),
        ))
        .await
        .expect("search round trip");
    assert_budget_reservation(&envelope_of(&result), "search");
    client.cancel().await.expect("client shutdown");
}

/// The 1:1 wire/budget invariant must hold from the LLM injection side too.
/// `returned == ntok(wire)` is enforced inside the server by `budget::finish_wire`; on
/// the text wire we observe the same rule by counting the visible page and asserting it
/// fits the configured budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_search_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "fn ", "output_mode": "content", "head_limit": 50}),
        ))
        .await
        .expect("search round trip");
    assert_wire_text_budget("search", &text_content(&result));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_glob() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "glob",
            json!({"pattern": ["**/*.rs"], "limit": 50}),
        ))
        .await
        .expect("glob round trip");
    assert_budget_reservation(&envelope_of(&result), "glob");
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_glob_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request(
            "glob",
            json!({"pattern": ["**/*.rs"], "limit": 50}),
        ))
        .await
        .expect("glob round trip");
    assert_wire_text_budget("glob", &text_content(&result));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_read() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 20}),
        ))
        .await
        .expect("read round trip");
    assert_budget_reservation(&envelope_of(&result), "read");
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_read_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 20}),
        ))
        .await
        .expect("read round trip");
    assert_wire_text_budget("read", &text_content(&result));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_outline() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request("outline", json!({"path": "src/lib.rs"})))
        .await
        .expect("outline round trip");
    assert_budget_reservation(&envelope_of(&result), "outline");
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_reservation_interlock_outline_text() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request("outline", json!({"path": "src/lib.rs"})))
        .await
        .expect("outline round trip");
    assert_wire_text_budget("outline", &text_content(&result));
    client.cancel().await.expect("client shutdown");
}

/// Text-side observer for the 1:1 wire/budget invariant: the visible wire text must
/// (a) not be envelope JSON (a leak would silently double-count in the model) and
/// (b) fit the configured budget exactly as the envelope field reports.
fn assert_wire_text_budget(tool: &str, text: &str) {
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "{tool}: text wire must not be envelope JSON: {text}"
    );
    let returned = mindctx_core::tokenize::count_tokens(text);
    assert!(
        returned <= DEFAULT_TEXT_WIRE_BUDGET,
        "{tool}: wire text returned={returned} tokens must not exceed budget={DEFAULT_TEXT_WIRE_BUDGET}: {text:?}"
    );
    assert!(!text.is_empty(), "{tool}: wire text must not be empty");
}

// ---------------------------------------------------------------------------
// Test 5: encoding_matrix_end_to_end — search/read via duplex MCP client
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoding_matrix_search_utf8() {
    let client = common::connected_client(common::encoding_root()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "hello", "path": "utf8.txt"}),
        ))
        .await
        .expect("search utf8 fixture");
    assert!(
        !result.is_error.unwrap_or(false),
        "search utf8 must succeed: {result:?}"
    );
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("envelope must have terminal.note: {env}"));
    assert!(
        !note.contains("fallback encoding"),
        "utf8 target must not carry a fallback-encoding note: {note}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoding_matrix_search_legacy_with_fallback() {
    let client = common::connected_client(common::encoding_root()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": ".", "path": ".", "fallback_encoding": "shift_jis"}),
        ))
        .await
        .expect("search with fallback_encoding");
    assert!(
        !result.is_error.unwrap_or(false),
        "search with fallback_encoding must succeed: {result:?}"
    );
    let env = envelope_of(&result);
    assert!(
        env["terminal"].get("state").is_some(),
        "envelope must have terminal.state even with legacy encoding: {env}"
    );
    // The fallback rescue must be visible to the caller as an advisory note, not silent.
    let text = env["text"].as_str().unwrap_or_default();
    let note = env["terminal"]["note"].as_str().unwrap_or_default();
    assert!(
        text.contains("(Note: ") && text.contains("decoded with fallback encoding shift_jis.)"),
        "search text must carry the frozen fallback note; text={text:?} note={note:?} env={env}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoding_matrix_read_explicit_utf8() {
    let client = common::connected_client(common::encoding_root()).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "utf8.txt", "offset": 1, "limit": 5, "encoding": "utf-8"}),
        ))
        .await
        .expect("read utf8 fixture");
    assert!(
        !result.is_error.unwrap_or(false),
        "read utf8 must succeed: {result:?}"
    );
    let env = envelope_of(&result);
    let note = env["terminal"]["note"]
        .as_str()
        .unwrap_or_else(|| panic!("read terminal must carry a note: {env}"));
    assert!(
        note.contains("(Complete:") || note.contains("(Partial:"),
        "read utf8 terminal note must use the frozen grammar: {note}"
    );
    let text = env["text"].as_str().unwrap_or_default();
    assert!(
        !text.is_empty() || env["terminal"]["state"].as_str() == Some("complete"),
        "read utf8 must produce text or complete status: {env}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoding_matrix_read_batch_sharing_budget() {
    let client = common::connected_client(common::encoding_root()).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({
                "files": [
                    {"path": "utf8.txt", "offset": 1, "limit": 3},
                    {"path": "windows1252.txt", "offset": 1, "limit": 3},
                    {"path": "no_such_file_xyz.txt", "offset": 1, "limit": 3}
                ]
            }),
        ))
        .await
        .expect("batch read round trip");
    assert!(
        !result.is_error.unwrap_or(false),
        "batch read must succeed: {result:?}"
    );
    let env = envelope_of(&result);
    assert!(
        env["terminal"].get("state").is_some(),
        "batch envelope must have terminal.state: {env}"
    );
    // Per-entry problems are reported inline: the missing entry must surface in the text
    // as a problem block, not fail the call and not vanish silently.
    let text = env["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("=== no_such_file_xyz.txt ==="),
        "batch read must report the missing entry as an inline problem: {env}"
    );
    let returned = env["token_usage"]["returned"]
        .as_u64()
        .unwrap_or_else(|| panic!("token_usage.returned must be present: {env}"));
    assert!(returned > 0, "batch read must return tokens: {env}");
    client.cancel().await.expect("client shutdown");
}

// ---------------------------------------------------------------------------
// Test 6: next_call shape (search/glob wrap a continuation envelope; read uses bare args)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_call_shape_search_uses_wrapper() {
    // search next_call is {"tool": "search", "arguments": {...}}
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "fn ", "output_mode": "content", "head_limit": 1}),
        ))
        .await
        .expect("search round trip");
    let env = envelope_of(&result);
    // head_limit=1 over a multi-match fixture must truncate, so next_call must be present
    // a regression that stops emitting it fails here instead of passing vacuously).
    let next_call = env
        .get("next_call")
        .unwrap_or_else(|| panic!("truncated search must provide next_call: {env}"));
    assert!(
        next_call.get("tool").is_some() && next_call.get("arguments").is_some(),
        "search next_call must use wrapper shape {{tool, arguments}}: {next_call}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_call_shape_glob_uses_wrapper() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "glob",
            json!({"pattern": ["**/*.rs", "**/*.go", "**/*.ts"], "limit": 1}),
        ))
        .await
        .expect("glob round trip");
    let env = envelope_of(&result);
    // The polyglot fixture has >= 3 matches for these patterns, so limit=1 must truncate.
    let next_call = env
        .get("next_call")
        .unwrap_or_else(|| panic!("truncated glob must provide next_call: {env}"));
    assert!(
        next_call.get("tool").is_some() && next_call.get("arguments").is_some(),
        "glob next_call must use wrapper shape {{tool, arguments}}: {next_call}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_call_shape_read_uses_bare_args() {
    // read next_call uses bare args: {"file_path": "...", "offset": N} / {"files": [...]}.
    // Only a BUDGET-cut page is partial and emits next_call — a page that satisfied an
    // explicit limit is complete — so the fixture is a generated file that overflows the
    // default 8500-token budget on a limit-free read.
    let tmp = tempfile::tempdir().expect("next-call tempdir");
    let big = tmp.path().join("big.txt");
    std::fs::write(
        &big,
        (1..=2000)
            .map(|i| format!("line {i} filler text to overflow the response budget\n"))
            .collect::<String>(),
    )
    .expect("write oversized fixture");
    let client = common::connected_client(tmp.path().to_path_buf()).await;
    let result = client
        .call_tool(call_request("read", json!({"file_path": "big.txt"})))
        .await
        .expect("read round trip");
    let env = envelope_of(&result);
    assert_eq!(
        env["terminal"]["state"], "partial",
        "a read over the 8500-token budget must come back partial: {env}"
    );
    let next_call = env
        .get("next_call")
        .unwrap_or_else(|| panic!("truncated read must provide next_call: {env}"));
    // read next_call must NOT have the "tool" wrapper (bare args shape)
    assert!(
        next_call.get("tool").is_none(),
        "read next_call must use bare args shape (no tool wrapper): {next_call}"
    );
    assert!(
        next_call.get("file_path").is_some() || next_call.get("files").is_some(),
        "read next_call must have file_path or files: {next_call}"
    );
    client.cancel().await.expect("client shutdown");
}

// ---------------------------------------------------------------------------
// Test 7: all 4 tools return the full envelope shape
// ---------------------------------------------------------------------------

fn assert_full_envelope_shape(env: &Value, tool: &str) {
    assert!(
        env.get("text").is_some_and(Value::is_string),
        "{tool}: envelope must have the rendered page: {env}"
    );
    assert!(
        env.get("token_usage").is_some(),
        "{tool}: envelope must have token_usage: {env}"
    );
    assert!(
        env["token_usage"].get("returned").is_some(),
        "{tool}: token_usage must have returned: {env}"
    );
    assert!(
        env.get("terminal").is_some(),
        "{tool}: envelope must have terminal: {env}"
    );
    assert!(
        env["terminal"].get("state").is_some(),
        "{tool}: terminal must have state: {env}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_tools_return_full_envelope_shape() {
    let client = common::connected_client(common::fixture_root()).await;

    for (name, args) in [
        ("search", json!({"pattern": "fn"})),
        ("glob", json!({"pattern": ["**/*.go"]})),
        (
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 3}),
        ),
        ("outline", json!({"path": "src/lib.rs"})),
    ] {
        let result = client
            .call_tool(call_request(name, args))
            .await
            .expect("{name} round trip");
        assert_full_envelope_shape(&envelope_of(&result), name);
    }

    client.cancel().await.expect("client shutdown");
}
