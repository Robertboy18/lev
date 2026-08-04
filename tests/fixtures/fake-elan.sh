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
    if [ "${3:-}" = "--no-build" ]; then
      printf 'lake-check\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
      if [ "${LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD:-}" = "1" ] &&
         [ "${LAKE_ARTIFACT_CACHE:-}" != "false" ]; then
        echo "shake freshness check used Lake's artifact cache" >&2
        exit 49
      fi
      if [ "${LEV_TEST_SHAKE_FRESHNESS_FAIL:-}" = "1" ]; then
        echo "error: object file 'Root.olean' of module Root does not exist" >&2
        exit 1
      fi
      if [ "${LEV_TEST_SHAKE_EXPLICIT_REQUIRE_BUILD:-}" = "1" ] &&
         [ ! -f "${LEV_TEST_SHAKE_BUILD_MARKER:?}" ]; then
        echo "error: target is out-of-date and needs to be rebuilt" >&2
        exit 3
      fi
      exit 0
    fi
    printf 'lake-build\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
    if [ "${LEV_TEST_SHAKE_REQUIRE_UNCACHED_BUILD:-}" = "1" ] &&
       [ "${LAKE_ARTIFACT_CACHE:-}" != "false" ]; then
      echo "shake verification used Lake's artifact cache" >&2
      exit 49
    fi
    if [ "${LEV_TEST_LAKE_BUILD_FAIL_MEMBER:-}" = "$(basename "$PWD")" ]; then
      exit "${LEV_TEST_LAKE_BUILD_EXIT:-37}"
    fi
    if [ -n "${LEV_TEST_SHAKE_FAIL_FILE:-}" ] &&
       grep -Fq "${LEV_TEST_SHAKE_FAIL_CONTENT:?}" "$LEV_TEST_SHAKE_FAIL_FILE"; then
      echo "simulated source-dependent build failure" >&2
      exit "${LEV_TEST_SHAKE_FAIL_EXIT:-41}"
    fi
    if [ -n "${LEV_TEST_SHAKE_BUILD_MARKER:-}" ]; then
      touch "$LEV_TEST_SHAKE_BUILD_MARKER"
    fi
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "shake" ]; then
    printf 'lake-shake\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
    fix=0
    explain=0
    for argument in "$@"; do
      if [ "$argument" = "--fix" ]; then
        fix=1
      fi
      if [ "$argument" = "--explain" ]; then
        explain=1
      fi
    done
    if [ "${LEV_TEST_SHAKE_REQUIRE_BUILD:-}" = "1" ] &&
       [ ! -f "${LEV_TEST_SHAKE_BUILD_MARKER:?}" ]; then
      echo "error: there are out of date oleans; run \`lake build\` first" >&2
      exit 1
    fi
    if [ "$explain" = "1" ]; then
      if [ -n "${LEV_TEST_SHAKE_EXPLAIN_FILE:-}" ]; then
        printf '%s:\n' "$LEV_TEST_SHAKE_EXPLAIN_FILE"
      fi
      if [ -n "${LEV_TEST_SHAKE_EXPLAIN_SECOND_FILE:-}" ]; then
        printf '%s:\n' "$LEV_TEST_SHAKE_EXPLAIN_SECOND_FILE"
      fi
    fi
    if [ "${LEV_TEST_SHAKE_SUGGEST:-}" = "1" ]; then
      printf '%s:\n' "${LEV_TEST_SHAKE_FILE:?}"
      echo "  remove #[import Mathlib]"
      echo "  add #[import Mathlib.Data.Nat.Prime.Basic]"
      if [ -n "${LEV_TEST_SHAKE_SECOND_FILE:-}" ]; then
        printf '%s:\n' "$LEV_TEST_SHAKE_SECOND_FILE"
        echo "  remove #[import Mathlib]"
        echo "  add #[import Mathlib.Algebra.Order.Ring.Nat]"
      fi
      if [ "$fix" = "1" ]; then
        cp "${LEV_TEST_SHAKE_REPLACEMENT_FILE:?}" "$LEV_TEST_SHAKE_FILE"
        count=1
        if [ -n "${LEV_TEST_SHAKE_SECOND_FILE:-}" ]; then
          cp "${LEV_TEST_SHAKE_SECOND_REPLACEMENT_FILE:?}" "$LEV_TEST_SHAKE_SECOND_FILE"
          count=2
        fi
        printf 'Successfully applied %s suggestions.\n' "$count"
        exit 0
      fi
      exit "${LEV_TEST_SHAKE_SUGGEST_EXIT:-1}"
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
