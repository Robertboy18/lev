---
name: lev
description: Use lev next to Lake in Lean 4 projects to explain why `lake build` rebuilt modules, to check every module with external kernels while rechecking only what an edit changed, to inventory and prune lev's and Lake's caches, and to check the tools a build or kernel check needs. Use when a build rebuilt more than expected, when a kernel check of a large project is too slow to repeat after small edits, when the Lake cache keeps growing, or when a Lake build or `lake check` fails on a machine for unclear reasons.
---

# Use lev Next To Lake

lev is a companion to Lake, not a replacement. Build, test, and check the
project with the commands the repository documents. Reach for lev when you
need to know why Lake rebuilt something, want an incremental check of every
module with external kernels, want to know what the caches hold, or whether
the machine is set up for a build or a kernel check.

## Start From The Repository

1. Find the nearest directory containing `lean-toolchain`. Work there or pass
   `--project PATH`.
2. Read the repository instructions, README, `lean-toolchain`, and Lakefile
   before changing anything.
3. Inspect the worktree before editing. Preserve unrelated user changes.
4. Run `lev --version` and `lev doctor` if the host setup is uncertain.

Do not change `lean-toolchain` unless the task calls for a project-wide
upgrade.

## Explain A Rebuild

```console
lev inspect build
lev inspect build --json
lev inspect build --why MODULE -- TARGET
```

`lev inspect build` runs `lake --verbose build` once through elan, relays
Lake's output on stderr, and prints its report on stdout. Arguments after
`--` go to `lake build`, and the exit status is Lake's. `-q` keeps Lake's
output off stderr when only the report matters.

Read the `lev.cli.inspect.build/v2` report this way:

- `edited`: modules whose own source file changed.
- `modules[].causes`: the trace inputs that changed for each module: `source`,
  `import` (the imported module), `options`, `toolchain`, `dependency`,
  `libraries`, `configuration`, or `other`.
- `modules[].inferred: true`: one of the module's traces came from a cache
  download and recorded no inputs, so the causes are the changed imports.
- `modules[].status: "new"`: the module had no trace before the build, so
  there is nothing to compare.
- `unexplained`: modules whose trace changed with no cause lev could name.
- `why`: with `--why MODULE`, the rebuilt modules from MODULE back to the
  one that rebuilt for its own reasons.

The report explains the build lev ran. It cannot explain an earlier
`lake build`, because Lake overwrites its traces.

## Check Modules With External Kernels

```console
lev check
lev check --json
lev check --no-build --kernel official --kernel nanoda
```

`lev check` runs `lake build` through elan (skip it with `--no-build`),
exports every module with lev-export, checks one slice per module with
`leanchecker` (the default `official` kernel), `leanchecker-paranoid` or
nanoda, and reuses every cached verdict whose slice did not change. It exits
0 when the development is accepted, 1 when it is rejected (a kernel rejected
a slice in every view, or a linker rule failed), and 2 when it could not
check: a setup error, or a kernel run that ended in an error, a timeout or a
decline. The first run with a toolchain also builds lev-export and exports
the toolchain's `Init` as the reference, so it takes longer.

Read the `lev.cli.check/v2` report this way:

- `check.verdict`: `accept`, `reject`, or `unknown` when nothing was
  rejected but some module could not be checked.
- `check.unchecked`: the modules a kernel could not check. Retry them with a
  longer `--timeout` before reporting anything about them.
- `check.violations[]`: broken linker rules, with `rule` (L1 to L8),
  `module` and `message`.
- `check.results[]`: per module and kernel, the final `verdict` and `view`,
  and every attempt with `cached`, `seconds` and the kernel's `message`.
- `check.fallbacks[]`: modules that needed the `theorems` or `closure` view.
- `check.axioms`: the real axioms the checked modules use.

An accept means every slice was accepted and lev's own linker found that the
slices compose. It is not a `lake check` of the whole environment; do not
report it as one.

## Inspect And Prune The Cache

```console
lev cache status
lev cache status --json
lev cache gc
lev cache gc --lake --max-age-days 7 --json
```

`lev inspect build` and `lev check` register their project. `lev cache
status` inventories lev's cache, the caches of `lev check`, and the Lake
caches registered projects used, and exits 1 when a local mapping names an
artifact the cache does not hold. `lev cache gc` lists the exporter builds,
references and verdicts no check used, all older than the age gate, 30 days
by default. With `--lake` it also lists the Lake mappings and artifacts that
no live registered project's traces reference, skips a Lake cache whose
references it cannot read in full, and lists a Lake cache other projects may
use under `shared`. Without `--apply` it deletes nothing. Do not pass
`--apply` unless the user asked for cleanup, and pass `--lake` with it only
when they asked for Lake's cache: projects lev never built are not
protected.

## Check The Machine

```console
lev doctor
lev doctor --json
```

`lev doctor` reports elan, Lean, and Lake for the project's toolchain, Git,
`bwrap`, the kernel checkers in the toolchain (`leanchecker`,
`leanchecker-paranoid`, `lean4lean`, `nanoda_bin`), whether `lev check` has
built its exporter and reference for the toolchain, where Lake's artifact
cache lives and whether it shares the project's filesystem, and lev's cache.
`problems` lists what is wrong; doctor always exits 0. When the Lake cache is
on another filesystem, Lake copies artifacts instead of hard-linking them.
Suggest pointing `LAKE_CACHE_DIR` at the project's filesystem rather than
moving files.

## Write Maintainable Lean

Follow repository conventions first, then current Lean and Mathlib conventions.
Use any dedicated Lean or Mathlib skill supplied by the repository alongside
this one; lev explains builds and caches, not theorem design.

- Search nearby code and the pinned dependencies before inventing a definition
  or theorem.
- Verify declaration names in the active environment. Do not guess Mathlib
  APIs from memory.
- Preserve theorem statements, names, namespaces, and public APIs unless the
  task explicitly changes them.
- Prefer the narrow imports used by nearby files. Do not add `import Mathlib`
  merely to make elaboration succeed.
- Check difficult proof steps against the compiler as they are written. Finish
  with the repository's build, test, or lint command.
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
- Keep reference tables, safety rules, exit behavior, and command examples
  direct. Personality must not blur a contract.
- Read the prose aloud once. Remove filler, inflated claims, fake excitement,
  and phrases that could describe any developer tool.

## Protect User State

- Preserve dirty source and dependency checkouts. Do not reset or clean them
  to make a command pass.
- Treat `lev cache gc` without `--apply` as a preview.
- Do not delete `.lake`, `lake-manifest.json`, or cache entries as a first
  diagnostic step.
- Read `lev COMMAND --help` before using an unfamiliar option.
- Inspect the final diff and report the commands actually run.
