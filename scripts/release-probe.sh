#!/usr/bin/env bash
# release-probe.sh -- ask both registries which packages already carry a version.
#
# Prints a package-by-package table and writes the same data as JSON to the state file the
# publish steps read. Fails closed: a probe that cannot answer (network, rate limit, auth)
# is reported as unknown and exits non-zero. Reading an unanswerable probe as "not
# published" would republish a version; reading it as "published" would silently skip one.
# Both are the failures this script exists to prevent, so neither is guessed.
#
# Read-only and unauthenticated: safe to run locally before pushing a tag.
#
# The version defaults to the workspace version, which release.yml has already checked
# equals the pushed tag, so the probe and the tag can never disagree.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

STATE_FILE="$REPO_ROOT/release-state.json"
VERSION=""

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --state-file) STATE_FILE="$2"; shift 2 ;;
    -h | --help)
      echo "usage: release-probe.sh [--version VERSION] [--state-file PATH]" >&2
      exit 0
      ;;
    *)
      echo "release-probe.sh: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [ -z "$VERSION" ]; then
  VERSION="$(grep -m1 '^version = ' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
fi
if [ -z "$VERSION" ]; then
  echo "release-probe.sh: could not read the workspace version from Cargo.toml" >&2
  exit 1
fi

CRATES=(mindctx-core mindctx-mcp mindctx)
NPMS=(@mindctx/darwin-arm64 @mindctx/darwin-x64 @mindctx/linux-x64 @mindctx/win32-x64 mindctx)

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Sparse index path: 1/<n>, 2/<n>, 3/<first>/<n>, <c0c1>/<c2c3>/<n> by name length.
index_path() {
  local c="$1" n=${#1}
  if [ "$n" -eq 1 ]; then printf '1/%s' "$c"
  elif [ "$n" -eq 2 ]; then printf '2/%s' "$c"
  elif [ "$n" -eq 3 ]; then printf '3/%s/%s' "${c:0:1}" "$c"
  else printf '%s/%s/%s' "${c:0:2}" "${c:2:2}" "$c"
  fi
}

echo "registry state for $VERSION"

crates_json="{"
unknown=0
for c in "${CRATES[@]}"; do
  body="$TMP/$c.index"
  # Bounded retries: one flaky request must not be reported as unknown, which stops the
  # release. 404 is an answer (crate name not taken), not a transient error, so it is not
  # retried by --retry (that only covers transient failures).
  code="$(curl -s --retry 3 --retry-delay 1 --retry-connrefused --max-time 20 \
    -o "$body" -w '%{http_code}' "https://index.crates.io/$(index_path "$c")")" || code=000
  case "$code" in
    200)
      # Exact comparison, not a regex: the version carries `-alpha.N` / `-rc.N` prerelease
      # tags, whose dots would match any character in an ERE and report a different version
      # (0.1.0-rcA1) as this one -- i.e. skip a crate that was never published.
      if jq -e --arg v "$VERSION" 'select(.vers == $v)' "$body" >/dev/null 2>&1; then
        s=true
      else
        s=false
      fi
      ;;
    404) s=false ;;
    *) s=unknown ;;
  esac
  [ "$s" = unknown ] && unknown=$((unknown + 1))
  printf '  crates.io  %-16s %s\n' "$c" "$s"
  crates_json="$crates_json\"$c\":$s,"
done
crates_json="${crates_json%,}}"

npms_json="{"
for p in "${NPMS[@]}"; do
  err="$TMP/$(printf '%s' "$p" | tr '/@' '__').err"
  if npm view "$p@$VERSION" version --fetch-retries=3 --fetch-retry-maxtimeout=20000 \
    >/dev/null 2>"$err"; then
    s=true
  elif grep -q 'npm error code E404' "$err"; then
    s=false
  else
    s=unknown
  fi
  [ "$s" = unknown ] && unknown=$((unknown + 1))
  printf '  npm        %-28s %s\n' "$p" "$s"
  npms_json="$npms_json\"$p\":$s,"
done
npms_json="${npms_json%,}}"

if [ "$unknown" -gt 0 ]; then
  echo "release-probe.sh: $unknown package probe(s) did not answer; refusing to report a" >&2
  echo "registry state that was not verified. Check connectivity and re-run." >&2
  exit 1
fi

jq -n --arg version "$VERSION" \
  --argjson crates "$crates_json" --argjson npms "$npms_json" \
  '{version: $version, crates: $crates, npms: $npms}' > "$STATE_FILE"

printf '  => %s\n' "$STATE_FILE"
