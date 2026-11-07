#!/bin/sh
set -eu

# Models only the elan and Lake commands lev runs. Environment variables let
# each process-level test choose what the fake build prints and writes.
printf '%s\n' "$*" >> "$LEV_TEST_LOG"
installed="${LEV_TEST_ELAN_STATE:?}/installed"
touch "$installed"

if [ "${1:-}" = "--version" ]; then
  echo "elan fake"
  exit 0
fi

if [ "${1:-}" = "which" ] && [ "${2:-}" = "lean" ]; then
  toolchain=${ELAN_TOOLCHAIN:-}
  if ! grep -Fxq "$toolchain" "$installed"; then
    echo "error: toolchain '$toolchain' is not installed" >&2
    exit 92
  fi
  printf '%s/bin/lean\n' "$LEV_TEST_TOOLCHAIN_ROOT"
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
  if ! grep -Fxq "$toolchain" "$installed"; then
    if [ "$install" = "1" ]; then
      printf '%s\n' "$toolchain" >> "$installed"
    else
      echo "error: toolchain '$toolchain' is not installed" >&2
      exit 1
    fi
  fi

  if [ "${1:-}" = "lean" ] && [ "${2:-}" = "--version" ]; then
    echo "Lean fake ($toolchain)"
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "--version" ]; then
    echo "Lake fake ($toolchain)"
    exit 0
  fi

  if [ "${1:-}" = "lake" ] && [ "${2:-}" = "--verbose" ] && [ "${3:-}" = "build" ]; then
    printf 'lake-build\t%s\t%s\n' "$PWD" "$*" >> "$LEV_TEST_LOG"
    # The tree holds the files the build leaves behind, relative to the project.
    if [ -n "${LEV_TEST_LAKE_TREE:-}" ]; then
      cp -R "$LEV_TEST_LAKE_TREE"/. .
    fi
    if [ -n "${LEV_TEST_LAKE_STDOUT:-}" ]; then
      cat "$LEV_TEST_LAKE_STDOUT"
    fi
    if [ -n "${LEV_TEST_LAKE_STDERR:-}" ]; then
      cat "$LEV_TEST_LAKE_STDERR" >&2
    fi
    exit "${LEV_TEST_LAKE_EXIT:-0}"
  fi
fi

echo "unexpected fake elan invocation: $*" >&2
exit 90
