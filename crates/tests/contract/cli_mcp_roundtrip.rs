// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP contract tests: a scripted JSON-RPC client talking straight to the stdio server.
//!
//! Covers: handshake (initialize/initialized), the four-tool surface (search/glob/read/
//! outline), the envelope subset per tool, nonzero token accounting, error semantics
//! (invalid params AND tool failures surface as isError results; only an unknown tool is
//! a protocol-level -32602) — no LLM anywhere.
//!
//! Wire mode: the historical contract assertions target `WireMode::Envelope` (machine
//! consumers parse the structured envelope). The default subprocess is spawned with
//! `--wire envelope` to preserve them. A separate test exercises the default text wire
//! the LLM injection surface) and asserts against the rendered page + a new wire-text
//! golden.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "../common/mod.rs"]
mod common;

use common::fixture_root;

/// Locate the `mindctx` binary. Cargo only sets `CARGO_BIN_EXE_<name>` for integration
/// tests in the same package as the binary; this crate (`mindctx-tests`) lives in
/// `crates/tests/` while the binary lives in `crates/cli/`, so the env var is unset.
/// Fall back to the standard workspace `target/{debug,release}/mindctx` path. CI runs
/// `cargo build --workspace` before `cargo test --workspace` so this file exists on
/// every CI machine (the CLI crate has only [[bin]], no [lib], so `cargo test` alone
/// does not produce it on a fresh tree — verified locally with `cargo clean && cargo
/// test --workspace` reproducing the ENOENT every cli_mcp_roundtrip test surfaces).
fn mindctx_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_mindctx") {
        return PathBuf::from(p);
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set");
    let manifest_path = PathBuf::from(manifest);
    let workspace_root = manifest_path
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    // Cargo emits `<bin>` on unix and `<bin>.exe` on windows. Probe both shapes so
    // the spawn doesn't blow up with NotFound on the windows-latest matrix lane.
    let bin_name = if cfg!(windows) {
        "mindctx.exe"
    } else {
        "mindctx"
    };
    for profile in ["debug", "release"] {
        let path = workspace_root.join(format!("target/{profile}/{bin_name}"));
        if path.exists() {
            return path;
        }
    }
    // Final fallback so the spawn error names the path the caller would have tried.
    workspace_root.join(format!("target/debug/{bin_name}"))
}

use mindctx_core::wire::WireMode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// Minimal JSON-RPC client: MCP stdio = newline-delimited JSON-RPC messages.
struct Client {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Spawn `mindctx serve` and complete the handshake (initialize + notifications/initialized).
    ///
    /// Default wire mode is [`WireMode::Envelope`] so the structured envelope assertions
    /// below stay byte-exact (this binary is the machine-consumer contract test).
    /// Tests targeting the LLM injection surface opt in to [`WireMode::Text`] via
    /// [`Self::spawn_with_wire`], which leaves the subprocess on its default text wire.
    async fn spawn(cwd: &PathBuf) -> Self {
        Self::spawn_with_wire(cwd, WireMode::Envelope).await
    }

    /// Same as [`Self::spawn`] but with an explicit wire mode.
    ///
    /// Always passes `--wire <mode>` so a stray `MINDCTX_WIRE` in the test environment
    /// cannot silently flip the subprocess into the other mode (otherwise the machine-
    /// consumer assertions below would parse the LLM injection surface by accident).
    async fn spawn_with_wire(cwd: &PathBuf, wire: WireMode) -> Self {
        Self::spawn_with_client_name(cwd, wire, "contract-test").await
    }

    /// Same as [`Self::spawn_with_wire`] but with a custom `clientInfo.name`.
    /// Used to test host-specific behavior (e.g. Codex hard cap) without calling
    /// `list_tools` first.
    async fn spawn_with_client_name(cwd: &PathBuf, wire: WireMode, client_name: &str) -> Self {
        let args: Vec<String> = vec![
            "serve".to_string(),
            "--wire".to_string(),
            wire.as_str().to_string(),
        ];

        let mut child = Command::new(mindctx_bin())
            .args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn mindctx serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };

        let info = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": client_name, "version": "0.0.0"}
                }),
            )
            .await
            .expect("initialize round trip");
        assert_eq!(info["serverInfo"]["name"], json!("mindctx"));
        assert_eq!(
            info["serverInfo"]["version"],
            json!(mindctx_core::VERSION),
            "single version source (workspace.package.version)"
        );
        assert!(
            info["capabilities"]["tools"].is_object(),
            "must declare the tools capability"
        );
        client.notify("notifications/initialized").await;
        client
    }

    /// Spawn with `clientInfo.name = "Codex CLI"` to test the Codex host path.
    /// Defaults to envelope wire mode (machine-consumer contract assertions).
    async fn spawn_with_codex_name(cwd: &PathBuf) -> Self {
        Self::spawn_with_client_name(cwd, WireMode::Envelope, "Codex CLI").await
    }

    async fn send_raw(&mut self, line: String) {
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Send a request and wait for the response with the same id (skipping server-initiated notifications).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })
        .to_string();
        self.send_raw(line).await;
        self.wait_response(id).await
    }

    async fn notify(&mut self, method: &str) {
        let line = json!({"jsonrpc": "2.0", "method": method}).to_string();
        self.send_raw(line).await;
    }

    async fn wait_response(&mut self, id: u64) -> Result<Value, Value> {
        loop {
            let mut line = String::new();
            timeout(Duration::from_secs(60), self.stdout.read_line(&mut line))
                .await
                .expect("timed out waiting for server response")
                .expect("server exited early");
            let msg: Value = serde_json::from_str(&line).expect("each line must be valid JSON");
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return match msg.get("error") {
                    Some(err) => Err(err.clone()),
                    None => Ok(msg["result"].clone()),
                };
            }
            // Other messages (server notifications etc.): skip and keep waiting.
        }
    }

    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Parse the text content of tools/call into envelope JSON.
fn envelope_of(call_result: &Value) -> Value {
    let text = call_result["content"][0]["text"]
        .as_str()
        .expect("tool result must be text content");
    serde_json::from_str(text).expect("text content must be valid envelope JSON")
}

#[tokio::test]
async fn tools_list_has_four_contract_tools() {
    let mut client = Client::spawn(&fixture_root()).await;
    let result = client
        .request("tools/list", json!({}))
        .await
        .expect("tools/list");
    let mut names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["glob", "outline", "read", "search"]);
    // Every tool must declare inputSchema (the host generates param validation from it).
    for tool in result["tools"].as_array().unwrap() {
        assert!(
            tool["inputSchema"].is_object(),
            "tool {} is missing inputSchema",
            tool["name"]
        );
    }
    client.shutdown().await;
}

#[tokio::test]
async fn search_round_trip_returns_envelope_with_tokens() {
    let mut client = Client::spawn(&fixture_root()).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "search", "arguments": {"pattern": "retry_with_backoff", "head_limit": 5}}),
        )
        .await
        .expect("search round trip");
    assert_eq!(result["isError"], json!(false), "search should not error");
    let env = envelope_of(&result);
    let body_lines = |env: &Value| {
        env["text"]
            .as_str()
            .unwrap_or_default()
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert!(
        body_lines(&env) >= 3,
        "cross-language hits on the page: {env}"
    );
    // token accounting exists and is > 0 ( contract assertion).
    assert!(
        env["token_usage"]["returned"].as_u64().unwrap() > 0,
        "token_usage.returned must be > 0"
    );
    assert_eq!(env["truncated"], json!(false));

    // A capped page is a resumable cursor: Partial state plus a ready-to-paste next_call.
    let result = client
        .request(
            "tools/call",
            json!({"name": "search", "arguments": {"pattern": "backoff", "head_limit": 1}}),
        )
        .await
        .expect("search pagination round trip");
    let env = envelope_of(&result);
    assert_eq!(env["truncated"], json!(true));
    assert_eq!(body_lines(&env), 1);
    assert!(
        env["next_call"]["arguments"].is_object(),
        "trimmed page must carry a continuation: {env}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn outline_round_trip_returns_symbol_skeleton() {
    let mut client = Client::spawn(&fixture_root()).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": "src/lib.rs"}}),
        )
        .await
        .expect("outline round trip");
    let env = envelope_of(&result);
    // Tree page: one `{start}-{end}\t{indent}{signature}` line per shown symbol.
    let symbols: Vec<&str> = env["text"]
        .as_str()
        .expect("outline must render a tree page")
        .split("\n\n")
        .next()
        .unwrap()
        .lines()
        .map(|line| line.split_once('\t').expect("symbol line").1)
        .collect();
    assert!(
        symbols.iter().any(|s| s.contains("pub struct HttpClient")),
        "skeleton contains HttpClient: {symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.starts_with("  ") && s.contains("pub fn full")),
        "indentation expresses nesting (jitter::full inside a mod): {symbols:?}"
    );
    assert!(env["token_usage"]["returned"].as_u64().unwrap() > 0);

    // depth filter: top level only.
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": "src/lib.rs", "depth": 1}}),
        )
        .await
        .expect("outline depth round trip");
    let env = envelope_of(&result);
    let symbols: Vec<&str> = env["text"]
        .as_str()
        .expect("outline must render a tree page")
        .split("\n\n")
        .next()
        .unwrap()
        .lines()
        .map(|line| line.split_once('\t').expect("symbol line").1)
        .collect();
    assert!(
        symbols.iter().all(|s| !s.starts_with(' ')),
        "depth=1 must not contain indented symbols: {symbols:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn read_round_trip_is_byte_exact_with_citation() {
    let mut client = Client::spawn(&fixture_root()).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "read", "arguments": {"file_path": "main.go", "offset": 9, "limit": 4}}),
        )
        .await
        .expect("read round trip");
    let env = envelope_of(&result);

    // One numbered body line per shown line, 1-based `N\tcontent` (read contract).
    let page = env["text"].as_str().expect("read must render a page");
    let body = page.split("\n\n").next().unwrap();
    let shown: Vec<(&str, &str)> = body
        .lines()
        .map(|line| line.split_once('\t').expect("numbered body line"))
        .collect();
    assert_eq!(shown.len(), 4);
    for (i, (num, _)) in shown.iter().enumerate() {
        assert_eq!(*num, &(9 + i).to_string()[..], "1-based line numbers");
    }

    // Content is byte-identical to disk ( baseline read assertion):
    // the shown lines, concatenated, reproduce the requested range exactly.
    let expected = std::fs::read_to_string(fixture_root().join("main.go")).unwrap();
    let expected_lines: Vec<&str> = expected.lines().skip(8).take(4).collect();
    let content: Vec<&str> = shown.iter().map(|(_, c)| *c).collect();
    assert_eq!(content.join("\n"), expected_lines.join("\n"));
    assert!(env["token_usage"]["returned"].as_u64().unwrap() > 0);
    client.shutdown().await;
}

