#!/usr/bin/env sh
set -eu

# End-to-end check of lev against real Lean toolchains, used by CI and before
# releases. The caller supplies the toolchains; keeping that list outside the
# script lets it exercise future releases, RCs, nightlies, and custom elan
# identifiers.
#
# Inputs:
#   LEV_MATRIX_VERSIONS Required whitespace-separated toolchain selectors
#   LEV_BIN             lev executable, defaulting to target/debug/lev
#   LEV_MATRIX_ROOT     Optional workspace and cache root
#   LEV_MATRIX_KEEP=1   Keep the generated projects for inspection

LEV_BIN=${LEV_BIN:-}
if [ -z "$LEV_BIN" ]; then
  LEV_BIN=$(pwd)/target/debug/lev
fi

if [ -z "${LEV_MATRIX_VERSIONS:-}" ]; then
  printf '%s\n' \
    'LEV_MATRIX_VERSIONS is required; pass any releases, RCs, nightlies, or complete elan identifiers' \
    >&2
  exit 2
fi
VERSIONS=$LEV_MATRIX_VERSIONS
ROOT=${LEV_MATRIX_ROOT:-}
KEEP=${LEV_MATRIX_KEEP:-0}

if [ -z "$ROOT" ]; then
  ROOT=$(mktemp -d "${TMPDIR:-/tmp}/lev-lean-matrix.XXXXXX")
fi

cleanup() {
  if [ "$KEEP" != "1" ]; then
    rm -rf "$ROOT"
  else
    printf 'matrix workspace retained at %s\n' "$ROOT"
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "$ROOT/cache"
LEV_CACHE_DIR="$ROOT/cache"
export LEV_CACHE_DIR

# Serial execution keeps failures readable.
index=0
for version in $VERSIONS; do
  index=$((index + 1))
  # Selectors may be complete elan identifiers containing slashes. Keep them
  # out of filesystem paths so every valid identifier exercises the same flow.
  project="$ROOT/project-$index"
  printf '\n==> Lean %s\n' "$version"
  mkdir -p "$project"
  (cd "$project" && elan run --install "$version" lake init Smoke lib)
  printf '%s\n' "$version" > "$project/lean-toolchain"

  "$LEV_BIN" --project "$project" doctor
  "$LEV_BIN" --project "$project" inspect build --json > "$ROOT/first-$index.json"

  # One edit must come back as that module's own source change.
  printf '\ntheorem smoke_edit : True := trivial\n' >> "$project/Smoke/Basic.lean"
  "$LEV_BIN" --project "$project" inspect build --why Smoke > "$ROOT/second-$index.txt"
  cat "$ROOT/second-$index.txt"
  if ! grep -q '^edited: Smoke.Basic$' "$ROOT/second-$index.txt"; then
    printf 'lev did not report Smoke.Basic as edited under %s\n' "$version" >&2
    exit 1
  fi
done

# The registered projects make both cache commands read real Lake caches.
"$LEV_BIN" cache status
"$LEV_BIN" cache gc --lake
printf '\nLean compatibility matrix passed: %s\n' "$VERSIONS"
