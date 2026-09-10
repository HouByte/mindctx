#!/usr/bin/env bash
# install-smoke.sh — local dry-run of the CI install-smoke job.
#
# Sequence mirrors the CI job: build release binary -> stage dist -> verdaccio
# publish -> npm install -> version / status / MCP handshake / missing-pkg
# error asserts. Differences from CI: the platform package staged matches the
# HOST os/arch (CI is always linux-x64), staging happens in a temp dir so the
# tree stays clean, and the install goes into a throwaway prefix so a
# maintainer's existing global mindctx is never clobbered.
#
# Verdaccio availability: reuses a server already on :4873, otherwise tries
# Docker. If neither works, prints a clear message to stderr and exits 0 —
# this script is a dry-run convenience, not a gate.

set -euo pipefail

REGISTRY="${REGISTRY:-http://localhost:4873}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# ---- Host -> platform package mapping (mirrors packages/ + launcher.js) ----
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) PKG=darwin-arm64 ;;
  Darwin/x86_64) PKG=darwin-x64 ;;
  Linux/x86_64) PKG=linux-x64 ;;
  Linux/aarch64 | Linux/arm64) PKG=linux-arm64 ;;
  MINGW* | MSYS* | CYGWIN*) PKG=win32-x64 ;;
  *)
    echo "[install-smoke] FAIL: unsupported host platform '$(uname -s)/$(uname -m)'" >&2
    exit 1
    ;;
esac
BIN_EXT=""
if [ "$PKG" = "win32-x64" ]; then
  BIN_EXT=".exe"
fi
echo "[install-smoke] host platform package: @mindctx/$PKG"

STAGE="$(mktemp -d)"
CONTAINER=""
cleanup() {
  if [ -n "$CONTAINER" ]; then
    docker stop "$CONTAINER" > /dev/null 2>&1 || true
  fi
  rm -rf "$STAGE" 2> /dev/null || true
}
trap cleanup EXIT

# ---- Verdaccio bootstrap (before the build — skip fast when unavailable) ----
start_verdaccio() {
  if curl -s --fail "$REGISTRY/-/ping" > /dev/null 2>&1; then
    echo "[install-smoke] reusing verdaccio already running at $REGISTRY"
    return 0
  fi

  if ! command -v docker > /dev/null 2>&1; then
    echo "[install-smoke] SKIP: verdaccio unavailable." >&2
    echo "[install-smoke] No server on $REGISTRY and docker is not installed." >&2
    echo "[install-smoke] To run this dry run, start verdaccio first, e.g.:" >&2
    echo "  docker run -d --name verdaccio -p 4873:4873 verdaccio/verdaccio" >&2
    return 1
  fi

  echo "[install-smoke] starting verdaccio container ..."
  CONTAINER="verdaccio-smoke-$$"
  docker run -d --name "$CONTAINER" --rm \
    -p 4873:4873 \
    -v "verdaccio-smoke-$$-storage:/verdaccio/storage" \
    verdaccio/verdaccio > /dev/null

  for _ in $(seq 1 30); do
    if curl -s --fail "$REGISTRY/-/ping" > /dev/null 2>&1; then
      echo "[install-smoke] verdaccio ready"
      return 0
    fi
    sleep 1
  done

  echo "[install-smoke] SKIP: verdaccio container did not become ready in 30s." >&2
  return 1
}

if ! start_verdaccio; then
  echo "[install-smoke] skipping dry run (exit 0)" >&2
  exit 0
fi

# ---- Build the real release binary (makes the asserts real, not dummies) ----
echo "[install-smoke] building mindctx release binary ..."
# shellcheck disable=SC1091
source "$REPO_ROOT/scripts/activate.sh"
cargo build --release -p mindctx

VERSION=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
echo "[install-smoke] workspace version: $VERSION"

# ---- Stage dist packages ----
echo "[install-smoke] staging dist packages ..."

PLAT_DIR="$STAGE/@mindctx/$PKG"
mkdir -p "$PLAT_DIR/bin"
cp "target/release/mindctx$BIN_EXT" "$PLAT_DIR/bin/mindctx$BIN_EXT"
chmod +x "$PLAT_DIR/bin/mindctx$BIN_EXT"
cat > "$PLAT_DIR/package.json" << EOF
{
  "name": "@mindctx/$PKG",
  "version": "$VERSION",
  "description": "mindctx binary for $PKG (installed automatically by the mindctx launcher; do not install directly)",
  "license": "MIT OR Apache-2.0",
  "os": ["${PKG%%-*}"],
  "cpu": ["${PKG#*-}"],
  "files": ["bin/"]
}
EOF

