// SPDX-License-Identifier: MIT OR Apache-2.0

//! mindctx MCP service. Four tools: search / glob / read / outline.
//!
//! Wire presentation ([`mindctx_core::wire`]): default text wire sends one rendered page per result;
//! `MINDCTX_WIRE=envelope` / `serve --wire envelope` restores the full envelope JSON (compact, v3).
//! Errors: invalid params + tool failures surface as `Ok(CallToolResult::error(...))`;
//! only an unknown tool name is a protocol-level `Err(McpError)` (-32602).
//!
//! Debugging: `npx @modelcontextprotocol/inspector mindctx serve`

use std::path::PathBuf;
use std::sync::Arc;

use mindctx_core::budget::{self, ToolKind};
use mindctx_core::envelope::Envelope;
use mindctx_core::error::Error;
use mindctx_core::guard::{self, GuardedTurnPool, TURN_GAP, profile_for_host};
use mindctx_core::retrieve::glob_tool::{GlobParams, glob_with_budget};
use mindctx_core::retrieve::read::{ReadParams, read_with_budget};
use mindctx_core::retrieve::{DEFAULT_OUTLINE_DEPTH, SearchParams, search_with_budget};
use mindctx_core::symbol::{OutlineParams, outline_with_budget};
use mindctx_core::tokenize::count_tokens;
use mindctx_core::wire::WireMode;

/// Wire-mode dispatch profile: the render function, server instructions, and whether
/// the turn pool is active (text wire only). Adding a new [`WireMode`] variant requires
/// updating only [`wire_profile`].
#[derive(Clone, Copy)]
struct WireProfile {
    render: fn(&Envelope) -> String,
    instructions: &'static str,
    pool_enabled: bool,
}

/// Returns the dispatch profile for `wire`. Adding a new [`WireMode`] variant requires
/// updating only this function and [`WireProfile`].
fn wire_profile(wire: WireMode) -> WireProfile {
    match wire {
        WireMode::Text => WireProfile {
            render: mindctx_core::wire::render,
            instructions: INSTRUCTIONS_TEXT,
            pool_enabled: true,
        },
        WireMode::Envelope => WireProfile {
            render: |env| serde_json::to_string(env).expect("envelope serialization cannot fail"),
            instructions: INSTRUCTIONS_ENVELOPE,
            pool_enabled: false,
        },
    }
}

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::*,
    schemars,
    service::{NotificationContext, RequestContext},
    tool, tool_handler, tool_router,
    transport::stdio,
};

/// Per-wire-mode server instructions: the first paragraph stays within 250 characters,
/// and both variants name the four tools and the exact token accounting.
const INSTRUCTIONS_TEXT: &str = "mindctx: local file tools — search, glob, read, outline. Responses are token-budgeted text pages: a (Partial|Complete…) status line at the end names the exact resume arguments when a page is partial.\nTool-call contract:\n- Resume a Partial page only with the exact arguments its status line carries; failures arrive as self-contained tool errors.\n- read batch calls (files, 1-32 entries) share one token budget: resume with the exact files array the status line carries.\n";

const INSTRUCTIONS_ENVELOPE: &str = "mindctx: local file tools — search, glob, read, outline. Every response is envelope JSON with exact token accounting (token_usage).\nTool-call contract:\n- envelope.terminal says Complete or Partial. Resume only through the exact arguments envelope.next_call carries; failures arrive as self-contained tool errors.\n- read batch calls (files, 1-32 entries) share one token budget: resume only with the exact files array a Partial next_call carries.\n";

/// Outline wire params: `OutlineParams` borrows the server-owned root, so the wire
/// struct carries only the caller-supplied fields.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct OutlineRequest {
    /// File path (project-relative)
    pub path: String,
    /// Outline depth in levels (default 2)
    #[serde(default)]
    pub depth: Option<u32>,
}

