# Security

Report security-sensitive issues through a
[private security advisory](https://github.com/robertboy18/lev/security/advisories/new).
Do not publish credentials, private repository URLs, or exploit details in a
public issue.

lev runs elan, Lean, Lake, Git, and bwrap. `lev inspect build` and
`lev check` run `lake build`, which executes the project's Lakefile and the
code of every dependency with the user's permissions, so a project and its
dependencies must be treated as executable code. lev is not a sandbox for
builds.

After the build, `lev check` runs Lake's queries, lev-export and the kernels
in a bwrap sandbox: `/` read-only, home directories and scratch space
covered, the toolchain and the project bound read-only, only the export
directory writable, a cleared environment, and no network. `--no-sandbox`
turns that off and `--no-build` skips the unsandboxed build. Two things stay
trusted. Verdicts in lev's cache are believed as they are, so anyone who can
write to lev's cache directory can make a check accept. And whether the
checked slices compose is decided by lev's own linker, which has no proof.

`lev cache gc --apply` deletes files in lev's own cache directory, and with
`--lake` also in Lake's artifact caches, and only the files it listed. It
counts a registered project as the only evidence that a Lake entry is in
use, so Lake builds of other projects on the same cache may have to rebuild
what it removed. It skips a Lake cache whose references it cannot read in
full. Report any way to make it delete a file outside those directories
privately.

Release installers verify SHA-256 files downloaded from the same GitHub
release as the executable. This detects corruption and mismatched assets but
is not an independent signing authority; release security still depends on
TLS and control of the configured GitHub repository.
