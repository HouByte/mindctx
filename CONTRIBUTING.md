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

### Trusted Publishing setup

Publishing uses OIDC Trusted Publishing — no long-lived credentials live in GitHub secrets.
Trust relationships are configured per package on the registry side, and are what the
workflow's OIDC token is exchanged against:

| Registry | Packages | Trust entry |
|---|---|---|
| crates.io | `mindctx-core`, `mindctx-mcp`, `mindctx` | trusted publisher naming this repository + `release.yml` |
| npm | `mindctx`, `@mindctx/darwin-arm64`, `@mindctx/darwin-x64`, `@mindctx/linux-x64`, `@mindctx/win32-x64` | trusted publisher naming repository `HouByte/mindctx`, workflow `release.yml`, environment `release` |

`packages/mindctx-linux-arm64/` is not published yet — the build matrix has no
`aarch64-unknown-linux-*` target, so it carries no binary and is absent from the launcher's
`optionalDependencies`.

A package with no trust entry falls back to credential lookup and fails `ENEEDAUTH`. npm
exchanges a token per package, so the workflow checks all five before publishing anything
and reports the ones that are missing as a named list. crates.io has no equivalent check:
its token exchange is repository-scoped (it succeeds if *any* crate is configured) and each
crate is authorised only when it is published, so a crate whose entry is missing surfaces at
publish time — after which the idempotent probe lets a re-push finish the rest. Check the
configured npm entries with `npm trust list <package>`.

### Check before pushing a tag

Trusted publishing is configured on the registry side, so a tag push is not the place to
find out whether it is set up. `release.yml` also runs on `workflow_dispatch`, which
performs the same checks and publishes nothing — run it first and fix anything it reports,
then push the tag once:

```bash
gh workflow run release.yml            # probe + trusted-publisher checks + publish dry-runs
gh run watch                           # follow it; nothing is published by this run
```

It must be this workflow: the trusted-publisher claims name the workflow file, so a check
running anywhere else would not be testing what the release actually does.

`scripts/release-probe.sh` does the registry half of that check locally — it is read-only
and needs no credentials, so it is the fastest way to see what is already published:

```bash
bash scripts/release-probe.sh          # prints the table, writes release-state.json
```

### Idempotency

Both registries are probed for the version before publishing, and each package is skipped
when it is already there. A release that stops halfway — one registry failing, one package
refused — is finished by re-pushing the same tag: the packages that landed are skipped and
only the missing ones publish. The state reaches the publish steps through
`release-state.json` rather than through step outputs, because a mistyped output silently
disables the gate reading it.

## Conventions

- Conventional Commits, English commit messages.
- One logical change per commit; the tree must stay green (`fmt` + `clippy -D warnings` + `test`).
- Language policy and engineering discipline are in `AGENTS.md` — follow it exactly.

## License

By contributing, you agree that your contributions will be dual-licensed under
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), the same terms as the rest of the
project. No Contributor License Agreement (CLA) is required.
