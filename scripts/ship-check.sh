#!/usr/bin/env bash
# Thin wrapper: the gate lives in `flywheel ship check`.
#
# Usage:
#   scripts/ship-check.sh <bead-id> [repo-dir] [--json]
#   flywheel ship check <bead-id> [--dir <repo-dir>] [--json]
#
# Exit: 0 legal ship, 1 not legal, 2 usage / environment error.
set -euo pipefail

usage() {
  sed -n '2,8p' "$0"
  exit "${1:-0}"
}

[ $# -ge 1 ] || usage 2
BIN="${FLYWHEEL_BIN:-flywheel}"
if [[ "$BIN" == */* ]]; then
  [ -x "$BIN" ] || {
    echo "ship-check: not executable: $BIN" >&2
    exit 2
  }
else
  command -v "$BIN" >/dev/null 2>&1 || {
    echo "ship-check: flywheel not on PATH (set FLYWHEEL_BIN)" >&2
    exit 2
  }
fi

# Preserve the old `<bead> [repo-dir]` form for scripts already using it.
if [ $# -ge 2 ] && [ "${2#-}" = "$2" ]; then
  exec "$BIN" ship check "$1" --dir "$2" "${@:3}"
fi
exec "$BIN" ship check "$@"
