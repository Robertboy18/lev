# Contributing

## Development

Use a disposable target directory when the checkout is on slow or remote
storage:

```bash
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/lev-target}"
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```

Run lev against real Lean toolchains with:

```bash
LEV_MATRIX_VERSIONS="stable nightly" \
  LEV_BIN="$CARGO_TARGET_DIR/debug/lev" \
  scripts/test-lean-matrix.sh
```

`LEV_MATRIX_VERSIONS` accepts any whitespace-separated releases, RCs,
nightlies, or complete elan toolchain identifiers. The script has no built-in
release list. For each toolchain it creates a small library with `lake init`,
explains a first build and a rebuild after one edit, and runs `lev doctor`;
it finishes with the cache commands.

## Layout

```text
src/
|-- app/      One module per command: inspect_build, check, cache, doctor;
|             exporter builds and runs lev-export for check
|-- cache/    lev's project registry, the check caches, Lake's artifact cache
|-- check/    The checking engine: export reader, hashes, slices, slice
|             writer, kernels, verdict cache, linker
|-- cli/      Side-effect-free clap argument schema
|-- core/     Small filesystem, process, and output helpers
`-- project/  Project discovery, Lake manifests, traces, and build output
lean/export/  lev-export, the per-module exporter `lev check` builds
```

Keep command parsing in `cli/`, command behavior in `app/`, and file formats
in `cache/` or `project/`. `core/` must not depend on the other modules.
`check/` is the library behind `lev check` (`lev::check::run`); it knows
nothing about elan, Lake or lev's cache layout.

`lean/export` is a Lean package. lev embeds its `lakefile.toml` and
`LevExport.lean`, and an edit to either changes the key of the exporter
build, so the next `lev check` builds it again. To compare its output with
the toolchain's `leanexport`, run `lake build` there and then
`lake env sh test/run.sh DIR`.

lev runs Lean and Lake through elan the way `lake build` would run in the
project, and it never changes Lake's environment. Every `--json` command goes
through `core::json_output::print`. Add its schema identifier to
`schemas/cli-envelope-v1.schema.json` and cover it with a CLI test.

## Changes

- lev itself does not write to a project or to Lake's cache. Outside lev's
  own cache only `lev cache gc --lake --apply` deletes, and only what it
  listed.
- Read Lake's files by the captions and layouts Lake writes. Report what lev
  cannot read instead of guessing.
- Add process-level tests for user-visible behavior. Keep the shared fixture
  in `tests/cli.rs` and the scenarios in `tests/cli/`. Tests must not open
  network connections or listen on ports. `tests/check.rs` runs `lev check`
  on a tiny Lake project with the real toolchain and skips itself when elan,
  that toolchain or `bwrap` is missing.
- Add focused unit tests for parsing and collection decisions.
- Run `scripts/test-lean-matrix.sh` after changing how lev reads Lake's
  output, traces, or cache.
