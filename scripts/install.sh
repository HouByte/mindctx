#!/bin/bash
# One-line installer for mindctx (macOS, Linux, WSL).
set -euo pipefail

# The agent prompt template is fetched from the same release tag as the binary (see main), so
# the block written into a host always matches the version being installed.
# MINDCTX_PROMPT_URL overrides that source.

# Markers delimit the chunks this installer owns. Prompt blocks live in markdown files (HTML
# comments); the [core] block lives in TOML and uses comments — HTML comments are not TOML.
PROMPT_BEGIN='<!-- mindctx:begin -->'
PROMPT_END='<!-- mindctx:end -->'
CORE_BEGIN='# mindctx:begin:core'
CORE_END='# mindctx:end:core'

# Release download base. Defaults to GitHub; point MINDCTX_RELEASE_BASE_URL at a mirror
# (e.g. object storage) when GitHub is unreachable. The mirror must preserve the
# /releases/download/vX.Y.Z/{asset,SHA256SUMS} and /releases/latest path layout.
RELEASE_BASE_URL="${MINDCTX_RELEASE_BASE_URL:-https://github.com/HouByte/mindctx}"

detect_platform() {
  local os arch
  os=$(uname -s)
  case "$os" in
    Linux) ;;
    Darwin) ;;
    *) echo "Unsupported OS: $os" >&2; exit 1 ;;
  esac

  arch=$(uname -m)
  case "$arch" in
    x86_64) ;;
    aarch64|arm64) ;;
    *) echo "Unsupported arch: $arch" >&2; exit 1 ;;
  esac

  # On macOS x86_64, check for Rosetta translation
  if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ]; then
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
      arch=aarch64
    fi
  fi

  # Linux binaries are musl-based (static), compatible with glibc distros
  echo "$os $arch"
}

resolve_version() {
  if [ -n "${MINDCTX_VERSION:-}" ]; then
    if ! [[ "$MINDCTX_VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      echo "Invalid MINDCTX_VERSION: $MINDCTX_VERSION (expected vX.Y.Z or X.Y.Z)" >&2
      exit 1
    fi
    echo "${MINDCTX_VERSION#v}"
  else
    local final_url tag version
    final_url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "${RELEASE_BASE_URL}/releases/latest")
    tag=$(basename "$final_url")
    version="${tag#v}"
    echo "$version"
  fi
}

download_and_verify() {
  local version="$1"
  local platform="$2"
  local tmpdir="$3"

  # Parse "os arch" — no subshell, no read from pipe
  local os arch
  # shellcheck disable=SC2086
  set -- $platform
  os="$1"
  arch="$2"

  local asset
  case "$os" in
    Darwin)
      case "$arch" in
        aarch64|arm64) asset="mindctx-aarch64-apple-darwin" ;;
        x86_64)        asset="mindctx-x86_64-apple-darwin" ;;
      esac ;;
    Linux)
      asset="mindctx-x86_64-unknown-linux-musl"
      ;;
  esac

  local base_url="${RELEASE_BASE_URL}/releases/download/v${version}"
  curl -fsSL "$base_url/SHA256SUMS" -o "$tmpdir/SHA256SUMS"
  curl -fsSL "$base_url/$asset" -o "$tmpdir/$asset"

  local expected_hash actual_hash
  # SHA256SUMS format: "HASH  arch_dir/mindctx-binary" — binary always at end after /
  expected_hash=$(grep "/$asset$" "$tmpdir/SHA256SUMS" | cut -d' ' -f1)
  if [ -z "$expected_hash" ]; then
    echo "Asset $asset not found in SHA256SUMS" >&2; exit 1
  fi

  actual_hash=$(shasum -a 256 "$tmpdir/$asset" | cut -d' ' -f1)
  if [ "$actual_hash" != "$expected_hash" ]; then
    echo "SHA256 mismatch for $asset:" >&2
    echo "  expected: $expected_hash" >&2
    echo "  actual:   $actual_hash" >&2
    exit 1
  fi

  echo "$asset"
}