/// MCP server instance: holds the project root (injected by the CLI at startup; core
/// pure-lib discipline), the resolved wire mode, and the lazily-built guarded
/// per-turn output pool. The pool is constructed once from the MCP `clientInfo.name`
/// captured during `initialize` (see [`MindctxServer::pool`]).
pub struct MindctxServer {
    root: PathBuf,
    wire: WireMode,
    /// Captured during `initialize`: the host identity drives [`profile_for_host`].
    host_name: std::sync::OnceLock<Option<String>>,
    /// One-shot pool initialization. The `get_or_init` closure captures `host_name`
    /// so the pool is built exactly once with the correct profile.
    pool: std::sync::OnceLock<Arc<GuardedTurnPool>>,
}

impl MindctxServer {
    /// Text wire (the default LLM injection surface). The pool is built on demand
    /// once the host identity arrives through `initialize` (see [`Self::pool`]).
    pub fn new(root: PathBuf) -> Self {
        Self::with_wire(root, WireMode::Text)
    }

    /// Explicit wire mode (the CLI resolves `--wire` / `MINDCTX_WIRE`).
    pub fn with_wire(root: PathBuf, wire: WireMode) -> Self {
        Self {
            root,
            wire,
            host_name: std::sync::OnceLock::new(),
            pool: std::sync::OnceLock::new(),
        }
    }

    /// Returns the guarded turn pool, building it on first call. Idempotent.
    /// The pool uses the host name captured during `initialize`; if no client info
    /// was observed (older client, or a path that skipped `list_tools` before any
    /// tool call) the plain profile applies.
    fn pool(&self) -> Arc<GuardedTurnPool> {
        self.pool
            .get_or_init(|| {
                let host = self.host_name.get().and_then(|opt| opt.as_deref());
                let profile = profile_for_host(host);
                GuardedTurnPool::new(profile.pool_budget, profile.hard_cap, TURN_GAP)
            })
            .clone()
    }
}

