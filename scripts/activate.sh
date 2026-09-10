#!/bin/sh
# mindctx project-local Rust toolchain: installed under this repo's .toolchains/ (RUSTUP_HOME/CARGO_HOME redirected),
# not on PATH, never touches home or shell config; .toolchains/ is gitignored.
#
# Usage: source scripts/activate.sh

_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export RUSTUP_HOME="$_ROOT/.toolchains/rustup"
export CARGO_HOME="$_ROOT/.toolchains/cargo"
export PATH="$CARGO_HOME/bin:$PATH"

if [ ! -x "$CARGO_HOME/bin/cargo" ]; then
  echo "[mindctx] project toolchain missing, installing into .toolchains/ (user directories untouched)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    RUSTUP_HOME="$RUSTUP_HOME" CARGO_HOME="$CARGO_HOME" sh -s -- \
      -y --profile minimal --default-toolchain stable --no-modify-path
  "$CARGO_HOME/bin/rustup" component add clippy rustfmt
fi

command -v cargo >/dev/null 2>&1 && echo "[mindctx] $(cargo --version)"