install_binary() {
  local asset="$1"
  local tmpdir="$2"
  local dest="$HOME/.local/bin/mindctx"

  mkdir -p "$HOME/.local/bin"
  install -m 0755 "$tmpdir/$asset" "$dest"
  echo "Installed mindctx to $dest"
}

check_path() {
  local dest="$HOME/.local/bin"
  local found=0
  case ":$PATH:" in
    *:"$dest":*) found=1 ;;
  esac
  if [ "$found" -eq 0 ]; then
    echo ""
    echo "NOTE: $dest is not in your PATH."
    echo "Add it with: export PATH=\"\$HOME/.local/bin:\$PATH\""
    echo "(Add this line to your ~/.bashrc or ~/.zshrc to persist it.)"
  fi
}

register_hosts() {
  local HAS_CLAUDE=0 HAS_CODEX=0

  command -v claude >/dev/null 2>&1 && HAS_CLAUDE=1
  command -v codex  >/dev/null 2>&1 && HAS_CODEX=1

  if [[ $HAS_CLAUDE -eq 0 && $HAS_CODEX -eq 0 ]]; then
    echo "No coding tools detected on PATH, skipping MCP registration."
    return 0
  fi

  echo "Registering mindctx as MCP server in detected coding tools..."

  if [[ $HAS_CLAUDE -eq 1 ]]; then
    echo "Registering mindctx as MCP server in Claude Code (user scope)..."
    if claude mcp add --scope user --transport stdio mindctx -- mindctx serve; then
      echo "  ok: Claude Code MCP registered"
    else
      echo "  warning: claude mcp add failed (exit $?) — register manually later" >&2
    fi
  fi

  if [[ $HAS_CODEX -eq 1 ]]; then
    echo "Registering mindctx as MCP server in Codex..."
    if codex mcp add mindctx -- mindctx serve; then
      echo "  ok: Codex MCP registered"
    else
      echo "  warning: codex mcp add failed (exit $?) — register manually later" >&2
    fi
  fi
}

# Fetch the agent prompt template. A missing template is fatal: a host must never be left
# with a half-written guidance block.
download_prompt_template() {
  local url="$1" dest="$2"

  if ! curl -fsSL "$url" -o "$dest"; then
    echo "Failed to download the agent prompt template: $url" >&2
    echo "Retry when the network is back, or set MINDCTX_PROMPT_URL to another source." >&2
    exit 1
  fi
  if ! grep -qxF "$PROMPT_BEGIN" "$dest" || ! grep -qxF "$PROMPT_END" "$dest"; then
    echo "Agent prompt template is missing its own marker lines: $url" >&2
    exit 1
  fi
}

# Strip every inclusive begin..end block, in place. Markers are matched as whole lines, so
# only blocks this installer wrote are removed. Returns non-zero when a begin marker is left
# behind (unterminated or nested): callers must not append on top of that state, or a later
# strip would span user content.
strip_marked_block() {
  local target="$1" begin="$2" end="$3"
  local tmp="$target.mindctx-tmp" guard=0

  [ -f "$target" ] || return 0

  while grep -qxF "$begin" "$target" && [ "$guard" -lt 20 ]; do
    if ! grep -qxF "$end" "$target"; then
      break
    fi
    awk -v begin="$begin" -v end="$end" '
      $0 == begin { skip = 1; next }
      skip { if ($0 == end) skip = 0; next }
      { print }
    ' "$target" > "$tmp"
    mv "$tmp" "$target"
    guard=$((guard + 1))
  done

  if grep -qxF "$begin" "$target"; then
    return 1
  fi
  return 0
}

# Drop trailing blank lines, so re-installing cannot accumulate blank-line drift.
trim_trailing_blank_lines() {
  local target="$1"
  local tmp="$target.mindctx-tmp"

  awk '
    { line[NR] = $0 }
    END {
      last = NR
      while (last > 0 && line[last] == "") last--
      for (i = 1; i <= last; i++) print line[i]
    }
  ' "$target" > "$tmp"
  mv "$tmp" "$target"
}

