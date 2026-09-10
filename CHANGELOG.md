# Changelog

All notable changes to mindctx are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
