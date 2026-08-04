# lev

A [uv](https://github.com/astral-sh/uv)-inspired environment manager for Lean.
lev keeps toolchains, dependencies, locks, caches, and repeatable commands tied
to a project while leaving its normal Lean files in charge.

I started lev because a second checkout should not forget everything the first
one downloaded, resolved, and built. Lake still owns the build graph and Lean
still checks the proofs. lev handles the environment around them.

[Documentation](https://robertboy18.github.io/lev/) |
[Getting started](https://robertboy18.github.io/lev/getting-started.html) |
[Command reference](https://robertboy18.github.io/lev/reference.html)

## Install

lev is distributed as the Cargo package `lev-cli` and installs the `lev`
executable:

```console
cargo install --git https://github.com/Robertboy18/lev --locked lev-cli
lev --version
lev doctor
```

Install [Rust](https://rustup.rs/) first if `cargo --version` is unavailable.
No repository clone is required.

## First Run

Use lev from an existing Lake project:

```console
cd my-lean-project
lev sync
lev build
```

Or create one:

```console
lev init hello --lean stable
cd hello
lev build
lev run lean --version
```

There is no activation step. Each command discovers the nearest Lake project,
selects its environment, and passes control to Lake or Lean.

## What lev Adds

| Job | Commands |
| --- | --- |
| Recreate a locked environment | `lev lock`, `lev sync --frozen`, `lev build --offline` |
| Reuse compatible work | `lev build`, `lev --local build`, `lev cache status` |
| Work across Lean versions | `lev lock --lean ...`, `lev build --lean ...`, `lev matrix` |
| Edit dependencies safely | `lev add`, `lev remove`, `lev outdated`, `lev upgrade --dry-run` |
| Minimize and inspect imports | `lev imports`, `lev graph`, `lev workspace shake` |
| Measure slow work | `lev --timings build`, `lev profile` |
| Run release checks | `lev audit`, `lev verify`, `lev export` |
| Automate a project | `lev task`, `lev script`, `lev watch`, `lev bundle` |

lev reads the ordinary `lean-toolchain`, Lakefile, and
`lake-manifest.json`. It adds `lev.lock` for the environment and integrity data
that those files do not record. Deleting lev does not turn the project into a
lev-specific package.

## Import Minimization

A Lean file may import all of Mathlib while using a small part of it. Finding
the right leaf modules by hand is harder than searching for declaration names:
syntax, tactics, instances, and elaborators can also create dependencies.

`lev imports` asks the selected Lake toolchain for compiled dependency
information:

```console
# Preview one module and explain each proposed import
lev imports MyProject.Proof --explain

# Apply the rewrite, then rebuild the project
lev imports MyProject.Proof --apply
```

For a reusable library, preserve deliberate public and implied imports:

```console
lev imports --keep-public --keep-implied --explain
lev imports --keep-public --keep-implied --apply
```

A preview never edits source and exits with status 1 when it finds changes.
During an apply run, lev snapshots every affected file, accepts Lake's proposed
imports, and rebuilds the requested module and package defaults. If
verification fails, lev restores the snapshot.

Some imports carry API or namespace intent that compiled declaration edges
cannot express. Keep those explicit:

```lean
import Mathlib.Some.Module -- shake: keep
```

The [imports guide](https://robertboy18.github.io/lev/imports.html) explains
coverage, isolated rewrites, CI output, and the whole-project run against
Imperial College London's FLT formalization.

## Where lev Fits

| Tool | Responsibility |
| --- | --- |
| [elan](https://github.com/leanprover/elan) | Install and select Lean toolchains |
| [Lake](https://github.com/leanprover/lean4/blob/master/src/lake/README.md) | Resolve packages and execute the build graph |
| lev | Lock environments, coordinate shared storage, and repeat workflows |

lev can use its verified toolchain store or an existing elan installation. It
does not replace Lake's package model or Lean's kernel.

## Reproducible Runs

```console
lev lock --check
lev sync --frozen
lev build --offline
lev verify --offline --strict
```

For EFS, NFS, or another shared filesystem, put lev's write-heavy local
workspace on node-local storage:

```console
LEV_CACHE_DIR="$LOCAL_SCRATCH/lev-cache" lev --local build
```

Inspect cache state with `lev cache status` and preview garbage collection with
`lev cache gc --max-age-days 14`. Nothing is removed until `--apply` is
present.

## Read More

- [Environments and cache](https://robertboy18.github.io/lev/environments.html)
  covers environment identity, lock modes, shared artifacts, remote cache
  snapshots, and version matrices.
- [Imports and graphs](https://robertboy18.github.io/lev/imports.html) covers
  compiler-guided import minimization, project coverage, and graph output.
- [Profiling](https://robertboy18.github.io/lev/profiling.html) separates lev
  phase timings, repeated command measurements, and Lean's native profiler.
- [Automation](https://robertboy18.github.io/lev/automation.html) covers
  verification, upgrades, workspaces, bundles, watch mode, tasks, and scripts.

Run `lev --help` for the complete command list.

The repository also includes an
[agent skill](.agents/skills/lev/SKILL.md) for coding agents working in Lean
projects.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development commands and
[SECURITY.md](SECURITY.md) for trust boundaries. Bug reports and focused pull
requests are welcome.

## License

Made by Robert Joseph George. lev is available under the
[MIT License](LICENSE-MIT).