# Idempotent upsert of the agent prompt block: replace the previous block in place, append
# when there is none, create the file when the host has none yet.
upsert_prompt_block() {
  local target="$1" tpl="$2"

  mkdir -p "$(dirname "$target")"
  if [ -f "$target" ]; then
    if ! strip_marked_block "$target" "$PROMPT_BEGIN" "$PROMPT_END"; then
      echo "warning: $target has an unterminated $PROMPT_BEGIN block — left as-is, block not written" >&2
      return 0
    fi
    trim_trailing_blank_lines "$target"
  else
    : > "$target"
  fi

  if [ -s "$target" ]; then
    printf '\n' >> "$target"
  fi
  cat "$tpl" >> "$target"
  echo "  ok: agent prompt block written to $target"
}

# Idempotent upsert of the managed [core] section in ~/.mindctx/config.toml. Everything
# outside the marker block — user sections and comments — is preserved.
write_core_config() {
  local config="$HOME/.mindctx/config.toml" legacy

  mkdir -p "$(dirname "$config")"
  if [ -f "$config" ]; then
    if ! strip_marked_block "$config" "$CORE_BEGIN" "$CORE_END"; then
      echo "warning: $config has an unterminated $CORE_BEGIN block — left as-is, block not written" >&2
      return 0
    fi
    trim_trailing_blank_lines "$config"
    # A bare, unmarked [core] can only come from `mindctx apply`, which wrote exactly that
    # and nothing else. Drop it so the file keeps one [core] table — the block below carries
    # it. A [core] holding keys is the user's; leave the file alone instead of emitting a
    # duplicate table, which is invalid TOML.
    if grep -qxF '[core]' "$config"; then
      legacy=$(grep -v '^[[:space:]]*$' "$config" | grep -v '^[[:space:]]*#' || true)
      if [ "$legacy" = '[core]' ]; then
        : > "$config"
      else
        echo "warning: $config already has an unmanaged [core] section — left as-is, block not written" >&2
        return 0
      fi
    fi
  else
    : > "$config"
  fi

  if [ -s "$config" ]; then
    printf '\n' >> "$config"
  fi
  {
    printf '%s\n' "$CORE_BEGIN"
    printf '[core]\n'
    printf '# managed by installer\n'
    printf '%s\n' "$CORE_END"
  } >> "$config"
  chmod 0644 "$config"
  echo "  ok: [core] block written to $config"
}

main() {
  # Refuse sudo (mirror Claude Code installer)
  if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
    echo "Do not run this script with sudo." >&2
    exit 1
  fi

  local platform version asset
  tmpdir=$(mktemp -d)
  trap 'rm -rf "$tmpdir"' EXIT

  platform=$(detect_platform)
  version=$(resolve_version)
  asset=$(download_and_verify "$version" "$platform" "$tmpdir")
  install_binary "$asset" "$tmpdir"
  check_path
  register_hosts

  # Host configuration init, formerly `mindctx apply`'s job: the guidance block for each
  # detected host plus the shared runtime config. The template is pinned to the version being
  # installed, so it is resolved after resolve_version has run.
  local prompt_url="${MINDCTX_PROMPT_URL:-https://raw.githubusercontent.com/HouByte/mindctx/v${version}/scripts/agent-prompt.md}"
  download_prompt_template "$prompt_url" "$tmpdir/agent-prompt.md"
  write_core_config
  if command -v claude >/dev/null 2>&1; then
    upsert_prompt_block "$HOME/.claude/CLAUDE.md" "$tmpdir/agent-prompt.md"
  fi
  if command -v codex >/dev/null 2>&1; then
    upsert_prompt_block "$HOME/.codex/AGENTS.md" "$tmpdir/agent-prompt.md"
  fi

  rm -rf "$tmpdir"
}

main