#[tokio::test]
async fn invalid_params_and_unknown_tool_surface_as_errors() {
    let mut client = Client::spawn(&fixture_root()).await;

    // Missing required param: rmcp surfaces it as a tool-level error (isError=true, readable text).
    let result = client
        .request("tools/call", json!({"name": "search", "arguments": {}}))
        .await
        .expect("missing pattern should still get a response");
    assert_eq!(
        result["isError"],
        json!(true),
        "missing params must be an error result: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("pattern"),
        "error text must name the missing field: {text}"
    );

    // Wrong param type: also a tool-level error.
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": 123}}),
        )
        .await
        .expect("type error should still get a response");
    assert_eq!(result["isError"], json!(true));

    // Unknown tool: protocol-level error -32602 (tool not found).
    let err = client
        .request(
            "tools/call",
            json!({"name": "nonexistent", "arguments": {}}),
        )
        .await
        .expect_err("unknown tool must be a protocol-level error");
    assert_eq!(err["code"], json!(-32602), "unknown tool error code: {err}");
    client.shutdown().await;
}

#[tokio::test]
async fn tool_level_errors_are_is_error_results() {
    let mut client = Client::spawn(&fixture_root()).await;
    // Nonexistent file: tool-level error (isError=true, text visible to the caller).
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": "nope.rs"}}),
        )
        .await
        .expect("outline on a nonexistent file");
    assert_eq!(
        result["isError"],
        json!(true),
        "tool-level errors go through isError: {result}"
    );

    // Path escape: tool-level error.
    let result = client
        .request(
            "tools/call",
            json!({"name": "read", "arguments": {"file_path": "../../../etc/passwd"}}),
        )
        .await
        .expect("read with an escaping path");
    assert_eq!(result["isError"], json!(true));

    // Invalid regex: tool-level error with the frozen recovery hint.
    let result = client
        .request(
            "tools/call",
            json!({"name": "search", "arguments": {"pattern": "(["}}),
        )
        .await
        .expect("search with an invalid pattern");
    assert_eq!(result["isError"], json!(true));
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Invalid regex pattern:"),
        "error text must carry the frozen hint: {text}"
    );

    // Unknown language extension.
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": "notes.md"}}),
        )
        .await
        .expect("outline md");
    assert_eq!(result["isError"], json!(true));
    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// Text-wire round trips: the default `mindctx serve` mode (LLM injection surface).
//
// The historical assertions above run against `--wire envelope` because they parse the
// structured envelope. These tests run against the default text wire to verify the
// production-default surface the model actually sees. The page is golden-locked as
// `wire_text/<tool>_cli.txt` in this crate.
// ---------------------------------------------------------------------------

/// Reads the text-wire fixture for the CLI subprocess variant (toolname_cli.txt).
/// Normalizes CRLF to LF: a windows checkout applies git's autocrlf to the tracked
/// fixture (CRLF on disk) while the program output is always LF — without the
/// normalize the windows-latest matrix lane fails every text-wire assertion with
/// "left != right" by exactly the carriage returns git inserted.
fn wire_text_fixture(tool: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("contract/wire_text")
        .join(format!("{tool}_cli.txt"));
    std::fs::read_to_string(&path)
        .expect("wire_text fixture must exist")
        .replace("\r\n", "\n")
}

