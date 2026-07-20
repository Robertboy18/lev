#!/bin/sh
set -eu

# Small POSIX installer used by release assets. It intentionally depends only
# on curl and a platform SHA-256 utility, so a user does not need Rust.
#
# Inputs:
#   LEV_REPOSITORY  GitHub owner/repository (rendered into release installers)
#   LEV_VERSION     Optional tag, with or without the leading "v"
#   LEV_INSTALL_DIR Optional destination, defaulting to ~/.local/bin

# Release CI replaces this placeholder. LEV_REPOSITORY keeps the source
# installer useful for forks and local release testing.
repository=${LEV_REPOSITORY:-@LEV_REPOSITORY@}
if [ "$repository" = "@LEV_REPOSITORY@" ]; then
  echo "lev installer: set LEV_REPOSITORY=owner/repository for an unrendered source installer" >&2
  exit 2
fi

# Keep this table in sync with the binary names in release.yml. Unsupported
# machines fail explicitly instead of receiving a binary for the wrong ABI.
case "$(uname -s):$(uname -m)" in
  Linux:x86_64|Linux:amd64) asset=lev-linux-x86_64 ;;
  Darwin:arm64|Darwin:aarch64) asset=lev-macos-arm64 ;;
  *)
    echo "lev installer: no released binary for $(uname -s)/$(uname -m)" >&2
    exit 2
    ;;
esac

if [ -n "${LEV_VERSION:-}" ]; then
  version=$LEV_VERSION
  case "$version" in v*) ;; *) version="v$version" ;; esac
  base="https://github.com/$repository/releases/download/$version"
else
  base="https://github.com/$repository/releases/latest/download"
fi

tmp=${TMPDIR:-/tmp}/lev-install-$$
mkdir -m 700 "$tmp"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

# Fetch the binary and checksum serially so a failed request is unambiguous.
curl -fL --proto '=https' --tlsv1.2 "$base/$asset" -o "$tmp/$asset"
curl -fL --proto '=https' --tlsv1.2 "$base/$asset.sha256" -o "$tmp/$asset.sha256"

# Verify in staging before touching the user's existing lev binary.
expected=$(awk -v name="$asset" 'NF == 2 && ($2 == name || $2 == "*" name) { print $1 }' "$tmp/$asset.sha256")
if [ -z "$expected" ]; then
  echo "lev installer: malformed checksum file" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
fi
if [ "$actual" != "$expected" ]; then
  echo "lev installer: SHA-256 verification failed" >&2
  exit 1
fi

destination=${LEV_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$destination"
# `install` publishes the already-verified file with executable permissions.
install -m 755 "$tmp/$asset" "$destination/lev"
echo "installed lev to $destination/lev"
