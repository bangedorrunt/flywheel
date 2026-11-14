#!/usr/bin/env bash
# Prove one revision of this repo against PINNED sibling revisions (bd-v6j35).
#
# What it proves, always:
#   build (warnings are errors) · `test --workspace --no-run` (every target,
#   tests/ included) · full `-p flywheel --bin flywheel` suite · `workflow
#   verify` with a hermetic XDG_CONFIG_HOME · doorbell crates · verify-clippy ·
#   verify-formal (TLC).
#
# Siblings come from proof.toml (path + rev); their rev is archived from git
# objects, never a dirty working tree. No proof.toml means a single-repo project.
#
# Mutations are OPT-IN: --mutate '<json>' applies {file,find,replace,filter,
# expect=RED|GREEN} inside the export and proves the expectation. Absent, the
# receipt says "mutations": none.
#
# Usage:
#   scripts/clean-proof.sh [--sha <rev>] [--head] [--mutate <json>]...
#                          [--receipt <path>] [--keep]
#
#   no selector  : prove the dirty tree (temporary commit from intended paths)
#   --head       : prove HEAD
#   --sha <rev>  : prove an arbitrary revision
#
# Writes only under $TMPDIR and the receipt path (default
# $TMPDIR/clean-proof/<short>-<stamp>.json). Never inside the repo.
# Exit: 0 proof passed, 1 proof failed, 2 usage / environment error.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"

KEEP=0
MODE="dirty"
TARGET_REV=""
RECEIPT=""
MUT_SPECS=""

usage() { sed -n '2,29p' "$0"; exit "${1:-0}"; }
die() { echo "clean-proof: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --sha) [ $# -ge 2 ] || die "--sha needs a revision"; MODE="sha"; TARGET_REV="$2"; shift 2 ;;
    --head) MODE="head"; shift ;;
    --mutate) [ $# -ge 2 ] || die "--mutate needs a spec"; MUT_SPECS="${MUT_SPECS}$2"$'\n'; shift 2 ;;
    --receipt) [ $# -ge 2 ] || die "--receipt needs a path"; RECEIPT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage 0 ;;
    *) die "unknown arg: $1" ;;
  esac
done

git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1 || die "not a git repo: $ROOT"

if [ -n "${MBX_CMD:-}" ]; then
  read -r -a MBX <<<"$MBX_CMD"
elif command -v mbx >/dev/null 2>&1; then
  MBX=(mbx +nightly-aarch64-apple-darwin)
else
  MBX=(cargo)
fi

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

mkdir -p "$TMP/flywheel" "$TMP/config" "$TMP/state"
case "$MODE" in
  sha)
    git -C "$ROOT" cat-file -e "$TARGET_REV^{commit}" 2>/dev/null \
      || die "rev not found: $TARGET_REV"
    PROVEN_SHA="$(git -C "$ROOT" rev-parse "$TARGET_REV")"
    git -C "$ROOT" archive "$PROVEN_SHA" | tar -x -C "$TMP/flywheel"
    ;;
  head)
    PROVEN_SHA="$(git -C "$ROOT" rev-parse HEAD)"
    git -C "$ROOT" archive "$PROVEN_SHA" | tar -x -C "$TMP/flywheel"
    ;;
  dirty)
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
    PROVEN_SHA="$(printf '%s\n' 'clean-proof' | git -C "$ROOT" \
      -c user.name='clean-proof' -c user.email='clean-proof@localhost' \
      commit-tree "$tree" -p HEAD)"
    echo "clean-proof: temporary commit $PROVEN_SHA (${#paths[@]} intended path(s))"
    git -C "$ROOT" archive "$PROVEN_SHA" | tar -x -C "$TMP/flywheel"
    ;;
esac
SHORT="$(git -C "$ROOT" rev-parse --short "$PROVEN_SHA")"

# Sibling trees: pinned revisions from proof.toml, archived from git objects.
SIBLING_RECORDS=""
while IFS='	' read -r rel pin; do
  [ -n "${rel:-}" ] || continue
  sib_repo="$(cd "$ROOT/$rel" && pwd)"
  sib_name="$(basename "$rel")"
  proof_archive "$sib_repo" "$pin" "$TMP/$sib_name" \
    || die "cannot archive sibling $rel at $pin"
  SIBLING_RECORDS="$SIBLING_RECORDS$rel=$pin
"
  echo "clean-proof: sibling $rel @ $pin"
done < <(proof_siblings "$ROOT")

cd "$TMP/flywheel"

STEP_NAMES=""
STEP_STATUSES=""
FAILED_STEP=""
LAST_STATUS=999

run_step() { # name cmd...
  local name="$1"
  shift
  echo "clean-proof: $name"
  set +e
  "$@" >"$TMP/$name.log" 2>&1
  LAST_STATUS=$?
  set -e
  STEP_NAMES="$STEP_NAMES$name
"
  STEP_STATUSES="$STEP_STATUSES$LAST_STATUS
"
  if [ "$LAST_STATUS" -ne 0 ]; then
    echo "clean-proof: $name failed; log $TMP/$name.log" >&2
    tail -25 "$TMP/$name.log" >&2 || true
    FAILED_STEP="$name"
    return 1
  fi
}

