#!/usr/bin/env bash
# Prove one revision of this repo against PINNED sibling revisions (bd-v6j35).
#
# What it proves, always:
#   build (warnings are errors) · `test --workspace --no-run` (every target,
#   tests/ included) · full `-p flywheel --bin flywheel` suite · `workflow
#   verify` with a hermetic XDG_CONFIG_HOME · doorbell crates · verify-clippy ·
#   verify-formal (TLC) · verify-coverage (the behavior ledger).
#
# Siblings come from proof.toml (path + rev); their rev is archived from git
# objects, never a dirty working tree. No proof.toml means a single-repo project.
#
# Mutations: `--mutations changed` (the landing default) runs every spec under
# scripts/mutations/ whose target files changed since the `[mutations] base` pin
# in proof.toml; `--mutations full` runs all of them. Both execute
# scripts/verify-mutations.sh inside the export, so the receipt carries one
# machine-readable row per mutation (spec, id, expected test, verdict).
# `--mutate '<json>'` still applies one ad-hoc {file,find,replace,filter,expect}
# in the export. With none of them the receipt says "mutations": none.
#
# Usage:
#   scripts/clean-proof.sh [--sha <rev>] [--head] [--mutate <json>]...
#                          [--mutations changed|full] [--receipt <path>] [--keep]
#
#   no selector  : prove the dirty tree (temporary commit from intended paths)
#   --head       : prove HEAD
#   --sha <rev>  : prove an arbitrary revision
#
# Writes only under $TMPDIR and the receipt path (default
# $TMPDIR/clean-proof/<short>-<stamp>.json). Never inside the repo.
# Exit: 0 proof passed, 1 proof failed, 2 usage / environment error.
set -euo pipefail

SELF_PATH="${BASH_SOURCE[0]}"
SELF_DIR="$(cd "$(dirname "$SELF_PATH")" && pwd)"
SELF_REPO_ROOT="$(cd "$SELF_DIR/.." && pwd)"
ROOT="${CLEAN_PROOF_REPO_ROOT:-$SELF_REPO_ROOT}"

usage() { sed -n '2,29p' "$SELF_PATH"; exit "${1:-0}"; }
die() { echo "clean-proof: $*" >&2; exit 2; }

# bd-jp9z1.1: one machine-wide slot for the whole run, released on every exit
# path (process death closes the flock). Re-exec under a python holder so the
# lock fd survives exec into this script.
if [ "${CLEAN_PROOF_SLOTTED:-}" != 1 ]; then
  export CLEAN_PROOF_SLOTTED=1
  export CLEAN_PROOF_ORIG_SCRIPT="${CLEAN_PROOF_ORIG_SCRIPT:-$SELF_PATH}"
  export CLEAN_PROOF_REPO_ROOT="${CLEAN_PROOF_REPO_ROOT:-$SELF_REPO_ROOT}"
  exec python3 - "$0" "$@" <<'PY'
import fcntl, json, os, sys, time
script, args = sys.argv[1], sys.argv[2:]
lock = os.environ.get("CLEAN_PROOF_LOCK_FILE", os.path.join(os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR") or "/tmp", "clean-proof.lock"))
holder = lock + ".holder"
fd = open(lock, "a+")
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    who = "unknown holder"
    try:
        with open(holder, "r", encoding="utf-8") as f:
            who = f.read().strip()
    except Exception:
        pass
    print(f"clean-proof: waiting for slot ({who})", file=sys.stderr, flush=True)
    fcntl.flock(fd, fcntl.LOCK_EX)
info = {
    "pid": os.getpid(),
    "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "argv": [os.path.basename(script)] + args,
}
try:
    with open(holder, "w", encoding="utf-8") as f:
        json.dump(info, f, sort_keys=True)
        f.flush()
        os.fsync(f.fileno())
except Exception:
    pass
os.dup2(fd.fileno(), 9)
os.close(fd.fileno())
os.set_inheritable(9, True)
os.execv("/bin/bash", ["bash", script] + args)
PY
fi

# Snapshot driver + sourced helpers into scratch and re-exec so edits to the
# repo tree cannot break a running proof (bash reads scripts incrementally).
if [ "${CLEAN_PROOF_SNAPSHOTTED:-}" != 1 ]; then
  SNAP_BASE="${CLEAN_PROOF_SNAPSHOT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-driver.XXXXXX")}"
  mkdir -p "$SNAP_BASE/scripts"
  cp "$SELF_PATH" "$SNAP_BASE/scripts/clean-proof.sh"
  cp "$SELF_DIR/proof-lib.sh" "$SNAP_BASE/scripts/proof-lib.sh"
  chmod +x "$SNAP_BASE/scripts/clean-proof.sh"
  export CLEAN_PROOF_SNAPSHOTTED=1
  export CLEAN_PROOF_REPO_ROOT="$ROOT"
  export CLEAN_PROOF_SNAPSHOT_DIR="$SNAP_BASE"
  exec /bin/bash "$SNAP_BASE/scripts/clean-proof.sh" "$@"
