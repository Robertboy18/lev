#!/bin/sh
set -eu

# This fixture models only the elan and Lake commands exercised by cli.rs.
# Environment variables let each process-level test select failure modes
# without generating a different executable for every case.
printf '%s\n' "$*" >> "$LEV_TEST_LOG"
state=${LEV_TEST_ELAN_STATE:?}
mkdir -p "$state/links"
installed="$state/installed"
touch "$installed"

if [ "${1:-}" = "--version" ]; then
  echo "elan fake"
  exit 0
fi

if [ "${1:-}" = "toolchain" ] && [ "${2:-}" = "list" ]; then
  cat "$installed"
  for link in "$state"/links/*; do
    [ -f "$link" ] || continue
    basename "$link"
  done
  exit 0
fi

if [ "${1:-}" = "toolchain" ] && [ "${2:-}" = "link" ]; then
  alias=${3:?}
  view=${4:?}
  case "$alias" in
    */*) echo "invalid alias: $alias" >&2; exit 91 ;;
  esac
  printf '%s\n' "$view" > "$state/links/$alias"
  exit 0
fi

if [ "${1:-}" = "toolchain" ] && [ "${2:-}" = "uninstall" ]; then
  toolchain=${3:?}
  next="$installed.next"
  grep -Fvx "$toolchain" "$installed" > "$next" || true
  mv "$next" "$installed"
  rm -f "$state/links/$toolchain"
  exit 0
fi

if [ "${1:-}" = "toolchain" ] && [ "${2:-}" = "gc" ]; then
  printf '{"unused":[],"deleted":false}\n'
  exit 0
fi

if [ "${1:-}" = "which" ] && [ "${2:-}" = "lean" ]; then
  toolchain=${ELAN_TOOLCHAIN:-}
  if grep -Fxq "$toolchain" "$installed"; then
    root=$LEV_TEST_TOOLCHAIN_ROOT
  else
    if [ -f "$state/links/$toolchain" ]; then
      root=$(cat "$state/links/$toolchain")
    else
      echo "unknown toolchain: $toolchain" >&2
      exit 92
    fi
  fi
  printf '%s/bin/lean\n' "$root"
  exit 0
fi

if [ "${1:-}" = "run" ]; then
  shift
  install=0
  if [ "${1:-}" = "--install" ]; then
    install=1
    shift
  fi
  toolchain="${1:-}"
  shift
  if [ "$install" = "1" ] && ! grep -Fxq "$toolchain" "$installed"; then
    printf '%s\n' "$toolchain" >> "$installed"
  fi

  if [ "${1:-}" = "lean" ] && [ "${2:-}" = "--version" ]; then
    echo "Lean fake ($toolchain)"
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "--version" ]; then
    echo "Lake fake ($toolchain)"
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "init" ]; then
    printf '[package]\nname = "fake"\n' > lakefile.toml
    printf 'temporary\n' > lean-toolchain
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "env" ] &&
     [ "${3:-}" = "lean" ] && [ "${4:-}" = "--version" ]; then
    printf 'lake-env\t%s\n' "$PWD" >> "$LEV_TEST_LOG"
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "env" ] &&
     [ "${3:-}" = "lean" ]; then
    printf 'lake-script\t%s\n' "$PWD" >> "$LEV_TEST_LOG"
    if [ -n "${LEV_TEST_SCRIPT_ARGS_FILE:-}" ]; then
      shift 3
      printf '%s\n' "$@" > "$LEV_TEST_SCRIPT_ARGS_FILE"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "update" ]; then
    printf 'lake-update\t%s\n' "$PWD" >> "$LEV_TEST_LOG"
    if [ "${LEV_TEST_LAKE_UPDATE_FAIL:-}" = "1" ] ||
       [ "${LEV_TEST_LAKE_UPDATE_FAIL_MEMBER:-}" = "$(basename "$PWD")" ]; then
      echo "simulated Lake update failure" >&2
      exit 42
    fi
    if [ -n "${LEV_TEST_LAKE_UPDATE_EXPECT_PACKAGE:-}" ] &&
       [ "${3:-}" != "$LEV_TEST_LAKE_UPDATE_EXPECT_PACKAGE" ]; then
      echo "Lake update did not receive expected package $LEV_TEST_LAKE_UPDATE_EXPECT_PACKAGE" >&2
      exit 48
    fi
    if [ -n "${LEV_TEST_LAKE_UPDATE_MANIFEST:-}" ]; then
      cp "$LEV_TEST_LAKE_UPDATE_MANIFEST" lake-manifest.json
    fi
    if [ -n "${LEV_TEST_LAKE_UPDATE_CLONE_URL:-}" ]; then
      mkdir -p .lake/packages
      git clone --quiet -- "$LEV_TEST_LAKE_UPDATE_CLONE_URL" .lake/packages/dep
      git -C .lake/packages/dep checkout --quiet --detach \
        "${LEV_TEST_LAKE_UPDATE_CLONE_REV:?}"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "build" ]; then
    printf 'lake-build\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
    if [ "${LEV_TEST_LAKE_BUILD_FAIL_MEMBER:-}" = "$(basename "$PWD")" ]; then
      exit "${LEV_TEST_LAKE_BUILD_EXIT:-37}"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "exe" ]; then
    printf 'lake-exe\t%s\t%s\n' "$PWD" "${3:-}" >> "$LEV_TEST_LOG"
    if [ -n "${LEV_TEST_TOOL_ARGS_FILE:-}" ]; then
      shift 4
      printf '%s\n' "$@" > "$LEV_TEST_TOOL_ARGS_FILE"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "--rehash" ] && [ "${3:-}" = "build" ]; then
    printf 'lake-build\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
    if [ "${LEV_TEST_LAKE_BUILD_FAIL_MEMBER:-}" = "$(basename "$PWD")" ]; then
      exit "${LEV_TEST_LAKE_BUILD_EXIT:-37}"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "upload" ]; then
    printf 'lake-upload\t%s\t%s\n' "$PWD" "${3:-}" >> "$LEV_TEST_LOG"
    exit "${LEV_TEST_LAKE_UPLOAD_EXIT:-0}"
  fi

  export ELAN_TOOLCHAIN="$toolchain"
  exec "$@"
fi

echo "unexpected fake elan invocation: $*" >&2
exit 90