mkdir -p "$STAGE/mindctx"
cp packages/mindctx/launcher.js "$STAGE/mindctx/"
cat > "$STAGE/mindctx/package.json" << EOF
{
  "name": "mindctx",
  "version": "$VERSION",
  "bin": { "mindctx": "launcher.js" },
  "engines": { "node": ">=18" },
  "optionalDependencies": {
    "@mindctx/win32-x64": "$VERSION",
    "@mindctx/linux-x64": "$VERSION",
    "@mindctx/linux-arm64": "$VERSION",
    "@mindctx/darwin-x64": "$VERSION",
    "@mindctx/darwin-arm64": "$VERSION"
  },
  "files": ["launcher.js"],
  "publishConfig": { "access": "public" }
}
EOF

# ---- Publish to verdaccio ----
echo "[install-smoke] publishing packages to verdaccio ..."
CRED='{"name":"smoke","password":"smoke1234","email":"smoke@test.local","type":"user","roles":[]}'
TOKEN=$(curl -s -X PUT "$REGISTRY/-/user/org.couchdb.user:smoke" \
  -H 'Content-Type: application/json' \
  -d "$CRED" \
  | node -pe 'JSON.parse(require("fs").readFileSync(0, "utf8")).token')
if [ -z "$TOKEN" ]; then
  echo "[install-smoke] FAIL: verdaccio did not return an auth token" >&2
  exit 1
fi
echo "//localhost:4873/:_authToken=$TOKEN" >> ~/.npmrc

npm publish "$PLAT_DIR" --registry "$REGISTRY" --access public
echo "[install-smoke]   published @mindctx/$PKG"
npm publish "$STAGE/mindctx" --registry "$REGISTRY" --access public
echo "[install-smoke]   published mindctx"

# ---- Install (throwaway prefix — never touches the global install) ----
echo "[install-smoke] installing mindctx from verdaccio ..."
PREFIX="$STAGE/prefix"
npm install -g --prefix "$PREFIX" mindctx --registry "$REGISTRY" --no-fund --no-audit
if [ "$PKG" = "win32-x64" ]; then
  MINDCTX="$PREFIX/mindctx.cmd"
else
  MINDCTX="$PREFIX/bin/mindctx"
fi

echo "[install-smoke] running smoke assertions ..."

# 1. --version prints the workspace version
VERSION_OUTPUT=$("$MINDCTX" --version)
echo "[install-smoke]   version output: $VERSION_OUTPUT"
if echo "$VERSION_OUTPUT" | grep -q "$VERSION"; then
  echo "[install-smoke]   PASS: --version matches workspace version $VERSION"
else
  echo "[install-smoke] FAIL: --version did not contain $VERSION" >&2
  exit 1
fi

# 2. status exits 0
"$MINDCTX" status
echo "[install-smoke]   PASS: status exits 0"

# 3. serve answers an MCP initialize handshake (serverInfo.name = mindctx)
MCP_RESPONSE=$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-test","version":"0.0.1"}}}' \
  | "$MINDCTX" serve 2> /dev/null \
  | head -1 || true)
if echo "$MCP_RESPONSE" | grep -q '"mindctx"'; then
  echo "[install-smoke]   PASS: MCP handshake response contains 'mindctx'"
else
  echo "[install-smoke] FAIL: MCP handshake response missing 'mindctx': $MCP_RESPONSE" >&2
  exit 1
fi

# 4. missing platform package: launcher error text matches the frozen contract byte-exact.
#    --omit=optional skips @mindctx/*, so the launcher must fall into the
#    not-installed branch when invoked.
SMOKE_DIR="$STAGE/missing-pkg"
mkdir -p "$SMOKE_DIR"
(cd "$SMOKE_DIR" && npm install mindctx --omit=optional --registry "$REGISTRY" --no-fund --no-audit --loglevel=error)
ACTUAL=$("$SMOKE_DIR/node_modules/.bin/mindctx" --version 2>&1 > /dev/null || true)
EXPECTED="mindctx: platform package @mindctx/$PKG was not installed (registry mirror may lag). Retry with the official registry: npm config set registry https://registry.npmjs.org/"
if [ "$ACTUAL" = "$EXPECTED" ]; then
  echo "[install-smoke]   PASS: missing-platform-package error matches the frozen contract exactly"
else
  echo "[install-smoke] FAIL: missing-platform-package error text drifted from the frozen contract" >&2
  echo "[install-smoke] expected: $EXPECTED" >&2
  echo "[install-smoke] actual:   $ACTUAL" >&2
  exit 1
fi

echo "[install-smoke] ALL SMOKE TESTS PASSED"
