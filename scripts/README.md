# scripts

Project-local tooling used on every build:

- `activate.sh` — exports `RUSTUP_HOME`/`CARGO_HOME` into `.toolchains/` (project-local, never touches home or shell config) and installs the stable toolchain + clippy + rustfmt on first run. Source it before any cargo invocation.
- `install-smoke.sh` — invoked by the `install-smoke` CI job to validate that the just-published npm platform packages + launcher resolve and launch the mindctx binary.
- `release-probe.sh` — asks crates.io (sparse index) and npm (its E404) which packages already carry the workspace version; prints a table and writes `release-state.json` for the publish steps to skip what exists. Read-only, so it runs locally too. Fails closed: a probe that cannot answer is reported as `unknown` and exits non-zero rather than guessing "published" (silent skip) or "not published" (republish).
- `verify-npm-trust.sh` — reads `release-state.json` and performs the same OIDC token exchange `npm publish` performs, without publishing, so a missing or mismatched npm trusted publisher is named before anything is written to a registry. GitHub Actions only (it needs the OIDC request environment).
