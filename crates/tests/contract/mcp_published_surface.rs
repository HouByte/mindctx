// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP contract tests: the published surface
//! is exactly four tools; every published schema stays inside the portable subset
//! AND carries concrete type information (the published surface lock is enforced by schema
//! same discipline as the envelope goldens); every tool carries read-only
//! annotations; server instructions mention all four tools and the token-accounting
//! sentence; and one in-process JSON-RPC round trip per tool over a duplex transport
//! returns envelope JSON (envelope wire) or a rendered text page (text wire, the
//! default). Plus normalizer unit tests for the collapse/failure edges.
//! No LLM anywhere.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[path = "../common/mod.rs"]
mod common;

use mindctx_core::wire::WireMode;
use rmcp::model::{CallToolRequestParams, JsonObject, PaginatedRequestParams};
use serde_json::{Value, json};

/// Default budget for text-wire budget invariant assertions. The default
/// `MINDCTX_TOKEN_BUDGET` (and any value below it) is what `returned` is hard-checked
/// against in the wire layer; the integration test below sees no env override, so the
/// default applies.
const DEFAULT_TEXT_WIRE_BUDGET: u64 = mindctx_core::budget::DEFAULT_TOKEN_BUDGET;

/// Keys allowed in a published schema (portable wire format). Mirrors
/// `crate::normalize_schema_node`; kept literal so a normalization bug cannot hide here.
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

/// The frozen continuation sentence every tool description must carry verbatim.
/// Wire-neutral so the default text wire does not reference JSON fields the model
/// never sees on that surface.
const CONTINUATION_SENTENCE: &str = "the final status line says Complete or Partial and carries the exact resume arguments; failures arrive as self-contained tool errors.";

const TOOL_TITLES: [(&str, &str); 4] = [
    ("search", "Search file contents"),
    ("glob", "Match file paths"),
    ("read", "Read a local file"),
    ("outline", "File symbol outline"),
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

/// Parses the text content of a tools/call result into envelope JSON.
fn envelope_of(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&text_content(result))
        .unwrap_or_else(|e| panic!("text content must be valid envelope JSON: {e}"))
}

/// Asserts the envelope subset published to MCP clients: token accounting, terminal
/// state, rendered page.
fn assert_envelope_shape(env: &Value) {
    assert!(
        env.get("token_usage")
            .is_some_and(|t| t.get("returned").is_some()),
        "envelope must carry token_usage.returned: {env}"
    );
    assert!(
        env.get("terminal")
            .is_some_and(|t| t.get("state").is_some()),
        "envelope must carry terminal.state: {env}"
    );
    assert!(
        env.get("text").is_some_and(Value::is_string),
        "envelope must carry the rendered page: {env}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exactly_four_tools_published() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    let mut names: Vec<&str> = result.tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(names, ["glob", "outline", "read", "search"]);
    for tool in &result.tools {
        assert!(
            !tool.description.as_deref().unwrap_or_default().is_empty(),
            "tool {} must carry its contract description",
            tool.name
        );
    }
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_schemas_stay_inside_portable_subset() {
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

/// Every published tool's normalized input schema, frozen in `expected_schemas.json`
/// discipline 1: schema shape is contract). The portable-subset walk above only proves an
/// ALLOWED-KEY set; the fixture proves the actual published shape — a schemars upgrade
/// changing `$defs` inlining or the normalizer degrading a node to `{}` fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_schemas_match_golden() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    let schemas: BTreeMap<String, Value> = result
        .tools
        .iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                Value::Object(tool.input_schema.as_ref().clone()),
            )
        })
        .collect();
    assert_eq!(schemas.len(), 4, "golden must cover every published tool");
    let fixture = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contract/expected_schemas.json"),
    )
    .expect("failed to read expected_schemas.json");
    let expected: Value =
        serde_json::from_str(&fixture).expect("expected_schemas.json must be valid JSON");
    let actual: Value = serde_json::to_value(&schemas).expect("schemas must serialize to JSON");
    assert_eq!(
        actual, expected,
        "published schemas must match the frozen fixture"
    );
    client.cancel().await.expect("client shutdown");
}

