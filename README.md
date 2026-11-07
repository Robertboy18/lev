# lev

Lake builds large Lean developments well. I kept running into the jobs
around those builds that it leaves to you. A one-line edit low in Mathlib
can rebuild thousands of modules, and the reasons are sitting in Lake's
`.trace` files, but nothing reads them back. `lake check` replays the whole
environment through the kernel on every run, so a one-line edit costs the
same hour as a fresh checkout. The artifact cache only grows. `lake cache
clean` is the one way to shrink it. And when a build or a kernel check fails
on a new machine, the cause is often a missing tool or a cache on the wrong
filesystem.

`lev` is a small companion to Lake for those jobs. Lake still owns packages,
builds and caches, and Lean's kernels still check the proofs.

[Documentation](https://robertboy18.github.io/lev/) |
[Checking](https://robertboy18.github.io/lev/checking.html) |
[Command reference](https://robertboy18.github.io/lev/reference.html)

## Install

The Cargo package is named `lev-cli` and installs the `lev` binary:

```console
cargo install --git https://github.com/Robertboy18/lev --locked lev-cli
lev --version
lev doctor
```

Install [Rust](https://rustup.rs/) first if `cargo` is not already available.
lev runs Lean and Lake through [elan](https://github.com/leanprover/elan).

## Why did that rebuild?

```console
lev inspect build
lev inspect build --why Mathlib.Order.Basic -- Mathlib
lev inspect build --json
```

`lev inspect build` reads the trace of every module in every package, runs
`lake --verbose build` with Lake's output passing through, and reads the
traces again. For each module whose trace changed, it reports the inputs that
changed: the source file, an import, the options, or the Lean commit. A trace
that came from a cache download records no inputs, so for those modules lev
names the changed imports that explain the rebuild. `--why MODULE` follows
the causes back to the module that rebuilt for its own reasons.

## Checking only what changed

```console
lev check
lev check --kernel official --kernel nanoda --jobs 16
lev check --no-build --json
```

`lev check` builds the project, exports every module once with lev-export
(a per-module exporter in [`lean/export`](lean/export) that lev builds for
each toolchain), and checks each module as its own slice: the module's own
constants in full, imported theorems as axioms of the same type, and the
imported definitions it needs in full. The official kernel, its statically
linked build `leanchecker-paranoid`, and nanoda check the slices. Every
verdict is cached under the slice's content hash. A new proof of the same
statement rechecks the module's own slice, and the slices of importers that
needed the closure view, since a closure slice copies imported theorem
bodies. A new statement rechecks the importers that use it. A small linker
then checks that the accepted slices compose into one environment. The
exporter, Lake's queries and the kernels run in a `bwrap` sandbox like the
one `lake check` uses. The exit status is 0 for accept and 1 for reject: a
kernel rejected a slice in every view, or a linker rule failed. It is 2 when
lev could not finish the check, because of a setup error or a kernel run
that ended in an error, a timeout or a decline, and the summary names the
modules it could not check.

On Mathlib (commit ae9dcc7a4b, 96-core machine), the path `lake check` takes
is a 7:04 export plus 52:37 of single-threaded kernel time. lev-export writes
all 11,136 modules in 26 s with 16 jobs, and a cold `lev check` with the
official kernel takes 5:44 with 32 jobs. Its kernel time adds up to about
three times the single full check, because every slice rechecks the imported
definitions it copies. Over 20 consecutive Mathlib master commits, a typical
commit rechecked 1 to 7 slices in about 20 s of checking. The `nnnorm` to
`nnNorm` rename across 174 files rechecked 791 slices in 76 s, a definition
change 222 slices in 31 s, and a Batteries bump 11 slices.

This is not a replacement for the trust model of `lake check` in one respect:
whether the slices compose is decided by my own small linker, not by a proved
theorem. con-leche PR #13 (Joachim Breitner, still open) checks module by
module with a proved composition theorem. The verified kernels con-leche and
con-ron decline lev's slices, because they reject non-standard axioms and an
axiomized import is one. The [checking guide](https://robertboy18.github.io/lev/checking.html)
covers the views, the linker rules and the other limits.

## Keeping the caches in check

```console
lev cache status
lev cache gc
lev cache gc --lake
lev cache gc --lake --apply
```

Every `lev inspect build` and `lev check` registers its project. `lev cache
status` inventories lev's cache, the exporter builds, references and verdicts
of `lev check`, and the Lake caches the registered projects use, and reports
mappings whose artifacts are missing. `lev cache gc` lists the check caches
no check used in the last 30 days (`--max-age-days N`) and the records of
projects that are gone. With `--lake` it also lists the Lake mappings and
artifacts older than the same gate that no registered project's traces
reference. `--apply` deletes the list. Lake builds of projects lev has never
seen are not protected, and the list says when a Lake cache may serve such
projects, as the toolchain's default cache does, so read it before applying
it. A Lake cache whose references lev cannot read in full (an unreadable
trace or manifest, or a lakefile that sets `buildDir`) is skipped, with the
reason.

## Checking a machine

```console
lev doctor
lev doctor --json
```

`lev doctor` reports elan, Lean, and Lake for the project's toolchain, Git,
`bwrap`, the kernel checkers the toolchain ships, whether `lev check` has
built its exporter for the toolchain yet, where Lake's artifact cache lives
and whether it sits on the project's filesystem, and lev's own cache
directory. Lake hard-links cached artifacts only within one filesystem and
copies them otherwise, without saying so.

## Developing lev

```console
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

`cargo test` also checks that the links between the documentation pages
resolve, and runs `lev check` end to end on a tiny Lake project when elan,
the toolchain lev-export targets and `bwrap` are installed. On a network
filesystem, point `CARGO_TARGET_DIR` at local scratch first.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development commands and
[SECURITY.md](SECURITY.md) for trust boundaries. Bug reports and focused pull
requests are welcome. The repo also ships an
[agent skill](.agents/skills/lev/SKILL.md) for coding agents working in Lean
projects.

## License

Made by Robert Joseph George. lev is available under the
[MIT License](LICENSE-MIT). The writer in `lean/export` is a fork of
LeanExport from the Lean toolchain, which is under the Apache-2.0 license.
