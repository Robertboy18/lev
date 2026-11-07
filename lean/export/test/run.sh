#!/bin/sh
# Compiles the fixture modules, exports them with lev-export and with the toolchain's leanexport,
# and compares the two. Run from lean/export after `lake build`: lake env sh test/run.sh [DIR]
set -eu
test=$(cd "$(dirname "$0")" && pwd)
out=${1:-$(mktemp -d)}
mkdir -p "$out/build"
for m in Mod Mod2 Plain; do
  LEAN_PATH="$out/build" lean -o "$out/build/$m.olean" "$test/$m.lean"
done
LEAN_PATH="$out/build" "$test/../.lake/build/bin/lev-export" --out "$out/lev" --jobs 4 Plain
LEAN_PATH="$out/build" leanexport Plain > "$out/official.ndjson"
python3 -I "$test/../../../scripts/compare_exports.py" "$out/official.ndjson" "$out/lev"