#[tool_router]
impl MindctxServer {
    /// Runs one tool under its resolved budget and renders the result.
    ///
    /// Text wire: the turn pool's fair-share allowance replaces the budget; a page
    /// that exhausts its share (or overshoots it) is replaced by the frozen stub
    /// ([`guard::render_stub`]) and the exact rendered count is charged to the pool.
    /// Envelope wire: the pool is a no-op (envelope output is not a budget surface)
    /// and the standard renderer runs. `finish_wire` stays untouched — the allowance
    /// already bounds the wire, so core fitting never trips OverBudget. Returns the
    /// envelope plus, when the pool delivered the page itself, the final text (`Some`).
    async fn run_page<F>(&self, kind: ToolKind, work: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(u64) -> Result<Envelope, Error> + Send + 'static,
    {
        let profile = wire_profile(self.wire);
        let pool = profile.pool_enabled.then(|| self.pool());
        let inner: Result<(Envelope, Option<String>), Error> = self
            .run_tool(move || {
                let normal = resolve_budget(kind)?;
                let mut claim = pool.map(|pool| pool.begin().claim(normal));
                let budget = claim.as_ref().map_or(normal, |claim| claim.allowance());
                let mut env = work(budget)?;
                let guarded_text = claim.take().map(|claim| {
                    let text = if claim.exhausted() {
                        // Preserve envelope metadata (terminal, next_call) for the model:
                        // mutate the envelope text in place and route through
                        // `envelope_result` so the wrapper always sees both wire paths.
                        let stub = guard::render_stub(claim.allowance());
                        env.text = Some(stub);
                        (profile.render)(&env)
                    } else {
                        (profile.render)(&env)
                    };
                    let actual = count_tokens(&text);
                    claim.complete(actual);
                    text
                });
                Ok((env, guarded_text))
            })
            .await?;
        match inner {
            Ok((_env, Some(text))) => Ok(self.envelope_result_with_text(&_env, text)),
            Ok((env, None)) => Ok(self.envelope_result(&env)),
            Err(error) => Ok(Self::tool_error(error)),
        }
    }

    /// Envelope wire success path (pool never fires on envelope).
    fn envelope_result(&self, env: &Envelope) -> CallToolResult {
        let profile = wire_profile(self.wire);
        let text = (profile.render)(env);
        CallToolResult::success(vec![ContentBlock::text(text)])
    }

    /// Text wire success path: both the guarded-text branch and the unguarded branch
    /// converge here so any wrapper around `envelope_result` covers both wire paths.
    fn envelope_result_with_text(&self, env: &Envelope, _text: String) -> CallToolResult {
        self.envelope_result(env)
    }

    /// Tool execution failure: text visible to the caller (as opposed to a protocol-level Err(McpError)).
    fn tool_error(err: mindctx_core::error::Error) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text(err.to_string())])
    }

    /// Runs one tool's synchronous core work on the blocking pool so a long traversal,
    /// decode, or fit never stalls concurrent tool calls on the async runtime workers.
    /// The closure owns its data (root/params cloned or moved); `Envelope` and `Error`
    /// are plain owned data, so the result is `Send + 'static`.
    async fn run_tool<T, F>(&self, work: F) -> Result<T, McpError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| McpError::internal_error(format!("tool task failed: {error}"), None))
    }

    #[tool(
        description = "Regex search over file contents. Patterns use Rust regex syntax: lookaround and backreferences are unavailable, and literal braces must be escaped. Output modes: files_with_matches (default), content, count (occurrence counts, not line counts), and summary. Ignore rules from .gitignore/.ignore apply; dotfiles are searched; .git and binary content are skipped and symlinked directories are not entered. Files come back newest-first; anything skipped or unreachable is itemized in the skip report. the final status line says Complete or Partial and carries the exact resume arguments; failures arrive as self-contained tool errors.",
        title = "Search file contents",
        annotations(
            title = "Search file contents",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        // Business logic lives in core; the handler only shuttles params and blocks the
        // blocking pool (not the async workers) for the duration of the synchronous
        // core work; the turn pool is enforced inside `run_page`.
        let root = self.root.clone();
        self.run_page(ToolKind::Search, move |budget| {
            search_with_budget(&root, &params, budget)
        })
        .await
    }

    #[tool(
        description = "Path matching with glob pattern(s) such as `**/*.rs`; a `!` prefix marks an exclusion, and exclusions always win. Knobs: sort=modified (default)|path, output_mode=paths (default)|details, filter_mode=ignore (default; honors .ignore and gitignore rules, prunes .git)|all for ignore-rule handling, limit 1-1000 (default 100). the final status line says Complete or Partial and carries the exact resume arguments; failures arrive as self-contained tool errors.",
        title = "Match file paths",
        annotations(
            title = "Match file paths",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn glob(
        &self,
        Parameters(params): Parameters<GlobParams>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.root.clone();
        self.run_page(ToolKind::Glob, move |budget| {
            glob_with_budget(&root, &params, budget)
        })
        .await
    }

    #[tool(
        description = "Read a local file — 1-based lines with encoding auto-detection and normalization — or a batch of 1-32 entries via files=[{path,offset,limit,encoding}] that shares a single output token budget and reports per-entry problems inline. In batch mode, the partial status line carries the exact files array to resume with. the final status line says Complete or Partial and carries the exact resume arguments; failures arrive as self-contained tool errors.",
        title = "Read a local file",
        annotations(
            title = "Read a local file",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn read(
        &self,
        Parameters(params): Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.root.clone();
        self.run_page(ToolKind::Read, move |budget| {
            read_with_budget(&root, &params, budget)
        })
        .await
    }

    #[tool(
        description = "Structural outline of a source file: the function/class/module tree without pulling in the whole file. path is project-relative; depth caps how many skeleton levels come back (default 2). the final status line says Complete or Partial and carries the exact resume arguments; failures arrive as self-contained tool errors.",
        title = "File symbol outline",
        annotations(
            title = "File symbol outline",
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn outline(
        &self,
        Parameters(req): Parameters<OutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.root.clone();
        self.run_page(ToolKind::Outline, move |budget| {
            outline_with_budget(
                &OutlineParams {
                    root: &root,
                    path: &req.path,
                    depth: Some(req.depth.unwrap_or(DEFAULT_OUTLINE_DEPTH)),
                },
                budget,
            )
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for MindctxServer {
    /// Captures `clientInfo.name` from the `initialize` handshake so the turn pool
    /// profile (Codex vs plain) is available even when a host skips `tools/list` and
    /// calls a tool directly. The host name is fed into the pool factory in
    /// [`Self::pool`]. `on_initialized` is called once per session regardless of
    /// whether `list_tools` is invoked.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        // `context.peer.peer_info()` is set by the default `initialize` handler before
        // `on_initialized` fires, for all sessions (both inline-lifecycle and ones
        // that ran `initialize` explicitly).
        let name = context
            .peer
            .peer_info()
            .map(|info| info.client_info.name.clone());
        let _ = self.host_name.set(name);
    }

    /// Published tool list with every input schema normalized to the portable subset
    /// `tools/list` and `get_tool` observe the same shape.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        let mut tools = Self::tool_router().list_all();
        for tool in &mut tools {
            tool.input_schema = normalize_input_schema(&tool.input_schema)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        Ok(ListToolsResult {
            tools,
            result_type: Some(ResultType::COMPLETE),
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
            ..Default::default()
        })
    }

    /// Schema by name, normalized exactly like the `tools/list` entry.
    fn get_tool(&self, name: &str) -> Option<Tool> {
        let mut tool = Self::tool_router().get(name)?.clone();
        // The schemas are compile-time constants; if any of them failed to normalize,
        // `list_tools` would already have failed with the same loud error.
        tool.input_schema = normalize_input_schema(&tool.input_schema)
            .expect("published tool schemas must normalize");
        Some(tool)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("mindctx", mindctx_core::VERSION)
                    .with_title("mindctx — local context engineering layer"),
            )
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(wire_profile(self.wire).instructions)
    }
}

/// Reads one raw budget env value at the process boundary. A set-but-non-UTF-8 value
/// is a malformed budget (the same frozen message a non-number gets).
fn read_budget_var(name: &str) -> Result<Option<String>, Error> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::Config(
            "budget must be a positive integer".to_string(),
        )),
    }
}

/// Resolves a tool's token budget: per-tool env var → `MINDCTX_TOKEN_BUDGET` → default.
/// The MCP layer is the process boundary — core itself never reads env.
fn resolve_budget(kind: ToolKind) -> Result<u64, Error> {
    let global = read_budget_var(budget::GLOBAL_BUDGET_VAR)?;
    let per_tool = read_budget_var(kind.env_var())?;
    budget::resolve_budget(kind, global.as_deref(), per_tool.as_deref())
}

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

/// Inlining depth cap: `$ref` resolution and the walk stop here. Purely defensive — the
/// produced param schemas are three levels deep at most (`read.files.items.properties.*`
/// after `BatchEntry` inlining). Overflowing the cap is a loud normalization failure, not
/// a degraded schema (see [`SchemaNormalizeError`]).
pub const SCHEMA_DEPTH_CAP: usize = 32;

/// Why a schema could not be normalized. Published schemas are static, so any of these
/// is a contract bug (e.g. a schemars upgrade changing the `$defs` ref prefix): the
/// surface must fail loudly instead of silently publishing a constraint-free `{}`.
#[derive(Debug)]
pub struct SchemaNormalizeError(String);

impl std::fmt::Display for SchemaNormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool input schema normalization failed: {}", self.0)
    }
}

