# scripts

Project-local tooling used on every build:

- `activate.sh` — exports `RUSTUP_HOME`/`CARGO_HOME` into `.toolchains/` (project-local, never touches home or shell config) and installs the stable toolchain + clippy + rustfmt on first run. Source it before any cargo invocation.
- `install-smoke.sh` — invoked by the `install-smoke` CI job to validate that the just-published npm platform packages + launcher resolve and launch the mindctx binary.