/// Pin the polyglot fixture files' mtimes so the default search sort (mtime DESC,
/// then rel_display bytes ASC) produces a deterministic order under any checkout
/// state — fresh CI clones give every file the same mtime and the secondary key
/// collapses the result to alphabetical, which breaks the byte-locked
/// `wire_text/search_cli.txt` fixture. Order matches the fixture's locked body:
/// src/lib.rs newest, then main.cpp, README.md, app.py. Files that don't carry
/// `retry_with_backoff` are left alone (they cannot enter the result page).
fn pin_polyglot_search_mtimes(root: &Path) {
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    // Four candidate files, mtime DESC matches the fixture's locked order
    // (src/lib.rs newest, app.py oldest).
    let slots: &[(&str, u64)] = &[
        ("app.py", 0),
        ("README.md", 1),
        ("main.cpp", 2),
        ("src/lib.rs", 3),
    ];
    for (rel, offset_secs) in slots {
        let path = root.join(rel);
        let file = match File::options().write(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mtime = base + Duration::from_secs(*offset_secs);
        let _ = file.set_modified(mtime);
    }
}

#[tokio::test]
async fn text_wire_default_search_round_trip_is_rendered_page() {
    // The default search sort is mtime DESC, then rel_display bytes ASC. On a fresh
    // git checkout (CI, `cargo clean` reset) every fixture file has the same mtime
    // and the result collapses to alphabetical, breaking the byte-locked fixture
    // (CI surfaces this as: actual "README.md,app.py,main.cpp,src/lib.rs" vs
    // expected "src/lib.rs,main.cpp,README.md,app.py"). Pin the four candidate
    // files' mtimes here so the relative order is the one the fixture was locked
    // against; the assertion stays a default-sort contract test.
    pin_polyglot_search_mtimes(&fixture_root());
    let mut client = Client::spawn_with_wire(&fixture_root(), WireMode::Text).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "search", "arguments": {"pattern": "retry_with_backoff"}}),
        )
        .await
        .expect("search round trip");
    assert_eq!(result["isError"], json!(false), "search should not error");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result must be text content");
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "text wire must not be envelope JSON: {text}"
    );
    assert_eq!(text, wire_text_fixture("search"));
    client.shutdown().await;
}

#[tokio::test]
async fn text_wire_default_glob_round_trip_is_rendered_page() {
    let mut client = Client::spawn_with_wire(&fixture_root(), WireMode::Text).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "glob", "arguments": {"pattern": "**/*.go"}}),
        )
        .await
        .expect("glob round trip");
    assert_eq!(result["isError"], json!(false), "glob should not error");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result must be text content");
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "text wire must not be envelope JSON: {text}"
    );
    assert_eq!(text, wire_text_fixture("glob"));
    client.shutdown().await;
}

#[tokio::test]
async fn text_wire_default_read_round_trip_is_rendered_page() {
    let mut client = Client::spawn_with_wire(&fixture_root(), WireMode::Text).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "read", "arguments": {"file_path": "main.go", "offset": 1, "limit": 4}}),
        )
        .await
        .expect("read round trip");
    assert_eq!(result["isError"], json!(false), "read should not error");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result must be text content");
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "text wire must not be envelope JSON: {text}"
    );
    assert_eq!(text, wire_text_fixture("read"));
    client.shutdown().await;
}

#[tokio::test]
async fn text_wire_default_outline_round_trip_is_rendered_page() {
    let mut client = Client::spawn_with_wire(&fixture_root(), WireMode::Text).await;
    let result = client
        .request(
            "tools/call",
            json!({"name": "outline", "arguments": {"path": "src/lib.rs"}}),
        )
        .await
        .expect("outline round trip");
    assert_eq!(result["isError"], json!(false), "outline should not error");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result must be text content");
    assert!(
        serde_json::from_str::<Value>(text).is_err(),
        "text wire must not be envelope JSON: {text}"
    );
    assert_eq!(text, wire_text_fixture("outline"));
    client.shutdown().await;
}

/// Verifies that a Codex host that skips `tools/list` (permitted by the MCP spec)
/// and calls a tool directly still gets the correct pool profile (Codex hard cap).
/// The server captures `clientInfo.name` during `initialize`, not in `list_tools`.
#[tokio::test]
async fn codex_skips_list_tools_direct_tool_call_succeeds() {
    // Spawn with a custom client name to simulate Codex CLI.
    let mut client = Client::spawn_with_codex_name(&fixture_root()).await;
    // Skip tools/list entirely — call a tool directly after initialize.
    let result = client
        .request(
            "tools/call",
            json!({"name": "search", "arguments": {"pattern": "retry_with_backoff", "head_limit": 3}}),
        )
        .await
        .expect("direct tool call after Codex initialize should succeed");
    assert_eq!(result["isError"], json!(false), "search should not error");
    let env = envelope_of(&result);
    assert!(
        env["token_usage"]["returned"].as_u64().unwrap() > 0,
        "token accounting must be present"
    );
    client.shutdown().await;
}
