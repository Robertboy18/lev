---
name: lev
description: Use lev to initialize, synchronize, lock, build, test, run, inspect, and troubleshoot Lean 4 and Lake projects. Use when working with lean-toolchain, lakefile.toml, lakefile.lean, lake-manifest.json, lev.lock, or lev.toml; when changing Lean toolchains or dependencies; or when a task would otherwise call lean, lake, or elan directly.
---

# Use lev in Lean Projects

Use lev for project and environment operations. Keep Lean, Lake, and the
project's own files authoritative: lev prepares the environment, while Lake
still defines and runs the build.

## Start From The Repository

1. Find the nearest directory containing `lean-toolchain` and a Lakefile. Work
   there or pass `--project PATH`.
2. Read the repository instructions, README, `lean-toolchain`, Lakefile, and
   `lev.toml` before changing the environment.
3. Inspect the worktree before editing. Preserve unrelated user changes.
4. Run `lev --version` and `lev doctor` if the host setup is uncertain.

Do not require `lev.toml`; ordinary Lake projects work without it. Do not
change `lean-toolchain` unless the task calls for a project-wide upgrade.

## Build And Check

Use the narrowest command that proves the current change:

```console
lev check Path/To/File.lean
lev build Target
lev test
lev lint
lev run lake COMMAND
```

These commands synchronize first unless `--no-sync` is present. Do not run a
separate `lev sync` before every build. Pass Lake targets and options through
the corresponding lev command.

Use `lev --local build` when the checkout is on slow shared storage and the
lev cache points to suitable local scratch.

## Keep Environments Reproducible

Use committed manifests and locks as the repository expects:

```console
lev lock --check
lev sync --locked
lev sync --frozen
lev build --offline
```

- Use `--locked` when `lake-manifest.json` must already exist.
- Use `--frozen` to reject configuration or lock drift.
- Use `--offline` only when every required toolchain and dependency should
  already be local.
- Do not update locks or dependencies during an unrelated source change.
- Never edit `lake-manifest.json` by hand.

Select an alternate locked environment without rewriting `lean-toolchain`:

```console
lev lock --lean "$TOOLCHAIN"
lev build --lean "$TOOLCHAIN"
lev test --lean "$TOOLCHAIN"
```

Accept any user-supplied release, nightly, channel, or complete toolchain
identifier. Do not replace it with a hardcoded version. Use `lev use`,
`lev pin`, or `lev upgrade --lean` only when the user intends to change
project state.

## Manage Dependencies

For a declarative `lakefile.toml`, use lev's transactional commands:

```console
lev add PACKAGE --scope OWNER
lev add PACKAGE --git URL --rev REVISION
lev add PACKAGE --path ../relative/path
lev remove PACKAGE
lev update PACKAGE
lev outdated
lev upgrade PACKAGE --dry-run
```

Preserve the package source and revision requested by the user or repository.
Do not infer a registry owner or revision from a package name. Inspect the
Lakefile, manifest, and lock diff after a dependency change.

Do not mechanically rewrite executable `lakefile.lean` files. Do not assume
Mathlib is installed. Run package-specific helpers such as
`lev run lake exe cache get` only when that package's documentation calls for
them.

## Write Maintainable Lean

Follow repository conventions first, then current Lean and Mathlib conventions.
Use any dedicated Lean or Mathlib skill supplied by the repository alongside
this one; lev governs the environment, not theorem design.

- Search nearby code and the pinned dependencies before inventing a definition
  or theorem.
- Verify declaration names in the active environment. Do not guess Mathlib
  APIs from memory.
- Preserve theorem statements, names, namespaces, and public APIs unless the
  task explicitly changes them.
- Prefer the narrow imports used by nearby files. Do not add `import Mathlib`
  merely to make elaboration succeed.
- Check difficult proof steps against the compiler as they are written. Finish
  with the relevant `lev check`, `lev build`, `lev test`, or `lev lint`.
