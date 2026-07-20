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

Run real Lean compatibility checks with:

```bash
LEV_MATRIX_VERSIONS="stable nightly" \
  LEV_BIN="$CARGO_TARGET_DIR/debug/lev" \
  scripts/test-lean-matrix.sh
```

`LEV_MATRIX_VERSIONS` accepts any whitespace-separated releases, RCs,
nightlies, or complete elan toolchain identifiers. The harness intentionally
has no built-in release list.

## Layout

```text
src/
|-- app/         CLI orchestration and command policy
|-- cache/       Local, remote, and Lake artifact caches
|-- cli/         Side-effect-free clap argument schema
|-- core/        Policy-free filesystem, transport, and platform primitives
|-- dependency/  General package acquisition, graphs, and resolution reuse
|-- project/     Standard Lean/Lake files and project-owned state
`-- toolchain/   Lean installation, storage, and distribution
```

Keep command parsing in `cli/`, command behavior in `app/`, and file-format or
storage logic in the matching domain module. `core/` is for small shared
primitives and must not depend on the higher-level modules.

Use `core::atomic_file` for complete-file publication and `app::transaction`
when one operation may change several project files. Toolchain archive and
manifest compatibility belongs under `toolchain/store/`.

Every `--json` command goes through `core::json_output::print`. Add its schema
identifier to the checked schema file and cover it with a CLI test.

## Changes

- Preserve standard Lean and Lake project files.
- Do not reset dirty dependency checkouts.
- Make filesystem mutations atomic or transactionally recoverable.
- Never let an explicit backend silently fall back to a different trust model.
- Add process-level tests for user-visible command behavior.
- Keep shared CLI fixtures in `tests/cli.rs`; put scenarios in `tests/cli/`.
- Add focused unit tests for parsing, policy decisions, and rollback.
- Add a real Lean smoke test when changing toolchain or Lake integration.