/// The portable subset alone could pass vacuously (`{}` contains only allowed keys): every
/// parameter leaf must carry CONCRETE type information, and the `$defs`-inlined `BatchEntry`
/// shape (read.files.items) must be resolved, not left as a dangling shapeless node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_schemas_have_concrete_types() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    for tool in &result.tools {
        let schema: Value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        assert_concrete_types(&schema, &format!("tool {}", tool.name));
    }
    let read: &rmcp::model::Tool = result
        .tools
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read must be published");
    let schema = serde_json::Value::Object(read.input_schema.as_ref().clone());
    let items = &schema["properties"]["files"]["items"];
    assert_eq!(
        items["type"],
        json!("object"),
        "read.files.items must inline to the BatchEntry object shape: {schema}"
    );
    assert_eq!(
        items["properties"]["path"]["type"],
        json!("string"),
        "BatchEntry.path must be resolved and typed: {schema}"
    );
    assert_eq!(
        schema["properties"]["files"]["minItems"],
        json!(1),
        "read.files batch bounds must survive normalization: {schema}"
    );
    assert_eq!(
        schema["properties"]["files"]["maxItems"],
        json!(32),
        "read.files batch bounds must survive normalization: {schema}"
    );
    client.cancel().await.expect("client shutdown");
}

/// Recursively asserts every schema node carries a concrete type (or enum). Vacuously
/// passing empty objects are exactly what this catches.
fn assert_concrete_types(value: &Value, path: &str) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("schema node {path} must be an object: {value}"));
    assert!(
        obj.contains_key("type") || obj.contains_key("enum"),
        "schema node {path} must carry concrete type information: {value}"
    );
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            assert_concrete_types(prop, &format!("{path}.{name}"));
        }
    }
    if let Some(items) = obj.get("items") {
        assert_concrete_types(items, &format!("{path}[]"));
    }
}

/// Recursively asserts every schema keyword belongs to the portable key set. Property
/// names under `properties` are parameter names, not schema keywords, and are exempt.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_has_annotations() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    assert_eq!(
        result.tools.len(),
        TOOL_TITLES.len(),
        "annotation assertions must cover every published tool"
    );
    for tool in &result.tools {
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("tool {} must carry annotations", tool.name));
        assert_eq!(
            annotations.read_only_hint,
            Some(true),
            "tool {} must be read-only",
            tool.name
        );
        assert_eq!(
            annotations.destructive_hint,
            Some(false),
            "tool {} must not be destructive",
            tool.name
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(false),
            "tool {} must be closed-world",
            tool.name
        );
        let expected = TOOL_TITLES
            .iter()
            .find(|(name, _)| *name == tool.name.as_ref())
            .map(|(_, title)| *title)
            .unwrap_or_else(|| panic!("unexpected tool name {}", tool.name));
        assert_eq!(
            annotations.title.as_deref(),
            Some(expected),
            "tool {} must carry its frozen title",
            tool.name
        );
    }
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn descriptions_carry_the_frozen_continuation_contract() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    for tool in &result.tools {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.ends_with(CONTINUATION_SENTENCE),
            "tool {} description must end with the frozen continuation sentence: {description}",
            tool.name
        );
    }
    client.cancel().await.expect("client shutdown");
}

