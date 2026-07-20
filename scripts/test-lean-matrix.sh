#!/usr/bin/env sh
set -eu

# End-to-end compatibility harness used by CI and local release checks. The
# caller supplies selectors; keeping that list outside the script lets this
# exercise future releases, RCs, nightlies, and custom elan identifiers.
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

# Serial execution keeps failures readable and cache reuse deterministic.
index=0
for version in $VERSIONS; do
  index=$((index + 1))
  # Selectors may be complete elan identifiers containing slashes. Keep them
  # out of filesystem paths so every valid identifier exercises the same flow.
  project="$ROOT/project-$index"
  printf '\n==> Lean %s\n' "$version"
  LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" init "$project" --lean "$version" --template lib
  LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" --project "$project" sync
  # The offline build proves that sync left a complete local environment.
  LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" --project "$project" build --offline
  LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" --project "$project" deps --json >/dev/null
  LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" --project "$project" doctor
done

# One final scan catches cross-version cache layout or ownership mistakes.
LEV_CACHE_DIR="$ROOT/cache" "$LEV_BIN" cache verify
printf '\nLean compatibility matrix passed: %s\n' "$VERSIONS"
