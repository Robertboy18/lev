# lev

A [uv](https://github.com/astral-sh/uv)-inspired, fast project environment
manager for Lean: toolchains, dependencies, caches, and repeatable workspaces
in one command.

lev is built for repeated work. A cold Lean build is still a cold build, but
matching toolchains, Git dependencies, locked environments, and Lake artifacts
can be reused across projects instead of downloaded or compiled again.

**[Read the full documentation](https://robertboy18.github.io/lev/)** for
toolchains, version matrices, workspaces, scripts, remote caches, and CI.

## Installation

You do not need to clone this repository.

First, check for Cargo:

```console
cargo --version
```

If Cargo is missing, install Rust with [rustup](https://rustup.rs/).

macOS or Linux:

```console
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Windows users can run
[`rustup-init.exe`](https://win.rustup.rs/). On macOS, run
`xcode-select --install` if compilation reports a missing compiler or linker.

Install lev directly from GitHub:

```console
cargo install --git https://github.com/Robertboy18/lev --locked lev-cli
lev --version
lev doctor
```

The Cargo package is `lev-cli`; the installed executable is `lev`.

To remove lev later, preview the exact executable and removal method first:

```console
lev self uninstall --dry-run
lev self uninstall
```

Cargo installations are removed through `cargo uninstall lev-cli` so Cargo's
install record stays correct. lev keeps its cache and toolchain data.

## Quick Start

Use lev in an existing Lake project:

```console
cd my-lean-project
lev sync
lev build
```

Or create a project:

```console
lev init hello --lean stable
cd hello
lev build
lev run lean --version
```

Nothing needs to be activated.

## Why Use It?

- **Fast returns to existing environments.** Reuse compatible Lake artifacts,
  exact Git revisions, and installed toolchains across checkouts.
- **Several Lean versions without one shared mess.** Keep separate locks,
  manifests, build trees, and cache namespaces, then return to any of them.
- **Standard Lean projects.** `lean-toolchain`, the Lakefile, and
  `lake-manifest.json` remain authoritative. There is no private lev package
  format.
- **Frozen and offline runs.** Verify the toolchain, platform, manifest, lock,
  and dependency checkouts before a build starts.
- **Safer dependency edits.** Declarative Lakefile changes roll back if
  resolution does not complete.
- **Less duplicated storage.** Share immutable Git objects and compatible
  artifacts while keeping mutable project workspaces isolated.
- **Better cluster behavior.** `lev --local build` moves write-heavy project
  and `.lake` state into the cache, which can live on node-local scratch.
- **Consistent benchmark environments.** Keep the Lean side of LLM and
  theorem-proving evaluations tied to a specific toolchain, manifest, lock,
  and cache namespace.
- **Useful beyond one build.** Matrices, monorepo workspaces, standalone Lean
  scripts, package tools, audits, SBOM export, and signed remote artifacts use
  the same environment model.

## Elan, Lake, And lev

lev coordinates the tools Lean already has:

| Tool | Owns |
| --- | --- |
| [elan](https://github.com/leanprover/elan) | Installing and selecting Lean toolchains |
| [Lake](https://github.com/leanprover/lean4/blob/master/src/lake/README.md) | Resolving packages and executing the build graph |
| lev | Locks, shared storage, isolated environments, cache policy, and repeatable execution |

lev can use its verified toolchain store or fall back to elan. Lake still
decides what a build means and whether an artifact is valid.

## Fast Cache Reuse

In one measured identical fresh checkout, another checkout had already
populated the compatible artifact cache:

| Fresh checkout command | Elapsed |
| --- | ---: |
| Default Lake with an empty local artifact cache | 9.66 s |
| `lev build` with the compatible cache present | 0.26 s |
| Lake with the same shared cache configured manually | 0.25 s |

The warm lev run was about **37x faster than the cold run**. Manually
configured warm Lake was equally fast, as it should be: Lake is the artifact
cache engine. lev makes the cache path, toolchain namespace, locks, Git reuse,
verification, and cleanup consistent across projects.

This is a cache-reuse result, not a claim that lev compiles new Lean code 37x
faster. See the
[documentation](https://robertboy18.github.io/lev/#cache) for the measurement
conditions and cache boundaries.

## Automatic Import Minimization

Use the selected Lake toolchain's compiled dependency metadata to replace broad
imports such as `import Mathlib` with the modules a file actually needs.
`lev imports` is a descriptive alias for `lev shake`:

```console
lev imports MyProject.Proof --explain
lev imports MyProject.Proof --apply
```

For example, `MyProject/Primes.lean` might start with one broad public import:

```lean
module

public import Mathlib

public theorem five_is_prime : Nat.Prime 5 := Nat.prime_five
```

Running `lev imports MyProject.Primes --explain` produces a preview like this
(the `add` list is abridged):

```text
MyProject/Primes.lean:
  remove #[public import Mathlib]
  add #[..., public import Mathlib.Data.Nat.Prime.Defs]
  note: `public import Mathlib.Data.Nat.Prime.Defs` required
    because `five_is_prime` refers to `Nat.Prime`
```

The complete result can contain several private imports for syntax, tactics,
instances, or elaborators. Those are compiler-recorded dependencies, not
source-text guesses, and the exact list can change with the Mathlib version.
`--apply` writes the complete list, rebuilds the package defaults, and then
rebuilds any explicitly requested module roots. Verification builds bypass
Lake's artifact cache and finish with freshness checks, so a cache hit cannot
hide a missing `.olean`.

Without `--fix` or `--apply`, the command is a dry run and exits with status 1
when it finds changes. Applying snapshots every affected project source,
accepts Lake's suggestions, and verifies both default and requested targets. lev
restores all edited files if either build fails. Stale `.olean` files are built
automatically before analysis unless `--no-build` or `--force` is selected.

Omit the module to analyze the package's default targets. Use `--keep-public`
for deliberate public re-exports and `--keep-prefix` when a broader convenience
module is preferable to the exact leaf imports. The selected Lake must provide
its native `shake` command and the analyzed files must use its module system.

For a reusable library, start with the conservative whole-project form:

```console
lev imports --keep-public --keep-implied --explain
lev imports --keep-public --keep-implied --apply
```

If an import is needed only to open a namespace or provide another
elaboration-time effect that Lake cannot trace, preserve it explicitly:

```lean
import Mathlib.Some.Module -- shake: keep
```

For a large project, `--isolate` keeps each subset of edits only after it
passes the real build:

```console
lev imports --keep-public --keep-implied --fix --isolate
```

lev first tries the complete rewrite. If that fails, it restores and verifies
the original baseline, then splits the proposed files into smaller groups.
Passing groups remain applied; failing files are restored and reported with
candidate `-- shake: keep` annotations. A partial result exits with status 1,
and a broken baseline leaves every file unchanged.

CI can check drift and audit which sources Lake actually analyzed:

```console
lev imports --check --coverage
lev imports --check --coverage --json
```

Coverage separates project Lean files into analyzed module files, module files
outside the selected roots, and legacy files without a leading `module`
command. JSON uses the versioned `lev.cli.shake/v1` envelope and includes each
proposed, applied, rejected, or restored file.

### Case Study: Imperial College's FLT Project

On July 28, 2026, lev was tested against the
[Imperial College London FLT project](https://github.com/ImperialCollegeLondon/FLT)
at commit
[`0996e0c`](https://github.com/ImperialCollegeLondon/FLT/commit/0996e0c76ea248a676fba363be3ea8f04bdf3d44).
This was a whole-project default-target run, not a hand-picked example. The
checkout contained 269 Lean files; the `FLT` default target exposed 259
module-system files to Lake's import analysis.

| Measurement | Result |
| --- | ---: |
| Baseline full build | 8,920 jobs in 196.79 s |
| First, fully aggressive preview | 119 files suggested |
| Conservative final diff | 34 files, 9 insertions, 54 deletions |
| Apply and full verification | 8,920 jobs passed in 228.04 s |
| Repeat minimization run | Exit 0, no further changes |

The first pass was intentionally aggressive. Applying all 119 suggestions
caused compilation failures in 10 modules, so lev restored every source file
from its transaction snapshot. Preserving public imports reduced the proposal
to 76 files, but one module still failed. Preserving both public and implied
imports reduced it to 34:

```console
lev imports --keep-public --keep-implied --explain
lev imports --keep-public --keep-implied --apply
```

One file still depended on two imports for namespace or elaboration context
that was not represented in the compiled declaration dependency graph. Those
imports were made explicit policy:

```lean
import Mathlib.NumberTheory.NumberField.Basic -- shake: keep
import Mathlib.RingTheory.DedekindDomain.Basic -- shake: keep
```

After those annotations, lev rewrote 33 other files automatically and the
complete 8,920-job build passed. Running the same minimization command again
reported no suggestions, establishing a fixed point.

This progression is why lev does not treat Lake's first suggestion list as
proof of correctness. A public import can be part of a library's downstream
API even when the current file does not reference it directly, and a
namespace-only import may leave no declaration edge to trace. lev applies the
edits transactionally and accepts them only after rebuilding the real project.

The run also exposed a verification weakness: Lake's artifact cache could
report a successful build while omitting a changed `.olean`. lev now disables
that cache for import-verification builds and follows each build with a
freshness check. If either check fails, the original sources are restored.

The timings above are observations from one checkout and machine, not a claim
that import minimization makes builds faster by a fixed amount. The result being
tested is narrower: lev produced a smaller verified import surface and reached
a repeatable fixed point on a large formalization.

### What the FLT Run Changed

The first three follow-ups from the FLT run are now available:

- **Failure isolation:** `--fix --isolate` keeps verified subsets, restores
  rejected files, and suggests likely `-- shake: keep` annotations.
- **Coverage reports:** `--coverage` lists analyzed module files, files outside
  the selected roots, and legacy files without a module header.
- **First-class CI output:** `--check --json` reports import drift through the
  stable `lev.cli.shake/v1` envelope.

The remaining roadmap is:

- **SARIF output:** convert rejected edits and import drift into review
  annotations.
- **Before-and-after metrics:** report direct and transitive import counts,
  affected modules, and optional measured build or elaboration time.
- **Workspace operation:** minimize every selected Lake package with
  per-project transactions and one aggregate report.

## Version Switching And Matrices

Any release, RC, nightly, channel, or complete selector supported by the
configured toolchain source can be used. Versions are not hardcoded.

```console
lev lock --lean "$PAPER_TOOLCHAIN" --lean "$CURRENT_TOOLCHAIN"
lev build --lean "$PAPER_TOOLCHAIN"
lev build --lean "$CURRENT_TOOLCHAIN"
lev build --lean "$PAPER_TOOLCHAIN"
```

Use a matrix when the same command should run in isolated environments:

```console
lev matrix \
  --lean "$PAPER_TOOLCHAIN" \
  --lean "$CURRENT_TOOLCHAIN" \
  --keep-going \
  -- lake build --wfail
```

lev reports compatibility; it does not rewrite proofs for an incompatible
Lean release.

## Reproducible And Local Runs

```console
lev lock --check
lev sync --frozen
lev build --offline
lev audit
```

For a project on EFS, NFS, or another shared filesystem, point the cache at
local scratch:

```console
LEV_CACHE_DIR="$LOCAL_SCRATCH/lev-cache" lev --local build
```

Inspect storage before deleting anything:

```console
lev cache status
lev cache verify --full
lev cache gc --max-age-days 14
lev cache gc --max-age-days 14 --apply
```

Garbage collection is a dry run until `--apply` is present.

## More

```console
lev add PACKAGE --scope OWNER
lev run lake env my-tool
lev shake MyProject.Proof --fix
lev script run Example.lean
lev workspace build --keep-going
lev export --format cyclonedx -o lev-sbom.json
```

Run `lev --help` for the complete command list. The
[documentation](https://robertboy18.github.io/lev/) contains examples and
explains current limitations.

Want your coding agent to use lev too? It is easy :) The
[agent skill](https://github.com/robertboy18/lev/blob/main/.agents/skills/lev/SKILL.md)
in the `.agents` folder gives
Codex, Claude Code, and other agents the same sync, lock, build, offline, and
cache rules. You can tell an agent:

```text
Read .agents/skills/lev/SKILL.md, then use lev for this project.
```

## Contributing

lev is a new project, and contributions are welcome. The interface is open for
discussion, especially around package workflows, benchmark reproducibility, and
community conventions. If a platform, Lean project, proxy, filesystem, or
toolchain workflow behaves badly, please open an issue or send a pull request.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development commands and
[SECURITY.md](SECURITY.md) for trust boundaries.

## License

Made by Robert Joseph George.

lev is available under the [MIT License](LICENSE-MIT).