/// The frozen per-tool contract clauses beyond the continuation sentence:
/// a future description edit must not silently drop e.g. "occurrence counts, not line counts".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn descriptions_carry_the_frozen_engine_and_batch_clauses() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .list_tools(Option::<PaginatedRequestParams>::None)
        .await
        .expect("tools/list");
    let description = |name: &str| {
        result
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} must be published"))
            .description
            .as_deref()
            .unwrap_or_default()
    };
    let search = description("search");
    for clause in [
        "Rust regex syntax",
        "lookaround and backreferences are unavailable",
        "files_with_matches (default)",
        "occurrence counts, not line counts",
        "newest-first",
        "skip report",
    ] {
        assert!(
            search.contains(clause),
            "search description must carry the frozen clause {clause:?}: {search}"
        );
    }
    let read = description("read");
    for clause in [
        "batch of 1-32 entries",
        "single output token budget",
        "per-entry problems inline",
    ] {
        assert!(
            read.contains(clause),
            "read description must carry the frozen clause {clause:?}: {read}"
        );
    }
    let glob = description("glob");
    assert!(
        glob.contains("exclusions always win"),
        "glob description must carry the frozen exclusion clause: {glob}"
    );
    let outline = description("outline");
    assert!(
        outline.contains("depth caps how many skeleton levels"),
        "outline description must carry the frozen depth clause: {outline}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instructions_mentions_all_four_tools_envelope() {
    instructions_asserts_for_wire(WireMode::Envelope).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instructions_mentions_all_four_tools_text() {
    instructions_asserts_for_wire(WireMode::Text).await;
}

/// Per-wire-mode instructions assertions: the four-tool list is invariant; the token
/// accounting clause uses mode-appropriate phrasing — `token_usage` for envelope
/// machine consumers see the field name) and `token-budgeted` for text (the LLM
/// injection surface, where the wire is one rendered text page per call). Asserting
/// the wrong phrase against the wrong mode fails the contract drift loud.
async fn instructions_asserts_for_wire(wire: WireMode) {
    let client = common::connected_client_with_wire(common::fixture_root(), wire).await;
    let info = client
        .peer_info()
        .expect("initialize must record peer info");
    let instructions = info
        .instructions
        .as_deref()
        .expect("server must publish instructions");
    for name in ["search", "glob", "read", "outline"] {
        assert!(
            instructions.contains(name),
            "instructions must mention the {name} tool: {instructions}"
        );
    }
    let (own_phrase, other_phrase) = match wire {
        WireMode::Text => ("token-budgeted", "token_usage"),
        WireMode::Envelope => ("token_usage", "token-budgeted"),
    };
    assert!(
        instructions.contains(own_phrase),
        "{wire:?} instructions must mention the wire-appropriate phrase {own_phrase:?}: {instructions}"
    );
    assert!(
        !instructions.contains(other_phrase),
        "{wire:?} instructions must NOT mention the other wire's phrase {other_phrase:?}: {instructions}"
    );
    let first_paragraph = instructions.split('\n').next().unwrap_or_default();
    assert!(
        first_paragraph.chars().count() <= 250,
        "the instructions headline must stay within 250 chars, got {}",
        first_paragraph.chars().count()
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_round_trip_returns_envelope() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "retry_with_backoff"}),
        ))
        .await
        .expect("search round trip");
    assert!(!result.is_error.unwrap_or(false), "search must succeed");
    let env = envelope_of(&result);
    assert_envelope_shape(&env);
    assert!(
        !env["text"].as_str().unwrap().is_empty(),
        "cross-language hits expected on the page: {env}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_round_trip_returns_envelope() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request("glob", json!({"pattern": "**/*.go"})))
        .await
        .expect("glob round trip");
    assert!(!result.is_error.unwrap_or(false), "glob must succeed");
    let env = envelope_of(&result);
    assert_envelope_shape(&env);
    assert!(
        env["text"].as_str().unwrap_or_default().contains("main.go"),
        "glob paths mode must list main.go in the body text: {env}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_round_trip_returns_envelope() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 4}),
        ))
        .await
        .expect("read round trip");
    assert!(!result.is_error.unwrap_or(false), "read must succeed");
    let env = envelope_of(&result);
    assert_envelope_shape(&env);
    assert_eq!(
        env["terminal"]["shown_to"].as_u64(),
        Some(4),
        "read must render the requested 4-line page: {env}"
    );
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outline_round_trip_returns_envelope() {
    let client = common::connected_client(common::fixture_root()).await;
    let result = client
        .call_tool(call_request("outline", json!({"path": "src/lib.rs"})))
        .await
        .expect("outline round trip");
    assert!(!result.is_error.unwrap_or(false), "outline must succeed");
    let env = envelope_of(&result);
    assert_envelope_shape(&env);
    assert!(
        env["text"]
            .as_str()
            .unwrap_or_default()
            .contains("pub struct HttpClient"),
        "outline must surface the HttpClient symbol: {env}"
    );
    client.cancel().await.expect("client shutdown");
}

// ---------------------------------------------------------------------------
// Text-wire round trips (the LLM injection surface; default `mindctx serve` mode).
//
// Each tool's rendered page is golden-locked as `wire_text/<tool>.txt` in this crate.
// The hard invariant — `count_tokens(wire) <= budget` — holds regardless of wire mode
// the envelope's `token_usage.returned` equals the exact o200k count of
// `wire::render(&env)`, hard-verified by `budget::finish_wire`), so verifying it on the
// text wire side is the same accounting rule observed from the outside: the page text
// we see on the wire must not exceed the configured budget.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_round_trip_returns_text_wire() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request(
            "search",
            json!({"pattern": "retry_with_backoff"}),
        ))
        .await
        .expect("search round trip");
    assert!(!result.is_error.unwrap_or(false), "search must succeed");
    let text = text_content(&result);
    assert_text_wire_shape(&text, "search");
    assert_wire_budget_holds(&text);
    assert_eq!(text, wire_text_fixture("search"));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn glob_round_trip_returns_text_wire() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request("glob", json!({"pattern": "**/*.go"})))
        .await
        .expect("glob round trip");
    assert!(!result.is_error.unwrap_or(false), "glob must succeed");
    let text = text_content(&result);
    assert_text_wire_shape(&text, "glob");
    assert_wire_budget_holds(&text);
    assert_eq!(text, wire_text_fixture("glob"));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_round_trip_returns_text_wire() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request(
            "read",
            json!({"file_path": "main.go", "offset": 1, "limit": 4}),
        ))
        .await
        .expect("read round trip");
    assert!(!result.is_error.unwrap_or(false), "read must succeed");
    let text = text_content(&result);
    assert_text_wire_shape(&text, "read");
    assert_wire_budget_holds(&text);
    assert_eq!(text, wire_text_fixture("read"));
    client.cancel().await.expect("client shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outline_round_trip_returns_text_wire() {
    let client = common::connected_client_with_wire(common::fixture_root(), WireMode::Text).await;
    let result = client
        .call_tool(call_request("outline", json!({"path": "src/lib.rs"})))
        .await
        .expect("outline round trip");
    assert!(!result.is_error.unwrap_or(false), "outline must succeed");
    let text = text_content(&result);
    assert_text_wire_shape(&text, "outline");
    assert_wire_budget_holds(&text);
    assert_eq!(text, wire_text_fixture("outline"));
    client.cancel().await.expect("client shutdown");
}

