# Changelog

All notable changes to mindctx are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1]

### Added

- **Global path inputs**: `search` / `glob` / `read` / `outline` accept absolute paths, `~` home expansion, and lexical `..`/`.` normalization — tool inputs are no longer confined to the project root
- **WSL input conversion**: inside WSL, Windows-form paths (drive letters, backslashes, `\\wsl$` / `\\wsl.localhost` UNC prefixes) resolve onto their WSL mounts
- **Backslash fallback (non-WSL)**: a path that misses on disk and contains `\` retries slash-normalized
- **Agent prompt block**: install scripts append a marker-delimited prompt block (search / glob / read / outline over built-in equivalents) to the host instruction file — Claude Code (`CLAUDE.md`), Codex (`AGENTS.md`), and any AGENTS.md-compatible host; the block also carries the path-flexibility rule so server instructions and the prompt block agree
- **Path-flexibility rule** in server instructions and the agent prompt block: paths may be project-relative, absolute, `~`-prefixed, or contain `..` — no need to `cd` first

### Changed

- Search/glob results outside the server root render as absolute paths, so every returned path re-resolves to the same file
- MCP tool descriptions document path parameters as project-relative or absolute
- **Installer hardening**: `install.sh` / `install.ps1` download the prompt template before the binary, then run `mindctx --version` after install to fail loudly on a broken binary; install path is centralized in `BIN_DIR` so PATH check and post-install verify resolve to one location; Linux script prefers `sha256sum` over `shasum` when available

### Removed

- The `path must be project-relative` and `must not escape the project root` rejections. mindctx is a local read-only tool; root confinement was never a security boundary and the README says so explicitly

## [0.2.0]

### Removed

- `mindctx apply` / `mindctx unapply` subcommands and the `core::control` module; host config init moves to the install/uninstall scripts

### Added

- **Distribution-time init**: install scripts register mindctx as an MCP server in detected hosts (`claude mcp add`, `codex mcp add`) and seed per-host agent-prompt markers; uninstall reverses precisely via host-CLI removal and marker strip.

## [0.1.0]

### Added

- **MCP tool contract**: four tools — `search`, `glob`, `read`, `outline` — over a JSON-RPC stdio server
- **Token counting**: exact o200k with mandatory envelope-floor reservation and per-tool budget resolution
- **Envelope wire format**: append-only, with `text` and `envelope` modes; `text` is the default
- **ChangeSet**: transactional `apply` / `unapply` for Claude Code and Codex config integration, with `AGENTS.md` marker block and receipt
- **Distribution**:
  - npm launcher `mindctx` (zero deps, no postinstall; invokes the platform binary as a subprocess)
  - npm platform packages `@mindctx/{darwin,linux,win32}-{arm64,x64}`
  - crates.io binary `mindctx` (CLI entry) and internal crates `mindctx-core`, `mindctx-mcp`
  - GitHub Releases with per-target binaries and SHA256SUMS
- **Release pipeline**: `xtask release <version> --tag-msg <msg>` (bump versions, self-check, commit, tag) plus CI-driven GitHub Release, npm publish (`next` / `latest`), and crates.io publish via OIDC Trusted Publishing

### Notes

- Pre-release versions (any tag containing `-`) publish under the `next` dist-tag; stable releases publish under `latest`.
- `mindctx-core` and `mindctx-mcp` are internal-unstable and not part of the public API.
