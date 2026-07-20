# Security

Report security-sensitive issues through a
[private security advisory](https://github.com/robertboy18/lev/security/advisories/new).
Do not publish credentials, private repository URLs, or exploit details in a
public issue.

lev executes selected Lean toolchains, Git, Lake, optional elan, and commands
configured by the local project. A project and its dependencies must therefore
be treated as executable code. Cache validation protects storage integrity; it
is not a sandbox. Installed tools and inline scripts have the same trust
boundary: their Lake packages and Lean code execute with the user's
permissions.

Private metadata-registry credentials are referenced by environment-variable
name. lev does not persist token values in project configuration, request
URLs, logs, or cache records. Authenticated metadata caches are partitioned by
a SHA-256 token digest; use high-entropy credentials and rotate them through
the registry rather than editing cache data.

Remote cache and toolchain signatures authenticate the explicit public key
supplied by the user. They do not certify that an arbitrary key belongs to Lean
upstream. Distribute trust anchors separately from the object host, keep
private signing keys out of publication trees and logs, and report any path,
signature, digest, or extraction bypass privately.

Release installers and `lev self update` verify SHA-256 files downloaded from
the same GitHub release as the executable. This detects corruption and
mismatched assets but is not an independent signing authority; release
security still depends on TLS and control of the configured GitHub repository.
Signed remote caches and toolchain indexes use an explicit out-of-band public
key when that stronger trust separation is required.