/// Normalizes one schema node to the portable subset: strips `$schema`,
/// `additionalProperties`, `format`, and any other non-portable key; inlines `$ref`
/// against `defs`; collapses `type: [T, "null"]` to `T` and `anyOf: [{T}, {null}]` to
/// `T`. Errors loudly on an unresolvable `$ref` or depth-cap overflow. Bounded depth
/// (see [`SCHEMA_DEPTH_CAP`]).
pub fn normalize_schema_node(
    value: &serde_json::Value,
    depth: usize,
    defs: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, SchemaNormalizeError> {
    if depth > SCHEMA_DEPTH_CAP {
        return Err(SchemaNormalizeError("schema depth cap exceeded".into()));
    }
    let Some(obj) = value.as_object() else {
        return Ok(value.clone());
    };
    // $ref: "#/$defs/Name" inlines the named subschema (schemars emits $ref/$defs for
    // named types such as enums); non-portable, so it never survives normalization.
    // Portable siblings (e.g. the field's own description/default) are kept on top.
    if let Some(reference) = obj.get("$ref").and_then(serde_json::Value::as_str) {
        let name = reference.strip_prefix("#/$defs/").unwrap_or(reference);
        let target = defs
            .and_then(|defs| defs.get(name))
            .ok_or_else(|| SchemaNormalizeError(format!("unresolved $ref: {reference}")))?;
        let resolved = normalize_schema_node(target, depth.saturating_add(1), defs)?;
        let mut out = match resolved {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), other);
                map
            }
        };
        copy_portable_siblings(obj, &["$ref"], &mut out);
        return Ok(serde_json::Value::Object(out));
    }
    // anyOf: [{T}, {null}] (or reversed) collapses to the non-null branch, keeping the
    // wrapper's portable siblings (schemars puts a field's doc comment on the anyOf
    // wrapper, e.g. for Option<NamedType>, and losing it would publish an undescribed param).
    if let Some(any_of) = obj.get("anyOf").and_then(serde_json::Value::as_array)
        && any_of.len() == 2
        && let Some(null_index) = any_of.iter().position(is_null_schema)
    {
        let collapsed =
            normalize_schema_node(&any_of[1 - null_index], depth.saturating_add(1), defs)?;
        let mut out = match collapsed {
            serde_json::Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), other);
                map
            }
        };
        copy_portable_siblings(obj, &["anyOf"], &mut out);
        return Ok(serde_json::Value::Object(out));
    }
    // A fieldless enum (`oneOf` of `const` strings) collapses to type:"string" + enum.
    // Defensive: schemars 1.2.2 emits unit-only enums as `type`+`enum` directly, so this
    // shape is not produced today — kept so a schemars shape change still publishes the
    // variant names instead of dropping them.
    if let Some(collapsed) = collapse_const_enum(obj) {
        let mut out = serde_json::Map::new();
        out.insert(
            "type".to_string(),
            serde_json::Value::String("string".to_string()),
        );
        out.insert("enum".to_string(), serde_json::Value::Array(collapsed));
        copy_portable_siblings(obj, &["oneOf"], &mut out);
        return Ok(serde_json::Value::Object(out));
    }
    let mut out = serde_json::Map::new();
    for (key, val) in obj {
        if !PORTABLE_SCHEMA_KEYS.contains(&key.as_str()) {
            continue;
        }
        match key.as_str() {
            "type" => {
                // `type: ["null"]` alone is useless as a published param schema; dropping
                // the key is louder than publishing it.
                if let Some(collapsed) = collapse_nullable_type(val) {
                    out.insert(key.clone(), collapsed);
                }
            }
            "properties" => {
                if let Some(props) = val.as_object() {
                    let props: Result<serde_json::Map<String, serde_json::Value>, _> = props
                        .iter()
                        .map(|(k, v)| {
                            Ok((
                                k.clone(),
                                normalize_schema_node(v, depth.saturating_add(1), defs)?,
                            ))
                        })
                        .collect();
                    out.insert(key.clone(), serde_json::Value::Object(props?));
                }
            }
            "items" => {
                let normalized = normalize_schema_node(val, depth.saturating_add(1), defs)?;
                out.insert(key.clone(), normalized);
            }
            _ => {
                // `default` is copied verbatim: a non-empty object default would fail the
                // test-side portable walker (asymmetric today, harmless today — no
                // published param has an object default).
                out.insert(key.clone(), val.clone());
            }
        }
    }
    Ok(serde_json::Value::Object(out))
}