/// Text-wire shape invariant: the result must be exactly `wire::render(env)` — one text block
/// whose body must NOT be envelope JSON (the structured fields ride in the envelope only on
/// `WireMode::Envelope`). This catches accidental envelope-mode
/// leaks on the text wire before the budget check below fires.
fn assert_text_wire_shape(text: &str, tool: &str) {
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "{tool}: text wire must not be envelope JSON: {text}"
    );
}

/// Hard budget invariant (wire text must never exceed its budget): the visible
/// wire must fit the configured budget. The envelope's `token_usage.returned` is the
/// exact o200k count of the same text; this assertion is the same rule observed from
/// outside the server.
fn assert_wire_budget_holds(text: &str) {
    let returned = mindctx_core::tokenize::count_tokens(text);
    assert!(
        returned <= DEFAULT_TEXT_WIRE_BUDGET,
        "wire text returned={returned} tokens must not exceed budget={DEFAULT_TEXT_WIRE_BUDGET}; \
         this is the 1:1 wire/budget invariant from the LLM injection side: {text:?}"
    );
    // Sanity: the visible wire must not be empty on a successful call.
    assert!(!text.is_empty(), "text wire must be non-empty on success");
}

/// Builds the `arguments` object for a tools/call request.
fn arguments(value: Value) -> JsonObject {
    value
        .as_object()
        .cloned()
        .expect("tool arguments must be a JSON object")
}

