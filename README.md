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
