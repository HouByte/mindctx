# Contributing

Everything here is for people building or releasing mindctx. For the tool itself, see
[README.md](README.md). For engineering discipline and language policy, see
[AGENTS.md](AGENTS.md).

## Environment

The Rust toolchain is project-local: it lives in `.toolchains/` (`RUSTUP_HOME`/`CARGO_HOME`
redirected), never enters `PATH`, never touches your home or shell config.

```bash
source scripts/activate.sh   # auto-installs the toolchain into .toolchains/ if missing
```

Delete `.toolchains/` to uninstall — zero residue.

## Build, test, lint

```bash
source scripts/activate.sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # mirrors CI
cargo deny check
```

## Debug MCP end-to-end

```bash
npx @modelcontextprotocol/inspector cargo run -p mindctx -- serve
```

This opens the MCP inspector against the local `mindctx serve`, so you can drive the four
tools over JSON-RPC interactively.

## Release

Releases are tag-driven: pushing a `v*` tag triggers `.github/workflows/release.yml`.

The `xtask` crate automates the pipeline:

```bash
cargo run -p xtask -- dist              # build + stage platform binaries + SHA256SUMS
cargo run -p xtask -- npm --version-sync  # sync npm package versions + stage platform bins
cargo run -p xtask -- verify-publish    # pre-publish manifest/order checks (dry-run)
cargo run -p xtask -- preflight         # crates.io / npm name-availability check (fails closed)
cargo run -p xtask -- about             # generate third-party-notices.html
```

Publishing uses OIDC Trusted Publishing (no long-lived secrets in GitHub). The npm packages
and crates.io crates are each configured with a Trusted Publisher for `release.yml` +
environment `release`.

## Conventions

- Conventional Commits, English commit messages.
- One logical change per commit; the tree must stay green (`fmt` + `clippy -D warnings` + `test`).
- Language policy and engineering discipline are in `AGENTS.md` — follow it exactly.

## License

By contributing, you agree that your contributions will be dual-licensed under
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), the same terms as the rest of the
project. No Contributor License Agreement (CLA) is required.
