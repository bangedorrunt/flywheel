#!/usr/bin/env bash
# Prove the intended tree, not the dirty checkout.
#
# By default this snapshots every tracked/untracked change except tracker and
# lavish artifacts into a temporary commit object using a throwaway index, then
# archives that commit and builds it beside the sibling source trees. `--head`
# archives HEAD only.
#
# Usage:
#   scripts/clean-proof.sh [--head] [--tests] [--keep]
#
# Exit: 0 proof passed, 1 proof failed, 2 usage / environment error.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HEAD_ONLY=0
TESTS=0
KEEP=0

usage() {
  sed -n '2,12p' "$0"
  exit "${1:-0}"
}

die() {
  echo "clean-proof: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --head) HEAD_ONLY=1; shift ;;
    --tests) TESTS=1; shift ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage 0 ;;
    *) die "unknown arg: $1" ;;
  esac
done

git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1 || die "not a git repo: $ROOT"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fw-proof.XXXXXX")"
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] || [ "$KEEP" -eq 1 ]; then
    echo "clean-proof: kept $TMP" >&2
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

mkdir -p "$TMP/flywheel"
if [ "$HEAD_ONLY" -eq 1 ]; then
  echo "clean-proof: archiving HEAD"
  git -C "$ROOT" archive HEAD | tar -x -C "$TMP/flywheel"
else
  export GIT_INDEX_FILE="$TMP/index"
  git -C "$ROOT" read-tree HEAD
  paths=()
  while IFS= read -r -d '' path; do
    case "$path" in
      .beads/*|.lavish/*|.flywheel/agent-names.json) continue ;;
    esac
    paths+=("$path")
  done < <(
    git -C "$ROOT" diff --name-only -z HEAD
    git -C "$ROOT" ls-files -o --exclude-standard -z
  )
  if [ "${#paths[@]}" -gt 0 ]; then
    git -C "$ROOT" add -- "${paths[@]}"
  fi
  tree="$(git -C "$ROOT" write-tree)"
  commit="$(printf '%s\n' 'clean-proof' | git -C "$ROOT" \
    -c user.name='clean-proof' -c user.email='clean-proof@localhost' \
    commit-tree "$tree" -p HEAD)"
  echo "clean-proof: temporary commit $commit (${#paths[@]} intended path(s))"
  git -C "$ROOT" archive "$commit" | tar -x -C "$TMP/flywheel"
fi

for sibling in toron elm-effect-rs; do
  [ -d "$ROOT/../$sibling" ] || die "missing sibling source: $ROOT/../$sibling"
  ln -s "$(cd "$ROOT/../$sibling" && pwd)" "$TMP/$sibling"
done

if [ -n "${MBX_CMD:-}" ]; then
  read -r -a MBX <<<"$MBX_CMD"
elif command -v mbx >/dev/null 2>&1; then
  MBX=(mbx +nightly-aarch64-apple-darwin)
else
  MBX=(cargo)
fi

cd "$TMP/flywheel"
echo "clean-proof: check --workspace"
set +e
"${MBX[@]}" check --workspace 2>&1 | tee "$TMP/check.log"
check_status=${PIPESTATUS[0]}
set -e
echo "$check_status" >"$TMP/check.status"
if [ "$check_status" -ne 0 ]; then
  echo "clean-proof: check failed; log $TMP/check.log" >&2
  exit 1
fi

echo "clean-proof: workflow verify --json"
set +e
"${MBX[@]}" run --quiet --bin flywheel -- workflow verify --json 2>&1 | tee "$TMP/workflow-verify.log"
verify_status=${PIPESTATUS[0]}
set -e
echo "$verify_status" >"$TMP/workflow-verify.status"
if [ "$verify_status" -ne 0 ]; then
  echo "clean-proof: workflow verify failed; log $TMP/workflow-verify.log" >&2
  exit 1
fi

if [ "$TESTS" -eq 1 ]; then
  echo "clean-proof: doorbell tests"
  set +e
  "${MBX[@]}" test -p doorbell-cli -p doorbell-rt -p doorbell-core 2>&1 | tee "$TMP/tests.log"
  test_status=${PIPESTATUS[0]}
  set -e
  echo "$test_status" >"$TMP/tests.status"
  if [ "$test_status" -ne 0 ]; then
    echo "clean-proof: tests failed; log $TMP/tests.log" >&2
    exit 1
  fi
else
  echo 0 >"$TMP/tests.status"
fi

echo "clean-proof: ok — $TMP"
