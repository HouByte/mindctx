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

```bash
npm i -g mindctx                                         # npm
cargo install mindctx                                    # crates.io
```

Verify:

```bash
mindctx --version
```

Then connect it to your agent (see [MCP setup](#mcp-setup)).

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

Useful flags:

- `--wire text|envelope` — wire presentation mode. `text` (default) is the LLM injection
  surface: the model sees a single token-budgeted text page per call. `envelope` returns the
  complete envelope JSON for machine consumers (HTTP / IDE plugin / contract test).
- `MINDCTX_WIRE` env var overrides `--wire` when the flag is absent.

## Contributing

Build, debug, and release instructions live in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT OR Apache-2.0