/// Builds a tools/call request for one tool.
fn call_request(name: &'static str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(arguments(args))
}

/// Reads the text-wire fixture for a given tool (in-process variant). Normalizes
/// CRLF to LF for the same reason as the cli_mcp_roundtrip helper: a windows
/// checkout applies git autocrlf to the tracked fixture while the rendered text
/// wire is always LF, and the byte-locked assertions need to compare apples to
/// apples on every platform.
fn wire_text_fixture(tool: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("contract/wire_text")
        .join(format!("{tool}.txt"));
    std::fs::read_to_string(&path)
        .expect("wire_text fixture must exist")
        .replace("\r\n", "\n")
}

// ---------------------------------------------------------------------------
// Normalizer unit tests: collapse edges and loud-failure edges
// ---------------------------------------------------------------------------

mod normalizer_tests {
    use serde_json::{Value, json};

    use mindctx_mcp::{SCHEMA_DEPTH_CAP, normalize_schema_node};

    fn defs(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().expect("defs must be an object")
    }

    /// The I1 edge: schemars wraps a named type in `anyOf: [{T}, {null}]` and puts the
    /// field's doc comment on the WRAPPER — collapsing must not drop it.
    #[test]
    fn anyof_collapse_keeps_wrapper_description() {
        let schema = json!({
            "description": "The file path",
            "default": {"path": "main.rs"},
            "anyOf": [{"$ref": "#/$defs/Named"}, {"type": "null"}]
        });
        let defs = defs(json!({
            "Named": {"type": "object", "properties": {"path": {"type": "string"}}}
        }));
        let out = normalize_schema_node(&schema, 0, Some(&defs)).unwrap();
        assert_eq!(
            out["type"],
            json!("object"),
            "collapsed branch must survive: {out}"
        );
        assert_eq!(out["description"], json!("The file path"), "{out}");
        assert_eq!(out["default"], json!({"path": "main.rs"}), "{out}");
        assert!(out.get("anyOf").is_none(), "{out}");
        assert_eq!(out["properties"]["path"]["type"], json!("string"), "{out}");
    }

    /// The I2 edge: an unresolvable `$ref` must fail loudly, never publish `{}`.
    #[test]
    fn unresolved_ref_fails_loudly() {
        let schema = json!({"$ref": "#/$defs/Missing"});
        let err =
            normalize_schema_node(&schema, 0, None).expect_err("unresolved $ref must be an error");
        assert!(
            err.to_string().contains("unresolved $ref: #/$defs/Missing"),
            "{err}"
        );
    }

    /// The I2 edge: depth-cap overflow must fail loudly, never publish `{}`.
    #[test]
    fn depth_overflow_fails_loudly() {
        let mut schema = json!({"type": "string"});
        for _ in 0..SCHEMA_DEPTH_CAP {
            schema = json!({"items": schema});
        }
        assert!(
            normalize_schema_node(&schema, 0, None).is_ok(),
            "a schema exactly at the cap still normalizes"
        );
        schema = json!({"items": schema});
        let err =
            normalize_schema_node(&schema, 0, None).expect_err("depth overflow must be an error");
        assert!(err.to_string().contains("depth cap"), "{err}");
    }

    #[test]
    fn nullable_type_collapse_and_lone_null_drop() {
        let out = normalize_schema_node(&json!({"type": ["string", "null"]}), 0, None).unwrap();
        assert_eq!(out["type"], json!("string"), "{out}");
        // `type: ["null"]` alone is useless as a published param; the key is dropped.
        let out = normalize_schema_node(&json!({"type": ["null"]}), 0, None).unwrap();
        assert!(out.get("type").is_none(), "{out}");
    }

    #[test]
    fn ref_inlining_keeps_portable_siblings() {
        let schema = json!({"description": "The file", "$ref": "#/$defs/Named"});
        let defs = defs(json!({"Named": {"type": "string"}}));
        let out = normalize_schema_node(&schema, 0, Some(&defs)).unwrap();
        assert_eq!(out, json!({"type": "string", "description": "The file"}));
    }
}