/// Copies the wrapper-level portable keys (e.g. a field's own `description`/`default`)
/// onto a collapsed or inlined schema, skipping keys already handled by the caller.
fn copy_portable_siblings(
    source: &serde_json::Map<String, serde_json::Value>,
    skip: &[&str],
    out: &mut serde_json::Map<String, serde_json::Value>,
) {
    for (key, val) in source {
        if !skip.contains(&key.as_str()) && PORTABLE_SCHEMA_KEYS.contains(&key.as_str()) {
            out.insert(key.clone(), val.clone());
        }
    }
}

/// `{"type": "null"}` recognition for the anyOf collapse.
fn is_null_schema(value: &serde_json::Value) -> bool {
    value
        .as_object()
        .and_then(|obj| obj.get("type"))
        .is_some_and(|t| t == "null")
}

/// A fieldless string enum (`oneOf` of `{const: "...", type: "string"}` variants, as
/// schemars emits it) collapses to its const values; `None` for anything else.
fn collapse_const_enum(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<Vec<serde_json::Value>> {
    let one_of = obj.get("oneOf")?.as_array()?;
    if one_of.is_empty() {
        return None;
    }
    one_of
        .iter()
        .map(|variant| {
            variant
                .as_object()?
                .get("const")?
                .as_str()
                .map(|c| serde_json::Value::String(c.to_string()))
        })
        .collect()
}

/// `type: [T, "null"]` collapses to `T`; a lone `["null"]` (or an empty array) yields
/// `None` — the key must be dropped, not published; a multi-valued type passes through.
fn collapse_nullable_type(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => {
            let non_null: Vec<&serde_json::Value> = items
                .iter()
                .filter(|t| t.as_str() != Some("null"))
                .collect();
            match non_null.as_slice() {
                [single] => Some((*single).clone()),
                [] => None,
                multiple => Some(
                    multiple
                        .iter()
                        .map(|t| (*t).clone())
                        .collect::<Vec<_>>()
                        .into(),
                ),
            }
        }
        other => Some(other.clone()),
    }
}