- Leave no `sorry`, `admit`, exploratory `#check` or `#print`, trace setting,
  suggestion tactic, or unapproved axiom in finished work.
- Keep comments scarce and specific. Explain a mathematical idea or a
  non-obvious design decision; do not narrate tactics or the editing process.
- Write docstrings as library documentation, not as a transcript of how the
  code was generated.

## Write Like A Maintainer

When changing README files, guides, examples, or release notes, keep the
technical details exact without flattening the writing into a feature list.

- Start with the situation a Lean developer recognizes, then introduce the
  command that helps.
- Explain why a workflow exists, not only what each flag does.
- Use verified project stories, failed attempts, and measured results when
  they teach something. Never invent an anecdote, benchmark, or motivation.
- Vary sentence rhythm and section openings. Avoid repeating templates such as
  "Use X to Y" or "This command provides."
- First person is fine for genuine project rationale; conversational phrasing
  is fine when it makes the explanation easier to follow.
- Keep reference tables, safety guarantees, exit behavior, and command examples
  direct. Personality must not blur a contract.
- Read the prose aloud once. Remove filler, inflated claims, fake excitement,
  and phrases that could describe any developer tool.

## Minimize Imports

Preview compiler-derived changes before applying them:

```console
lev imports MyProject.Module --explain
lev imports MyProject.Module --apply
```

`lev imports` aliases `lev shake`. A dry run exits with status 1 when it finds
changes. Applying changes snapshots the affected sources, rebuilds them, and
restores the originals if verification fails.

For a reusable library, preserve public and implied imports on the first pass:

```console
lev imports --keep-public --keep-implied --explain
lev imports --keep-public --keep-implied --apply
```

The replacement may include private syntax, tactic, instance, or elaborator
modules that are not visible in the theorem text. Keep Lake's complete result.
Preserve an intentionally untraced dependency with:

```lean
import Some.Module -- shake: keep
```

Use `--isolate` for a large batch so independently verified edits can survive
a failing subset. Use `--check --coverage` in CI. Avoid `--force` unless stale
compiled metadata is explicitly acceptable.

## Inspect And Automate

Use one-run timings or repeated profiles when measurements are requested:

```console
lev --timings build
lev profile --warmup 1 --repeat 5 -- lake build
```

Use `lev graph MODULE` for source-level project imports. Add `--external` only
when dependency edges matter, and select `--format json` or `--format dot` for
machine-readable output.

Use the ordered verification gate for project-wide checks:

```console
lev verify --offline --test --lint --imports
```

For a configured monorepo, apply import policy per member and emit one report:

```console
lev workspace shake --check --json --keep-going
lev workspace shake --check --sarif lev-shake.sarif --keep-going
```

Use `lev bundle -o project.lev.tar.zst` for a deterministic locked source
archive. It excludes dependency checkouts and generated outputs; it is not a
vendored or binary bundle.

Use `lev watch -- lake build` only for an interactive edit loop. Bound
automated runs with `--max-runs N`, and stop the watcher before finishing.

## Diagnose Failures

Start with the failed command, then inspect the relevant state:

```console
lev --verbose build
lev doctor
lev deps
lev tree
lev why PACKAGE
lev audit
lev cache status
lev cache verify
```

Report the first useful Lean, Lake, toolchain, dependency, or cache error.
Use `lev build --rehash` only when stale Lake hash sidecars are plausible.
Do not delete `.lake`, locks, dependency checkouts, or shared cache entries as
a first diagnostic step.

## Protect User State

- Preserve dirty source and dependency checkouts. Do not reset or clean them to
  make a command pass.
- Treat garbage collection without `--apply` as a preview.
- Do not publish, push remote caches, self-update, uninstall, or run destructive
  cleanup unless the user explicitly asks.
- Read `lev COMMAND --help` before using an unfamiliar mutating command.
- Inspect the final diff and report the commands actually run.
