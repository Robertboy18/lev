---
name: lev
description: Use lev to initialize, synchronize, lock, build, test, run, inspect, and troubleshoot Lean 4 and Lake projects. Use when working with lean-toolchain, lakefile.toml, lakefile.lean, lake-manifest.json, lev.lock, or lev.toml; when changing Lean toolchains or dependencies; or when a task would otherwise call lean, lake, or elan directly.
---

# Work With lev

Use lev as the project-level entry point while keeping Lean and Lake files
authoritative. Let lev prepare the selected toolchain and dependency
environment, then let Lake perform compilation and tests.

## Inspect The Project

1. Locate the nearest directory containing `lean-toolchain` and a
   `lakefile.toml` or `lakefile.lean`. Work there, or pass `--project PATH`.
2. Read the project README, `lean-toolchain`, Lakefile, and `lev.toml` before
   changing the environment.
3. Run `lev --version` and `lev doctor` when availability or host setup is in
   doubt.
4. Respect the toolchain in `lean-toolchain`. Do not change it unless the task
   explicitly requires a project-wide version change.

Do not require `lev.toml`; ordinary Lake projects work without it.

## Use The Normal Workflow

Use these commands for routine work:

```console
lev build
lev test
lev check Path/To/File.lean
lev run lake env lean --version
```

`lev build`, `lev test`, `lev check`, and `lev run` synchronize first unless
`--no-sync` is passed. Do not run a separate synchronization before every
command once the environment is ready.

For explicit setup, use `lev sync --locked` when the repository already
commits `lake-manifest.json`. Use `lev sync` only when the project intentionally
needs Lake to create its first manifest. Never edit `lake-manifest.json` by
hand.

Pass Lake targets and options through the lev command:

```console
lev build MyTarget
lev test -- --some-lake-option
lev run lake COMMAND
```

Use `lev --local build` when a checkout is on slow storage and a persistent
cache-local execution workspace is appropriate.

## Keep Runs Reproducible

Create or refresh locks only when the task calls for an environment change:

```console
lev lock
lev lock --check
lev sync --frozen
lev build --offline
```

- Commit `lev.lock` alongside `lake-manifest.json` when that is the
  repository's policy.
- Use `--frozen` to reject lock/configuration drift.
- Use `--offline` only when all required toolchains and dependencies should
  already be local.
- Do not use `--update`, `lev update`, or `lev lock --refresh` during an
  unrelated source change.

## Select Toolchains Deliberately

Use any user-supplied release, channel, nightly, or complete elan identifier;
never substitute a hardcoded version.

```console
lev build --lean "$TOOLCHAIN"
lev test --lean "$TOOLCHAIN"
lev lock --lean "$TOOLCHAIN"
```

Use `--lean` for a temporary environment without rewriting
`lean-toolchain`. Run `lev use TOOLCHAIN`, `lev pin TOOLCHAIN`, or
`lev upgrade --lean TOOLCHAIN` only when asked to change project state, and
inspect the resulting diff.

Use `lev matrix` only for an explicitly requested multi-toolchain CI check.
Pass each requested selector with `--lean`; do not invent a default version
list.

## Manage Dependencies Carefully

Use lev's transactional dependency commands for declarative
`lakefile.toml` projects:

```console
lev add PACKAGE --scope OWNER
lev add PACKAGE --git URL --rev REVISION
lev add PACKAGE --path ../relative/path
lev remove PACKAGE
lev update PACKAGE
lev outdated
lev upgrade PACKAGE --dry-run
```

- Preserve the package name, owner, URL, revision, and compatibility policy
  given by the user or project documentation.
- Never infer a registry owner or revision from a package name.
- Omitting `--rev` delegates selection to Reservoir and Lake; it does not
  select a lev-specific or hardcoded release.
- Do not mechanically rewrite executable `lakefile.lean` files. Follow the
  project's existing Lean configuration style when a manual edit is required.
- Inspect the Lakefile, `lake-manifest.json`, and `lev.lock` diff after any
  dependency change.
- Run package-specific helpers such as `lev run lake exe cache get` only when
  that package's documentation requests them. Do not assume Mathlib or any
  other dependency is present.

## Diagnose Failures

Start with the narrowest relevant command, then inspect the environment:

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

Treat a Lean elaboration error, test failure, or Lake target failure as a
project failure unless lev failed while selecting the toolchain,
synchronizing dependencies, or restoring artifacts. Report the underlying
command and first useful error instead of describing every later failure.

Use `lev build --rehash` when stale Lake hash sidecars are a plausible cause.
Do not delete `.lake`, dependency checkouts, locks, or shared cache entries as
a first diagnostic step.

## Protect User State

- Preserve dirty source and dependency checkouts. Never reset or clean them
  to make synchronization pass.
- Avoid `lev clean`, `lev cache gc --apply`, `lev tool gc --apply`,
  publication, remote cache pushes, self-updates, and self-uninstallation
  unless explicitly requested.
- Treat `lev cache gc` without `--apply` as a preview; review it before
  deletion.
- Keep normal Lean files usable with `lean`, `lake`, and `elan`. Do not
  introduce a lev-only project format when standard files are sufficient.
- Read `lev COMMAND --help` before using an unfamiliar or mutating command.
