# lev

Git gives you the source tree. It does not give you the exact Lean setup that
made the tree trustworthy.

I kept running into checkouts that had the right files and the wrong working
state: the wrong Lean toolchain, a different Mathlib revision, stale build
artifacts, or a cache nobody wanted to trust. `lev` makes that surrounding
state explicit while still letting Lake own packages and builds and Lean own
proof checking.

[Documentation](https://robertboy18.github.io/lev/) |
[Getting started](https://robertboy18.github.io/lev/getting-started.html) |
[Import minimization](https://robertboy18.github.io/lev/imports.html) |
[Profiling](https://robertboy18.github.io/lev/profiling.html) |
[Automation](https://robertboy18.github.io/lev/automation.html) |
[Command reference](https://robertboy18.github.io/lev/reference.html)

## Install

The Cargo package is named `lev-cli` and installs the `lev` binary. Install it
from the repository:

```console
cargo install --git https://github.com/Robertboy18/lev --locked lev-cli
lev --version
lev inspect
```

Install [Rust](https://rustup.rs/) first if `cargo` is not already available.

## First session

If I have just cloned a Lean project, I start with the commands that work
whether or not the project already uses lev:

```console
cd my-lean-project
lev inspect
lev doctor
lev sync
lev build
```

That answers the basic questions in order:

- Did `lev` discover the project I meant?
- Can this machine run the selected Lean and Lake?
- Can lev prepare the environment and hand the build to Lake?

For a repository that already commits `lev.lock`, use the stricter path:

```console
lev lock --check
lev sync --frozen
lev verify --offline
```

Add `--test` or `--lint` only when the project defines the corresponding Lake
drivers.

There is no activation step. Each `lev` command discovers the nearest Lake
project, selects the requested environment, and then delegates to Lake or Lean.

## What lev adds

### Reproducible project environments

`lev` reads the ordinary `lean-toolchain`, Lakefile, and
`lake-manifest.json`. It adds `lev.lock` for the environment and integrity data
those files do not record.

```console
lev sync
lev build
lev build --offline
```

Use `lev` when you want several Lean environments to stay isolated without
giving up shared caches, deterministic locks, or reviewable setup.

### Import minimization that rebuilds before it keeps the edit

The original motivating question was simple: if a file says `import Mathlib`,
can the tool figure out the smaller import set for me?

```console
lev shake MyProject.Proof --explain
lev shake MyProject.Proof --apply
```

On a file that only needs one narrow module, the preview looks like this:

```text
remove #[import Mathlib]
add #[import Mathlib.Data.Nat.Prime.Basic]
```

The exact replacement depends on the pinned Mathlib revision. `lev` asks the
selected Lake toolchain for compiled dependency evidence, applies repository
policy such as `--keep-public` or `--keep-implied`, and keeps the rewrite only
after the requested rebuild succeeds.

### Inspection and profiling that stay tied to the selected environment

```console
lev inspect
lev inspect dependencies
lev inspect declaration Nat.add_comm --from Mathlib
lev --timings build
lev profile --warmup 1 --repeat 5
lev inspect performance --files
```

These commands answer different questions. `inspect` explains the recorded
state. `--timings` splits one real run into environment, locking, delegated
command, and total time. `profile` collects repeated command samples. File mode
cuts a slow project down to the files worth opening in Lean's native profiler.

### Automation you can rerun

```console
lev verify --offline --test --lint
lev check --task test --task lint --imports
lev deps upgrade --check --test
lev workspace build --keep-going
lev project bundle --output artifacts/source.lev.tar.zst
lev watch -- lake build
```

`lev verify` is the fixed public gate. `lev check` adds repository policy.
Upgrade checks run in an isolated workspace before they touch the checkout.
Workspaces, bundles, and watch mode cover the practical parts of a real Lean
repository once it grows past one local machine.

## Mental model

Lake still owns package resolution and the build graph. Lean still owns
elaboration, kernel checking, and the native profiler. `lev` sits around them:
it locks the environment, coordinates shared state, and gives common workflows
stable command shapes.

Some commands stay short because they come up constantly:

- `lev doctor` / `lev inspect environment`
- `lev shake` / `lev inspect imports`
- `lev profile` / `lev inspect performance`
- `lev verify` / `lev check`

For the fuller command map, use the
[command reference](https://robertboy18.github.io/lev/reference.html).

## Documentation

The complete guides and references are:

- [Overview](https://robertboy18.github.io/lev/)
- [Getting started](https://robertboy18.github.io/lev/getting-started.html)
- [Environments and cache](https://robertboy18.github.io/lev/environments.html)
- [Import minimization](https://robertboy18.github.io/lev/imports.html)
- [Profiling](https://robertboy18.github.io/lev/profiling.html)
- [Inspection](https://robertboy18.github.io/lev/inspection.html)
- [Automation](https://robertboy18.github.io/lev/automation.html)
- [Command reference](https://robertboy18.github.io/lev/reference.html)
- [JSON contracts](https://robertboy18.github.io/lev/contracts.html)
- [Diagnostics and exits](https://robertboy18.github.io/lev/diagnostics.html)

The repo also ships an [agent skill](.agents/skills/lev/SKILL.md) for coding
agents working in Lean projects.

## Developing lev

The main local development pass is:

```console
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
npx --yes html-validate docs/*.html --rule no-inline-style:off
```

On a network filesystem, set `CARGO_TARGET_DIR` to local scratch before the
Rust commands so the build cache does not punish every metadata lookup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development commands and
[SECURITY.md](SECURITY.md) for trust boundaries. Bug reports and focused pull
requests are welcome.

## License

Made by Robert Joseph George. lev is available under the
[MIT License](LICENSE-MIT).