# Warnings are errors for every workspace target (skips cached deps).
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"

run_step build "${MBX[@]}" build --workspace --all-targets || true
[ -n "$FAILED_STEP" ] || run_step no-run "${MBX[@]}" test --workspace --no-run || true
[ -n "$FAILED_STEP" ] || run_step suite "${MBX[@]}" test -p flywheel --bin flywheel || true
[ -n "$FAILED_STEP" ] || run_step workflow-verify \
  env "XDG_CONFIG_HOME=$TMP/config" "XDG_STATE_HOME=$TMP/state" \
  "${MBX[@]}" run --quiet --bin flywheel -- workflow verify --json || true
[ -n "$FAILED_STEP" ] || run_step doorbell \
  "${MBX[@]}" test -p doorbell-cli -p doorbell-rt -p doorbell-core || true
[ -n "$FAILED_STEP" ] || run_step verify-clippy bash scripts/verify-clippy.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-formal bash scripts/verify-formal.sh || true

# Opt-in mutations, applied inside the export only (never the repo).
MUTATION_RECORDS=""
printf '%s' "$MUT_SPECS" >"$TMP/mutations.in"
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  [ -n "$FAILED_STEP" ] && break
  MUT_SPEC="$spec" MUT_OUT="$TMP/mutation.spec" python3 - <<'PY'
import json, os, sys
spec = os.environ["MUT_SPEC"]
try:
    d = json.loads(spec)
except Exception as exc:
    sys.exit("mutation spec is not JSON: %s" % exc)
missing = [k for k in ("file", "find", "replace", "filter") if not d.get(k)]
if missing:
    sys.exit("mutation spec missing: " + ", ".join(missing))
with open(os.environ["MUT_OUT"], "w", encoding="utf-8") as fh:
    for key in ("file", "find", "replace", "filter"):
        fh.write(d[key] + "\0")
    fh.write(d.get("expect", "RED") + "\0")
PY
  mut_file="$(python3 -c 'import sys;print(open(sys.argv[1],encoding="utf-8").read().split("\0")[0])' "$TMP/mutation.spec")"
  mut_find="$(python3 -c 'import sys;print(open(sys.argv[1],encoding="utf-8").read().split("\0")[1])' "$TMP/mutation.spec")"
  mut_replace="$(python3 -c 'import sys;print(open(sys.argv[1],encoding="utf-8").read().split("\0")[2])' "$TMP/mutation.spec")"
  mut_filter="$(python3 -c 'import sys;print(open(sys.argv[1],encoding="utf-8").read().split("\0")[3])' "$TMP/mutation.spec")"
  mut_expect="$(python3 -c 'import sys;print(open(sys.argv[1],encoding="utf-8").read().split("\0")[4])' "$TMP/mutation.spec")"
  echo "clean-proof: mutate $mut_file (expect $mut_expect on '$mut_filter')"
  backup="$(proof_apply_mutation "$mut_file" "$mut_find" "$mut_replace")" \
    || die "mutation did not apply: $mut_file"
  set +e
  "${MBX[@]}" test -p flywheel --bin flywheel "$mut_filter" >"$TMP/mutation.log" 2>&1
  mut_status=$?
  set -e
  proof_restore_mutation "$mut_file" "$backup"
  mut_actual="GREEN"
  [ "$mut_status" -ne 0 ] && mut_actual="RED"
  MUTATION_RECORDS="$MUTATION_RECORDS$mut_file:$mut_filter:expect=$mut_expect:got=$mut_actual
"
  if ! proof_mutation_verdict "$mut_actual" "$mut_expect"; then
    echo "clean-proof: mutation expectation failed ($mut_actual != $mut_expect); log $TMP/mutation.log" >&2
    FAILED_STEP="mutation:$mut_file"
    break
  fi
done <"$TMP/mutations.in"
MUTATION_RECORDS_FILE="$TMP/mutations.txt"
printf '%s' "$MUTATION_RECORDS" >"$MUTATION_RECORDS_FILE"

OK=0
[ -n "$FAILED_STEP" ] && OK=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
[ -n "$RECEIPT" ] || RECEIPT="${TMPDIR:-/tmp}/clean-proof/${SHORT}-${STAMP}.json"
mkdir -p "$(dirname "$RECEIPT")"
proof_write_receipt "$RECEIPT" "$OK" "$PROVEN_SHA" "$SHORT" "$STAMP" "$MODE" "$TMP" \
  "$FAILED_STEP" "$STEP_NAMES" "$STEP_STATUSES" "$SIBLING_RECORDS" "$MUTATION_RECORDS_FILE"

if [ "$OK" -eq 1 ]; then
  echo "clean-proof: ok — $TMP"
  exit 0
fi
echo "clean-proof: FAILED at ${FAILED_STEP}; receipt $RECEIPT" >&2
exit 1