fi

# shellcheck source=proof-lib.sh
. "$SELF_DIR/proof-lib.sh"

KEEP=0
MUTATIONS_MODE=""
RECEIPT_MUTATIONS_BASE=""
RECEIPT_CHANGED_PATHS=""
MODE="dirty"
TARGET_REV=""
RECEIPT=""
MUT_SPECS=""

LOCK_HOLD_TEST=""

while [ $# -gt 0 ]; do
  case "$1" in
    --sha) [ $# -ge 2 ] || die "--sha needs a revision"; MODE="sha"; TARGET_REV="$2"; shift 2 ;;
    --head) MODE="head"; shift ;;
    --mutate) [ $# -ge 2 ] || die "--mutate needs a spec"; MUT_SPECS="${MUT_SPECS}$2"$'\n'; shift 2 ;;
    --mutations)
      [ $# -ge 2 ] || die "--mutations needs changed|full"
      case "$2" in
        changed | full) MUTATIONS_MODE="$2" ;;
        *) die "--mutations takes changed or full (got $2)" ;;
      esac
      shift 2 ;;
    --receipt) [ $# -ge 2 ] || die "--receipt needs a path"; RECEIPT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --lock-hold-test) [ $# -ge 2 ] || die "--lock-hold-test needs seconds"; LOCK_HOLD_TEST="$2"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) die "unknown arg: $1" ;;
  esac
done

git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1 || die "not a git repo: $ROOT"

# Test hook (bd-jp9z1.1): prove slot serialization with a fake build step.
if [ -n "$LOCK_HOLD_TEST" ]; then
  marker="${CLEAN_PROOF_LOCK_TEST_MARKER:-}"
  if [ -n "$marker" ]; then
    printf 'start %s %s\n' "$$" "$(python3 -c 'import time; print(time.time_ns())')" >>"$marker"
    # Sleep in subprocess with lock fd closed so a killed holder releases flock.
    ( exec 9>&-; sleep "$LOCK_HOLD_TEST" ) &
    wait $! || true
    printf 'end %s %s\n' "$$" "$(python3 -c 'import time; print(time.time_ns())')" >>"$marker"
  else
    ( exec 9>&-; sleep "$LOCK_HOLD_TEST" ) &
    wait $! || true
  fi
  exit 0
fi

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
[ -n "$FAILED_STEP" ] || run_step stack-recover bash scripts/test-stack-recover.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-clippy bash scripts/verify-clippy.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-formal bash scripts/verify-formal.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-coverage bash scripts/verify-coverage.sh || true

MUTATION_RECORDS=""

# Diff-scoped specs: every scripts/mutations/*.spec.sh whose target files the
# proven revision changed since the [mutations] base pin. `full` runs them all;
# no base (fresh clone, or a project without one) also means all, because a
# landing that skips proofs silently is worse than a slow one.
if [ -n "$MUTATIONS_MODE" ] && [ -z "$FAILED_STEP" ]; then
  mut_base=""
  mut_scoped=0
  mut_changed=""
  if [ "$MUTATIONS_MODE" = changed ]; then
    mut_base="$(proof_mutation_base "$ROOT")"
    if [ -n "$mut_base" ] && git -C "$ROOT" cat-file -e "${mut_base}^{commit}" 2>/dev/null; then
      mut_scoped=1
      mut_changed="$(git -C "$ROOT" diff --name-only "$mut_base..$PROVEN_SHA" || true)"
      echo "clean-proof: mutations since $mut_base ($(printf '%s\n' "$mut_changed" | grep -c .) path(s))"
    else
      mut_base=""
      echo "clean-proof: no [mutations] base in proof.toml — running every spec"
    fi
  fi
  RECEIPT_MUTATIONS_BASE="$mut_base"
  RECEIPT_CHANGED_PATHS="$mut_changed"
  wrapper="$TMP/mbx-wrapper"
  {
    printf '#!/usr/bin/env bash\nexec'
    printf ' %q' "${MBX[@]}"
    printf ' "$@"\n'
  } >"$wrapper"
  chmod +x "$wrapper"
  mut_ran=0
  mut_failed=0
  for spec in scripts/mutations/*.spec.sh; do
    [ -f "$spec" ] || continue
    if [ "$mut_scoped" -eq 1 ]; then
      hit=""
      while IFS=$'\t' read -r _id target; do
        if printf '%s\n' "$mut_changed" | grep -qxF -- "$target"; then
          hit=yes
          break
        fi
      done < <(bash scripts/verify-mutations.sh --list "$spec")
      [ -n "$hit" ] || continue
    fi
    mut_ran=$((mut_ran + 1))
    echo "clean-proof: mutations $spec"
    set +e
    # The proof build is -D warnings; a mutation deliberately leaves dead code
    # (that is what killing a guard looks like), so the witness run must not
    # inherit it or every such mutation reports BUILD-BROKEN.
    RUSTFLAGS="" bash scripts/verify-mutations.sh "$spec" --cargo "$wrapper" \
      --json-out "$TMP/mut-rows.$mut_ran.jsonl" >"$TMP/mutations-$mut_ran.log" 2>&1
    mut_status=$?
    set -e
    if [ -f "$TMP/mut-rows.$mut_ran.jsonl" ]; then
      cat "$TMP/mut-rows.$mut_ran.jsonl" >>"$TMP/mutations.txt"
    fi
    if [ "$mut_status" -ne 0 ]; then
      echo "clean-proof: $spec did not prove every mutation; log $TMP/mutations-$mut_ran.log" >&2
      tail -20 "$TMP/mutations-$mut_ran.log" >&2 || true
      mut_failed=$((mut_failed + 1))
    fi
  done
  if [ "$mut_ran" -eq 0 ]; then
    echo "clean-proof: mutations no changed spec (nothing to re-prove)"
  else
    echo "clean-proof: mutations $mut_ran spec(s), $mut_failed failing"
  fi
  STEP_NAMES="$STEP_NAMES"$'mutations\n' 
  if [ "$mut_failed" -ne 0 ]; then
    STEP_STATUSES="$STEP_STATUSES"$'1\n'
    FAILED_STEP="mutations"
  else
    STEP_STATUSES="$STEP_STATUSES"$'0\n' 
  fi
fi

# Opt-in ad-hoc mutations, applied inside the export only (never the repo).
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
  printf '{"spec":"ad-hoc","id":"%s","expect_test":"%s","expect_green":%s,"verdict":"%s"}\n' \
    "$mut_file" "$mut_filter" "$([ "$mut_expect" = GREEN ] && echo true || echo false)" \
    "$mut_actual" >>"$TMP/mutations.txt"
  if ! proof_mutation_verdict "$mut_actual" "$mut_expect"; then
    echo "clean-proof: mutation expectation failed ($mut_actual != $mut_expect); log $TMP/mutation.log" >&2
    FAILED_STEP="mutation:$mut_file"
    break
  fi
done <"$TMP/mutations.in"
MUTATION_RECORDS_FILE="$TMP/mutations.txt"
touch "$MUTATION_RECORDS_FILE"
# Local belt for the receipt env below: a run that never entered the mutations
# section must still hand proof_write_receipt a DEFINED value under set -u.
RECEIPT_MUTATIONS_BASE="${RECEIPT_MUTATIONS_BASE:-}"
RECEIPT_CHANGED_PATHS="${RECEIPT_CHANGED_PATHS:-}"

OK=0
[ -n "$FAILED_STEP" ] && OK=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
[ -n "$RECEIPT" ] || RECEIPT="${TMPDIR:-/tmp}/clean-proof/${SHORT}-${STAMP}.json"
mkdir -p "$(dirname "$RECEIPT")"
PROOF_MUTATIONS_MODE="$MUTATIONS_MODE" \
PROOF_MUTATIONS_BASE="$RECEIPT_MUTATIONS_BASE" \
PROOF_CHANGED_PATHS="$RECEIPT_CHANGED_PATHS" \
proof_write_receipt "$RECEIPT" "$OK" "$PROVEN_SHA" "$SHORT" "$STAMP" "$MODE" "$TMP" \
  "$FAILED_STEP" "$STEP_NAMES" "$STEP_STATUSES" "$SIBLING_RECORDS" "$MUTATION_RECORDS_FILE"

if [ "$OK" -eq 0 ]; then
  echo "clean-proof: ok — $TMP"
  exit 0
fi
echo "clean-proof: FAILED at ${FAILED_STEP}; receipt $RECEIPT" >&2
exit 1
