# MindCtx

[![CI](https://github.com/HouByte/mindctx/actions/workflows/ci.yml/badge.svg)](https://github.com/HouByte/mindctx/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

> **Local context engineering for coding agents.**
> Built-in tools get you *something*. mindctx gets you the *right thing, within budget, and remembers.*
>
> [中文说明](README.zh-CN.md) · [License](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE)

mindctx is a single Rust binary that runs as an MCP server, giving your coding agent a fast,
budget-aware way to look around a repository. No daemon, no remote service — it indexes and
serves from your machine, respecting your `gitignore`.

## Install

**One-line install (macOS / Linux / WSL):**
```bash
curl -fsSL https://mindctx.com/install.sh | bash
```

**One-line install (Windows PowerShell):**
```powershell
irm https://mindctx.com/install.ps1 | iex
```

Pin a version with `MINDCTX_VERSION=v0.1.0` (e.g. `curl -fsSL ... | MINDCTX_VERSION=v0.1.0 bash`, or `$env:MINDCTX_VERSION='v0.1.0'; irm ... | iex` on PowerShell).

Prebuilt binaries cover macOS (x64 / arm64), Linux x64 (musl), and Windows x64; **Linux arm64 has no prebuilt artifact** (use `cargo install mindctx`).

```bash
npm i -g mindctx                                         # npm
cargo install mindctx                                    # crates.io
```

Verify:

```bash
mindctx --version
```

Then connect it to your agent (see [MCP setup](#mcp-setup)).

### What install does

Besides dropping the binary in `~/.local/bin`, the install script wires mindctx into every
host it finds on `PATH`:

| Step | Effect |
|---|---|
| MCP registration | `claude mcp add --scope user` / `codex mcp add` register mindctx as a user-scoped MCP server (`mindctx serve`, stdio) |
| Agent prompt | The block in [`scripts/agent-prompt.md`](scripts/agent-prompt.md) is written to `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md`, wrapped in `<!-- mindctx:begin -->` / `<!-- mindctx:end -->` markers |
| Runtime config | A `[core]` section wrapped in `# mindctx:begin:core` / `# mindctx:end:core` markers is written to `~/.mindctx/config.toml` |

The prompt block is fetched from the same release tag as the binary, so it always matches the
installed version. Hosts whose CLI is not on `PATH` are skipped, and only home-directory files
are touched — project files are never modified.

Every step is idempotent: re-running the installer replaces the mindctx blocks in place and
leaves the rest of each file alone. MCP registration is best-effort (a failure prints a
warning and the install continues); a failed prompt-template download stops the install with
an error rather than half-configuring a host.

### Manual setup

The installer is a convenience, not a requirement. To wire mindctx in by hand:

1. Register the server: `claude mcp add --scope user --transport stdio mindctx -- mindctx serve`,
   `codex mcp add mindctx -- mindctx serve`, or edit the host config directly (see
   [MCP setup](#mcp-setup)).
2. Append the block in [`scripts/agent-prompt.md`](scripts/agent-prompt.md) to your agent's
   instruction file (`~/.claude/CLAUDE.md` or `~/.codex/AGENTS.md`).
3. Optionally create `~/.mindctx/config.toml` with a `[core]` section for runtime settings.

To undo it, delete the block you appended, remove the entry with
`claude mcp remove mindctx --scope user` / `codex mcp remove mindctx`, and delete any file left
empty.

### Uninstall

**macOS / Linux / WSL:**
```bash
curl -fsSL https://mindctx.com/uninstall.sh | bash
```

Pass `--purge` to also delete `~/.mindctx/record/` (receipts and backups left by the removed
`mindctx apply`), which is kept by default:
`curl -fsSL https://mindctx.com/uninstall.sh | bash -s -- --purge`

**Windows (PowerShell):**
```powershell
irm https://mindctx.com/uninstall.ps1 | iex
```

On PowerShell, pass `-Purge` by invoking the script rather than piping it:
`& ([scriptblock]::Create((irm https://mindctx.com/uninstall.ps1))) -Purge`

Uninstall reverses exactly what install created: it strips only its own marker blocks (removing
a host file that ends up empty), asks the host CLIs to drop the mindctx entry, and deletes only
its own binary. Anything else in those files is preserved.

## Why mindctx

Three kinds of waste, three countermeasures:

| Waste | Fix |
|---|---|
| **Navigation waste** — grep round-trips, re-walking the tree | Persistent index; `search`/`glob` answer in one call |
| **Capacity waste** — reading a whole file to change ten lines | Token budgets on every tool; exact o200k accounting |
| **Repetition waste** — re-learning the same thing every session | Layered knowledge base (markdown, git-friendly, cross-host) |

## Tools

Four MCP tools, on by default:

| Tool | What it does |
|---|---|
| `search` | Regex content search (Rust regex). 4 output modes, glob/type filters, `head_limit`+`offset` pagination, a skip report. Newest-first, **no relevance ranking** — judging is the agent's job. |
| `glob` | File discovery with `!` exclusions, filter modes, path/modified ordering, pagination. |
| `read` | Text read with 1-based lines, a token budget as the only ceiling, batch reads of 1–32 files sharing one budget, encoding auto-detection. |
| `outline` | File symbol skeleton via tree-sitter (6 languages) — see structure before reading the whole file. |

Every result is an **envelope** with exact token accounting (`token_usage`) and a
`terminal`/`next_call` continuation contract: truncation is a cursor, not a dead end.

## MCP setup

Add mindctx as an MCP server in your agent's config:

```json
{
  "command": "/path/to/mindctx",
  "args": ["serve"]
}
```

`serve` exposes the four tools over stdio JSON-RPC. Point it at a project root with
`--root /path/to/project`, or let it inherit the current directory. The project root is the
search corpus (`gitignore` respected).

Paths handed to the tools are not confined to the root: absolute paths, `~`-prefixed
paths, and `..`-normalized forms resolve anywhere on the filesystem. Inside WSL,
Windows-form inputs (`C:\Users\...`, `\\wsl$\...`) are converted automatically. The
tools stay read-only, but there is no root sandbox — the server can read any file the
running user can.

Useful flags:

- `--wire text|envelope` — wire presentation mode. `text` (default) is the LLM injection
  surface: the model sees a single token-budgeted text page per call. `envelope` returns the
  complete envelope JSON for machine consumers (HTTP / IDE plugin / contract test).
- `MINDCTX_WIRE` env var overrides `--wire` when the flag is absent.

## Agent prompt

mindctx has a two-layer prompt mechanism:

1. **Server instructions** are delivered automatically at MCP connect (no configuration needed).
2. The optional **prompt block** below teaches an agent to *prefer* mindctx tools over built-in equivalents.

```markdown
<!-- mindctx:begin -->
Code navigation: prefer the mindctx MCP tools (search / glob / read / outline) over built-in grep/glob/read — one budgeted call replaces repeated round-trips.

- Cross-file questions → `search` (regex); file discovery → `glob`. Results are mtime-ordered, not ranked — judging relevance is your job.
- `outline` before `read` on code files (rs / go / ts / py / java / c-family — other extensions skip it); either way, `read` with offset/limit ranges.
- A `Partial` page is a cursor: resume only with the exact arguments its status line (or `next_call`) names.
- Paths may be project-relative, absolute, `~`-prefixed, or contain `..` — no need to `cd` first.
<!-- mindctx:end -->
```

The installer appends this block automatically (steps 2–3 above). Manual setup is the same procedure done by hand.

| Host | File | How |
|---|---|---|
| Claude Code | `~/.claude/CLAUDE.md` (user) or project `CLAUDE.md` | append the block |
| Codex | `~/.codex/AGENTS.md` (user) or project `AGENTS.md` | append the block |
| any AGENTS.md-compatible host | its instruction file | append the block |

## Status and index

Read-only helpers:

- `status` — version, project root, run-dir presence, index status, retrieval corpus size.
- `index` — corpus walk report (file count, top extensions, total bytes). Retrieval
  queries the rg layer directly, so no prebuilt index is required.

## Contributing

Build, debug, and release instructions live in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT OR Apache-2.0