/// Normalizes a published input schema map (see [`normalize_schema_node`]): root-level
/// `$defs` are harvested for `$ref` inlining, then dropped with `$schema`. An error here
/// unresolved `$ref`, depth overflow) must abort publication — never degrade to `{}`.
fn normalize_input_schema(
    schema: &Arc<JsonObject>,
) -> Result<Arc<JsonObject>, SchemaNormalizeError> {
    let mut value = serde_json::Value::Object(schema.as_ref().clone());
    let defs = value
        .get("$defs")
        .and_then(serde_json::Value::as_object)
        .cloned();
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    match normalize_schema_node(&value, 0, defs.as_ref())? {
        serde_json::Value::Object(map) => Ok(Arc::new(map)),
        _ => Err(SchemaNormalizeError(
            "normalized tool input schema is not an object".into(),
        )),
    }
}

/// Runs the MCP server over stdio (`mindctx serve`).
/// `root`: project root, the corpus scope for search/outline/read; the CLI defaults to the
/// current working directory. `wire`: the resolved presentation mode (`--wire`/`MINDCTX_WIRE`).
/// Note: stdout is the protocol channel; any logging must go to stderr.
pub async fn serve_stdio(root: PathBuf, wire: WireMode) -> Result<(), McpError> {
    let service = MindctxServer::with_wire(root, wire)
        .serve(stdio())
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    service
        .waiting()
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(())
}
