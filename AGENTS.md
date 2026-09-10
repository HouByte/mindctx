# AGENTS.md — mindctx

## Environment (project-local toolchain)

Rust toolchain lives in `.toolchains/` (`RUSTUP_HOME`/`CARGO_HOME` redirected). **Does not enter PATH, does not touch home or shell config**; `.toolchains/` is gitignored.

- **Activate before each build**: `source scripts/activate.sh` (exports env; auto-installs toolchain into project dir if missing)
- CI/release never depends on local: GitHub Actions uses `dtolnay/rust-toolchain@stable`
- Delete `.toolchains/` to uninstall — zero residue

## Common commands

```bash
source scripts/activate.sh
cargo build && cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # mirrors CI
cargo run -p mindctx -- --version
npx @modelcontextprotocol/inspector cargo run -p mindctx -- serve   # MCP debug
cargo run -p xtask -- <dist|npm|release|verify-publish|preflight|about>   # release automation
```

## Language policy

The primary reader of source text in this repo is the agent, not a human. To keep that reader fast and unambiguous:

| Layer | Language | Reason |
|---|---|---|
| Source comments (`///` `//!` `//`) | **English** | Agent is the primary reader; bilingual comments cost context budget and attention |
| `AGENTS.md` | **English** | This file is the agent's entry point |
| Commit messages | **English** | Conventional commits + locale-stable `git log` |
| MCP `description` / `instructions` / `with_title` | English | AI protocol strings |
| Error `Display` impls | English | Bubbles into MCP tool errors |
| `panic!` / `expect(...)` text | English | Panic backtrace + search-friendly |
| envelope field names / enum values | English | Schema contract |

This table is the source of truth. New code must follow it without re-debate.

## Commit hygiene

- **No autonomous commits for small tasks**: for a small change, finish the work, run the full pre-commit gate (item 8: fmt + clippy `-D warnings` + test), then stop and wait for review. Do not commit unless the user explicitly asks. A single small fix must never turn into a dozen commits.
- Each commit must leave the tree green on its own (item 8 gate: fmt + clippy `-D warnings` + test).
- Subject ≤72 chars, imperative mood, no trailing period. Conventional commits (per Language policy table). Body in bullets when context is needed.

## Development workflow

- **PR with rebase-merge**: changes land through a pull request; the GitHub "Rebase and merge" button fast-forwards `main` to the PR head, keeping history linear.
- **CI on PR only**: `ci.yml` triggers on `pull_request`. The PR is the merge gate — branch protection requires CI to be green before the merge button is enabled, so a failing PR can't land. Under rebase-merge the merged commit is the PR head unchanged, so a second CI run on the merge is unnecessary.

## Engineering discipline (violations get reverted)

1. **envelope is one locked contract**: single definition at `crates/core/src/envelope.rs`; the JSON shape is locked by inline assertions in `crates/tests/core/envelope_smoke.rs` and the shape literals in the envelope tests. A breaking field change must move those locks in the same change; additive fields never break old consumers (unknown fields deserialize as ignored). The MCP tool schema fixture at `crates/tests/contract/expected_schemas.json` locks the tools' input schemas, not the envelope shape.
2. **No business logic in mcp handlers**: transport coupling would force a rewrite for the second protocol line (HTTP / IDE plugin).
3. **core is a pure lib**: state is injected by callers, no process-level singletons.
4. **Don't reinvent wheels**: prefer established crates (clap, serde, etc.) over custom reimplementations; only build something custom when the existing options genuinely don't fit.
5. **Dependency versions live in `[workspace.dependencies]`**: crates always use `workspace = true`.
6. **New line of business = 4-step onboarding**: core module → mcp tool group (default-gated off) → cli subcommand → index corpus adapter (optional).
7. **Single version source**: `[workspace.package].version`; npm packages mirror it via `xtask release`.
8. **Pre-commit must be green**: fmt + clippy `-D warnings` + test (lefthook pre-commit hook).
9. **No Chinese in source files** (enforced by the `comment-language` CI job — see below); comments, error text, and protocol strings must be English.
10. **Docs are current-state truth, not a journal**: READMEs and this file state only what is still true — current facts, in-force decisions, open gates. No date-stamped history ("landed on …", "retired on …", "decided on …") and no internal task/issue IDs: the timeline is git history, and provenance is cited as a tag/commit/branch, never a calendar date or a tracker ID. A day-by-day work log, if needed, lives outside the repo, never inside.

## CI guard: comment-language

A dedicated job in `.github/workflows/ci.yml` blocks Chinese characters in source files (`*.rs` `*.js` `*.sh` `*.toml` `*.yml`/`*.yaml` across crates, tests, packages, scripts, and root configs). The check is full-tree. Exempt (may contain Chinese): other `*.md` and `crates/tests/fixtures/**` (multilingual test data). The job checks comment lines and Rust attribute strings — the layers the policy table governs. String-literal *data* (test inputs, e.g. the CJK samples in `retrieve::tests` / `tokenize::tests`) is machine input, not authored prose: it is out of scope and may be whatever the test requires.

---

`CLAUDE.md` is a single line `@AGENTS.md` — purely for multi-host compatibility (Claude Code read entry). Source of truth is always this file. Change conventions only here.