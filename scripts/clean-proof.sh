#!/usr/bin/env bash
# Prove one revision of this repo against PINNED sibling revisions (bd-v6j35).
#
# What it proves, always:
#   build (warnings are errors) · `test --workspace --no-run` (every target,
#   tests/ included) · full `-p flywheel --bin flywheel` suite · `workflow
#   verify` with a hermetic XDG_CONFIG_HOME · doorbell crates · verify-clippy ·
#   verify-formal (everystate) · verify-coverage (the behavior ledger).
#
# Siblings come from proof.toml (path + rev), read from the PROVEN revision
# (bd-gmrxv: a dirty working-tree pin cannot move the proof); their rev is
# archived from git objects, never a dirty working tree. No proof.toml means a
# single-repo project.
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
#                          [--no-cache] [--mutations-keep-going]
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
  # Which driver runs is part of the proof (bd-jp9z1.3 follow-up). A proof of
  # one sha must verdict that sha's own driver: the working tree may carry
  # another lane's uncommitted edit to this script or to its helpers, and then
  # the receipt would describe code nobody proved. --sha/--head take the driver
  # from the revision; only the dirty-tree mode runs the working copy.
  DRIVER_ARG=""
  prev=""
  for arg in "$@"; do
    case "$prev" in
      --sha) DRIVER_ARG="$arg" ;;
    esac
    case "$arg" in
      --head) DRIVER_ARG="HEAD" ;;
    esac
    prev="$arg"
  done
  if [ -n "$DRIVER_ARG" ]; then
    DRIVER_SHA="$(git -C "$ROOT" rev-parse "$DRIVER_ARG^{commit}" 2>/dev/null)" \
      || die "rev not found for the driver: $DRIVER_ARG"
    git -C "$ROOT" show "$DRIVER_SHA:scripts/clean-proof.sh" >"$SNAP_BASE/scripts/clean-proof.sh" 2>/dev/null \
      || die "no scripts/clean-proof.sh at $DRIVER_SHA"
    git -C "$ROOT" show "$DRIVER_SHA:scripts/proof-lib.sh" >"$SNAP_BASE/scripts/proof-lib.sh" 2>/dev/null \
      || die "no scripts/proof-lib.sh at $DRIVER_SHA"
  else
    DRIVER_SHA=""
    cp "$SELF_PATH" "$SNAP_BASE/scripts/clean-proof.sh"
    cp "$SELF_DIR/proof-lib.sh" "$SNAP_BASE/scripts/proof-lib.sh"
  fi
  chmod +x "$SNAP_BASE/scripts/clean-proof.sh"
  export CLEAN_PROOF_SNAPSHOTTED=1
  export CLEAN_PROOF_REPO_ROOT="$ROOT"
  export CLEAN_PROOF_SNAPSHOT_DIR="$SNAP_BASE"
  export CLEAN_PROOF_DRIVER_SHA="$DRIVER_SHA"
  echo "clean-proof: driver from ${DRIVER_SHA:-the working tree}" >&2
  exec /bin/bash "$SNAP_BASE/scripts/clean-proof.sh" "$@"
fi

# shellcheck source=proof-lib.sh
. "$SELF_DIR/proof-lib.sh"

KEEP=0
NO_CACHE=0
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/flywheel/mutation-verdicts"
# bd-b2yg7: green receipts are archived here so a landing can derive its
# mutation base instead of trusting a pin that never advances.
RECEIPT_ARCHIVE="${XDG_STATE_HOME:-$HOME/.local/state}/flywheel/clean-proof/receipts"
MUTATIONS_MODE=""
MUTATIONS_KEEP_GOING=0
MUT_FAILED_SPEC=""
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
    --no-cache) NO_CACHE=1; shift ;;
    # bd-xwn96: default is fail-fast; --mutations-keep-going restores the old
    # "run everything even after a VACUOUS spec" behaviour for triage.
    --mutations-keep-going) MUTATIONS_KEEP_GOING=1; shift ;;
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
# A proof can run for hours. The lock tells fence.sh's export sweep this dir
# is live even when it is older than the sweep age.
PROOF_LOCK="$TMP/.proof-running.lock"
: >"$PROOF_LOCK"

# bd-9dxjt: build into a REAL target dir the proof owns. By default mbx makes
# `target` a symlink into its shared managed cache, which ANY lane's `mbx gc`
# can evict mid-proof: the suite's self-spawning tests (current_exe) then fail
# with ENOENT because their binary was unlinked under a running process. A real
# CARGO_TARGET_DIR is not a registered managed target, so no sweep can reach
# our test binaries. Same carve-out verify-mutations.sh and e2e-live.sh use.
export CARGO_TARGET_DIR="$TMP/target"

CACHE_STORES="$TMP/cache-stores.tsv"
: >"$CACHE_STORES"
cleanup() {
  local status=$?
  rm -f "$PROOF_LOCK"
  if [ "$status" -ne 0 ] || [ "$KEEP" -eq 1 ]; then
    echo "clean-proof: kept $TMP" >&2
    # bd-3681w: a kept export is for the captain's install. Older ones must not
    # accumulate until the disk fills. bd-j45zo: TMPDIR may end in '/', so $TMP
    # is .../T//fw-proof.X while find prints .../T/fw-proof.X - excluding by the
    # path string deletes the export we just kept. Exclude by basename instead.
    # A red proof keeps the last installable export: prune only on green.
    if [ "$status" -eq 0 ]; then
      find "${TMPDIR:-/tmp}" -maxdepth 1 -type d -name 'fw-proof.*' \
        ! -name "$(basename "$TMP")" -exec sh -c \
        'for d do [ -f "$d/.proof-running.lock" ] || rm -rf "$d"; done' sh {} + 2>/dev/null || true
    fi
    # bd-9dxjt: a kept export keeps only logs, receipt and source. The real
    # proof-owned target dirs are multi-GB; drop them on this exit path too so
    # --keep never piles up a target per export (the disk was at 87%).
    rm -rf "$TMP/target" "$TMP/toron/target"
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

# Test hook (bd-j45zo): drive the EXIT cleanup without a build. The caller seeds
# older fw-proof.* dirs and sets TMPDIR (with a trailing slash) to reproduce the
# exact path spelling that deleted proof46's own kept export.
if [ -n "${CLEAN_PROOF_KEEP_TEST:-}" ]; then
  # bd-9dxjt: seed fake proof-owned target dirs so the EXIT cleanup's target
  # removal is exercised without a real multi-GB build.
  mkdir -p "$TMP/target" "$TMP/toron/target"
  printf 'target\n' >"$TMP/target/marker"
  printf 'toron\n' >"$TMP/toron/target/marker"
  exit "$CLEAN_PROOF_KEEP_TEST"
fi

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
SIBLING_TEST_STEPS=""
while IFS='	' read -r rel pin; do
  [ -n "${rel:-}" ] || continue
  sib_repo="$(cd "$ROOT/$rel" && pwd)"
  sib_name="$(basename "$rel")"
  proof_archive "$sib_repo" "$pin" "$TMP/$sib_name" \
    || die "cannot archive sibling $rel at $pin"
  SIBLING_RECORDS="$SIBLING_RECORDS$rel=$pin
"
  SIBLING_TEST_STEPS="$SIBLING_TEST_STEPS$sib_name
"
  echo "clean-proof: sibling $rel @ $pin"
done < <(proof_siblings "$ROOT" "$PROVEN_SHA")

# bd-jp9z1.3: specs whose spec file or targets changed most recently run
# first, so a fresh edit fails the proof early instead of after every other
# spec. The export has no git history, so the table is built from the real
# repo at the proven revision.
SPEC_RECENCY="$TMP/spec-recency.tsv"
: >"$SPEC_RECENCY"
for spec in scripts/mutations/*.spec.sh; do
  [ -f "$spec" ] || continue
  spec_files=("$spec")
  while IFS=$'\t' read -r _id target; do
    [ -n "${target:-}" ] && spec_files+=("$target")
  done < <(bash scripts/verify-mutations.sh --list "$spec" 2>/dev/null || true)
  spec_ts="$(git -C "$ROOT" log -1 --format=%ct "$PROVEN_SHA" -- "${spec_files[@]}" 2>/dev/null || true)"
  printf '%s\t%s\n' "${spec_ts:-0}" "$(basename "$spec")" >>"$SPEC_RECENCY"
done

# Mutation specs in the order above (newest change first, name on a tie).
ordered_specs() {
  local spec name ts
  for spec in scripts/mutations/*.spec.sh; do
    [ -f "$spec" ] || continue
    name="$(basename "$spec")"
    ts="$(awk -F'\t' -v n="$name" '$2 == n { print $1; exit }' "$SPEC_RECENCY")"
    printf '%s\t%s\n' "${ts:-0}" "$spec"
  done | sort -t "$(printf '\t')" -k1,1nr -k2,2 | cut -f2
}

cd "$TMP/flywheel"

STEP_NAMES=""
STEP_STATUSES=""
STEP_SECS=""
FAILED_STEP=""
LAST_STATUS=999

run_step() { # name cmd...
  local name="$1"
  shift
  local started
  started="$(date +%s)"
  echo "clean-proof: $name"
  set +e
  "$@" >"$TMP/$name.log" 2>&1
  LAST_STATUS=$?
  set -e
  STEP_NAMES="$STEP_NAMES$name
"
  STEP_STATUSES="$STEP_STATUSES$LAST_STATUS
"
  STEP_SECS="$STEP_SECS$(( $(date +%s) - started ))
"
  if [ "$LAST_STATUS" -ne 0 ]; then
    echo "clean-proof: $name failed; log $TMP/$name.log" >&2
    tail -25 "$TMP/$name.log" >&2 || true
    FAILED_STEP="$name"
    return 1
  fi
}

# bd-lfii3: a proof covers every repo it pins. Each sibling's OWN workspace
# suite runs in this same export at its pinned rev, fail-fast before the
# mutation stage, and is recorded as a receipt step.
sibling_tests() { # reads sibling names on stdin
  while IFS= read -r sib_name; do
    [ -n "$sib_name" ] || continue
    [ -z "$FAILED_STEP" ] || break
    [ -f "$TMP/$sib_name/Cargo.toml" ] || continue
    run_step "sibling-$sib_name-tests" env "CARGO_TARGET_DIR=$TMP/$sib_name/test-target" \
      "${MBX[@]}" test --manifest-path "$TMP/$sib_name/Cargo.toml" --workspace \
      --no-fail-fast || true
  done
}

# Warnings are errors for every workspace target (skips cached deps).
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"

# bd-jp9z1.3: the cheapest check that can fail runs first. A rotted needle
# costs under a second here instead of 40 minutes after the suite; a clippy
# error fails before the suite instead of after it.
run_step verify-needles bash scripts/verify-needles.sh || true
[ -n "$FAILED_STEP" ] || run_step build "${MBX[@]}" build --workspace --all-targets --locked || true
# bd-9dxjt: fail loudly if mbx still turned our target into a symlink into its
# evictable managed cache. A self-spawning test would then die with ENOENT
# after a concurrent sweep, and the proof would be blaming the code for a
# neighbour's gc. The target must be a real directory the proof owns.
if [ -z "$FAILED_STEP" ] && [ -L "$CARGO_TARGET_DIR" ]; then
  die "proof target is a symlink into mbx's swept cache, not a real dir ($CARGO_TARGET_DIR)"
fi
[ -n "$FAILED_STEP" ] || run_step no-run "${MBX[@]}" test --workspace --no-run || true
[ -n "$FAILED_STEP" ] || run_step verify-clippy bash scripts/verify-clippy.sh || true
# bd-jp9z1.5: the suite now executes the tests/ targets, and
# tests/adr0027_slice1 refuses an installed toron on purpose — it needs the
# PINNED sibling built at ../toron/target/debug/toron. Build the export's
# sibling, never the host's.
#
# bd-proof-toron-bin: the mutation stage below runs in verify-mutations' own
# scratch export, whose ../toron symlink points at the WORKSPACE tree, not this
# export. Hand it the CLI this step built, or adr0027-s1-receipts fails only
# inside the proof.
#
# The path handed over must be a file the PROOF owns. In the export,
# toron/target is a symlink into mbx's shared cache, and mbx evicts that
# directory while a multi-hour proof runs: proof38 handed over a path that
# dangled by the time the mutation stage reached it ("run failed: NotFound" at
# tests/adr0027_slice1.rs:121). Copy the binary out first, then hand over the
# copy.
TORON_BIN_BUILT=""
if [ ! -f "$TMP/toron/Cargo.toml" ]; then
  echo "clean-proof: no sibling toron to build; integration tests that need it are ignored"
elif [ -z "$FAILED_STEP" ]; then
  # bd-9dxjt: this sibling keeps its own proof-owned real target dir too; with
  # CARGO_TARGET_DIR exported above, pin it here so the copied CLI stays at the
  # path the mutation stage (and proof_steps/T3) expect.
  run_step sibling-toron env "CARGO_TARGET_DIR=$TMP/toron/target" "${MBX[@]}" build --manifest-path "$TMP/toron/Cargo.toml" -p toron-cli || true
  if [ -x "$TMP/toron/target/debug/toron" ]; then
    mkdir -p "$TMP/bin"
    cp "$TMP/toron/target/debug/toron" "$TMP/bin/toron"
    TORON_BIN_BUILT="$TMP/bin/toron"
  fi
fi
# bd-lfii3: sibling suites before flywheel's own suite and mutations.
sibling_tests <<< "$SIBLING_TEST_STEPS"
[ -n "$FAILED_STEP" ] || run_step suite "${MBX[@]}" test -p flywheel --tests || true
[ -n "$FAILED_STEP" ] || run_step workflow-verify \
  env "XDG_CONFIG_HOME=$TMP/config" "XDG_STATE_HOME=$TMP/state" \
  "${MBX[@]}" run --quiet --bin flywheel -- workflow verify --json || true
[ -n "$FAILED_STEP" ] || run_step doorbell \
  "${MBX[@]}" test -p doorbell-cli -p doorbell-rt -p doorbell-core || true
[ -n "$FAILED_STEP" ] || run_step stack-recover bash scripts/test-stack-recover.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-formal bash scripts/verify-formal.sh || true
[ -n "$FAILED_STEP" ] || run_step verify-coverage bash scripts/verify-coverage.sh || true

MUTATION_RECORDS=""

# Diff-scoped specs: every scripts/mutations/*.spec.sh whose target files the
# proven revision changed since the [mutations] base pin. `full` runs them all;
# no base (fresh clone, or a project without one) also means all, because a
# landing that skips proofs silently is worse than a slow one.
if [ -z "$FAILED_STEP" ]; then
  mut_base=""
  mut_scoped=0
  mut_changed=""
  # bd-r7852: even with NO --mutations, resolve the base and changed paths so
  # the receipt can say "witnesses" and landing can require crew witnesses.
  if [ "$MUTATIONS_MODE" != full ]; then
    # bd-b2yg7: a manual [mutations] base is an OVERRIDE; the normal base is the
    # newest green receipt that is an ancestor of the proven sha. A pin that
    # never advances starved the cache and re-ran every spec on each landing.
    mut_base="$(proof_mutation_base "$ROOT" "$PROVEN_SHA")"
    [ -n "$mut_base" ] || mut_base="$(proof_green_base "$ROOT" "$PROVEN_SHA" "$RECEIPT_ARCHIVE")"
    if [ -n "$mut_base" ] && git -C "$ROOT" cat-file -e "${mut_base}^{commit}" 2>/dev/null; then
      mut_scoped=1
      mut_changed="$(git -C "$ROOT" diff --name-only "$mut_base..$PROVEN_SHA" || true)"
      echo "clean-proof: mutations since $mut_base ($(printf '%s\n' "$mut_changed" | grep -c .) path(s))"
    else
      mut_base=""
      echo "clean-proof: no green base yet — running every spec"
    fi
  fi
  RECEIPT_MUTATIONS_BASE="$mut_base"
  RECEIPT_CHANGED_PATHS="$mut_changed"
  if [ -n "$MUTATIONS_MODE" ]; then
  MUT_STARTED="$(date +%s)"
  wrapper="$TMP/mbx-wrapper"
  {
    printf '#!/usr/bin/env bash\nexec'
    printf ' %q' "${MBX[@]}"
    printf ' "$@"\n'
  } >"$wrapper"
  chmod +x "$wrapper"
  mut_ran=0
  mut_failed=0
  while IFS= read -r spec; do
    [ -f "$spec" ] || continue
    if [ "$mut_scoped" -eq 1 ]; then
      hit=""
      while IFS=$'\t' read -r _id target; do
        if printf '%s\n' "$mut_changed" | grep -qxF -- "$target"; then
          hit=yes
          break
        fi
      done < <(bash scripts/verify-mutations.sh --list "$spec" 2>/dev/null)
      [ -n "$hit" ] || continue
    fi
    mut_ran=$((mut_ran + 1))
    # bd-jp9z1.2: content-addressed key over everything the verdict depends on.
    # A hit reuses a RED whose witness receipt is still green; --mutations full
    # and --no-cache bypass the cache by design.
    mut_key=""
    if [ "$NO_CACHE" -eq 0 ] && [ "$MUTATIONS_MODE" != full ]; then
      mut_key="$(python3 scripts/mutation-cache.py key --proven "$PROVEN_SHA" --repo "$ROOT" \
        --spec "$spec" --driver "${CLEAN_PROOF_DRIVER_SHA:-}" 2>/dev/null || true)"
    fi
    echo "clean-proof: mutations $spec"
    set +e
    # The proof build is -D warnings; a mutation deliberately leaves dead code
    # (that is what killing a guard looks like), so the witness run must not
    # inherit it or every such mutation reports BUILD-BROKEN.
    # bd-b2yg7: ONE target dir for the whole proof, owned by this export, shared
    # by every mutation run instead of cold-building a new one per spec. It is
    # never the shared mbx cache that bd-6setu.20 showed can be evicted mid-run.
    # bd-r7852: ONE scratch tree for the whole proof (siblings exported once),
    # plus ONE warm build dir. TMP is removed at proof exit.
    proof_env=(RUSTFLAGS="" "VERIFY_MUTATIONS_TARGET_DIR=$TMP/target" "VERIFY_MUTATIONS_BUILD_DIR=$TMP/mut-tree")
    [ -n "$TORON_BIN_BUILT" ] && proof_env+=("TORON_BIN=$TORON_BIN_BUILT")
    env "${proof_env[@]}" bash scripts/verify-mutations.sh "$spec" --cargo "$wrapper" \
      --cache-key "$mut_key" --cache-dir "$CACHE_DIR" \
      --json-out "$TMP/mut-rows.$mut_ran.jsonl" >"$TMP/mutations-$mut_ran.log" 2>&1
    mut_status=$?
    set -e
    if [ -f "$TMP/mut-rows.$mut_ran.jsonl" ]; then
      cat "$TMP/mut-rows.$mut_ran.jsonl" >>"$TMP/mutations.txt"
      if [ "$mut_status" -eq 0 ] && [ -n "$mut_key" ] && ! grep -q '"cached"' "$TMP/mut-rows.$mut_ran.jsonl"; then
        printf '%s\t%s\t%s\n' "$mut_key" "$spec" "$TMP/mut-rows.$mut_ran.jsonl" >>"$CACHE_STORES"
      fi
    fi
    if [ "$mut_status" -ne 0 ]; then
      echo "clean-proof: $spec did not prove every mutation; log $TMP/mutations-$mut_ran.log" >&2
      tail -20 "$TMP/mutations-$mut_ran.log" >&2 || true
      mut_failed=$((mut_failed + 1))
      MUT_FAILED_SPEC="$spec"
      # bd-xwn96: a doomed spec used to cost 40 more spec runs. Stop on the
      # first one; the receipt names it so the caller knows where to look.
      if proof_mutation_stops "$mut_status" "$MUTATIONS_KEEP_GOING"; then
        echo "clean-proof: stopping the mutation stage at $spec (pass --mutations-keep-going to run the rest)" >&2
        break
      fi
    fi
  done < <(ordered_specs)
  if [ "$mut_ran" -eq 0 ]; then
    echo "clean-proof: mutations no changed spec (nothing to re-prove)"
  else
    echo "clean-proof: mutations $mut_ran spec(s), $mut_failed failing"
  fi
  STEP_NAMES="$STEP_NAMES"$'mutations\n'
  STEP_SECS="$STEP_SECS$(( $(date +%s) - MUT_STARTED ))
"
  if [ "$mut_failed" -ne 0 ]; then
    STEP_STATUSES="$STEP_STATUSES"$'1\n'
    FAILED_STEP="mutations"
  else
    STEP_STATUSES="$STEP_STATUSES"$'0\n' 
  fi
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

# bd-n21ml: a red proof names the crew that can act on it. Batch one mail per
# owner (the bead assignee of the last commit touching the spec's target, else
# the captain) with the log path, cause and exact rerun command. A green
# receipt mails nobody about failures.
mail_red_owners() {
  PROOF_RECEIPT="$1" PROOF_REPO="$2" PROOF_PROJECT="$3" PROOF_MAIL_AS="$4" python3 - <<'PY'
import json, os, subprocess

def run(args, cwd=None):
    try:
        return subprocess.run(args, cwd=cwd or os.environ.get("PROOF_REPO", "."),
                              capture_output=True, text=True, check=False).stdout
    except OSError:
        return ""

with open(os.environ["PROOF_RECEIPT"], encoding="utf-8") as fh:
    receipt = json.load(fh)
if receipt.get("ok"):
    raise SystemExit(0)

repo = os.environ.get("PROOF_REPO", ".")
project = os.environ.get("PROOF_PROJECT", "flywheel")
sender = os.environ.get("PROOF_MAIL_AS", "Flywheel")
captain = os.environ.get("AGENT_NAME") or sender
logs = receipt.get("logs", "")

def bead_in(text):
    return next((t.rstrip(".,;:") for line in text.splitlines()
                 for t in line.replace("(", " ").replace(")", " ").split()
                 if t.startswith("bd-")), "")

def pin_for(bead):
    if not bead:
        return None
    try:
        return json.loads(run(["br", "show", bead, "--json"])).get("assignee") or None
    except ValueError:
        return None

def owner_for(target):
    # bd-1czzw follow-up: a git author is not a mailing identity. Resolve the
    # bead cited by the last commit touching the target; otherwise the captain.
    return pin_for(bead_in(run(["git", "log", "-1", "--format=%B", "--", target]))) or captain

def owner_for_step(failed, base):
    # bd-n21ml follow-up: a failed sibling step belongs to a SIBLING commit.
    # Ask that repo (at the rev the receipt pins), never the flywheel git name.
    scoped_repo = repo
    msg = ""
    if failed.startswith("sibling-") and failed.endswith("-tests"):
        name = failed[len("sibling-"):-len("-tests")]
        for sib in receipt.get("siblings", []):
            if os.path.basename(sib.get("path", "")) == name:
                scoped_repo = os.path.normpath(os.path.join(repo, sib["path"]))
                rev = sib.get("rev", "")
                msg += run(["git", "log", "-1", "--format=%B", rev], scoped_repo) if rev else ""
                break
    base = base or receipt.get("sha")
    if base:
        msg += run(["git", "log", f"{base}..HEAD", "--format=%B"], scoped_repo)
    for token in (t.rstrip(".,;:") for t in msg.split() if t.startswith("bd-")):
        pin = pin_for(token)
        if pin and pin != captain:
            return pin
    return captain

rows = []
for row in receipt.get("mutations", []):
    if not isinstance(row, dict) or row.get("verdict") == "RED":
        continue
    spec = row.get("spec", "")
    target = spec
    spec_path = os.path.join(repo, "scripts/mutations", spec)
    if os.path.isfile(spec_path):
        for line in open(spec_path, encoding="utf-8"):
            if line.strip().startswith("file="):
                target = line.strip().split("=", 1)[1].strip()
                break
    cause = f"{spec}#{row.get('id', '')} {row.get('expect_test', '')} {row.get('verdict', '?')}"
    log = f"{logs}/mutations-{spec}.log" if logs else "(see receipt logs)"
    rerun = f"bash scripts/verify-mutations.sh scripts/mutations/{spec} --cargo mbx"
    rows.append((owner_for(target), f"{cause}\nlog: {log}\nrerun: {rerun}"))

failed = receipt.get("failed_step")
if failed:
    owner = owner_for_step(failed, receipt.get("mutations_base") or receipt.get("sha"))
    log = f"{logs}/{failed}.log" if logs else "(see receipt logs)"
    rows.append((owner, f"clean-proof failed at {failed}\nlog: {log}\nrerun: bash scripts/clean-proof.sh --sha <sha>"))

batched = {}
for owner, body in rows:
    batched.setdefault(owner, []).append(body)
for owner, bodies in sorted(batched.items()):
    body = "\n\n".join(bodies)
    # bd-n21ml follow-up: a long test dump made the captain mail fail gift-wrap.
    # The log path and rerun command are the payload; never paste full output.
    if len(body) > 2000:
        body = body[:2000].rsplit("\n", 1)[0] + "\n... (truncated; see the log path above)"
    args = ["toron", "mail", "send", "--project", project, "--as", sender, "--to", owner,
            "--subject", f"[clean-proof] {len(bodies)} unproven row(s)",
            "--body", body]
    try:
        out = subprocess.run(args, cwd=repo, capture_output=True, text=True, check=False)
        if out.returncode:
            print(f"clean-proof: warn: red mail to {owner} failed: {out.stderr.strip()}")
        else:
            print(f"clean-proof: mailed {len(bodies)} red row(s) to {owner}")
    except OSError as exc:
        print(f"clean-proof: warn: red mail to {owner} skipped: {exc}")
PY
}

OK=0
[ -n "$FAILED_STEP" ] && OK=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
[ -n "$RECEIPT" ] || RECEIPT="${TMPDIR:-/tmp}/clean-proof/${SHORT}-${STAMP}.json"
mkdir -p "$(dirname "$RECEIPT")"
PROOF_MUTATIONS_MODE="${MUTATIONS_MODE:-witnesses}" \
PROOF_MUTATIONS_BASE="$RECEIPT_MUTATIONS_BASE" \
PROOF_CHANGED_PATHS="$RECEIPT_CHANGED_PATHS" \
PROOF_MUTATION_FAILED_SPEC="$MUT_FAILED_SPEC" \
PROOF_RECEIPT_ARCHIVE="$RECEIPT_ARCHIVE" \
proof_write_receipt "$RECEIPT" "$OK" "$PROVEN_SHA" "$SHORT" "$STAMP" "$MODE" "$TMP" \
  "$FAILED_STEP" "$STEP_NAMES" "$STEP_STATUSES" "$SIBLING_RECORDS" "$MUTATION_RECORDS_FILE" \
  "$STEP_SECS"

# bd-n21ml: route red rows to their owners once the receipt exists. Never fail
# a proof because a mail could not be delivered.
mail_red_owners "$RECEIPT" "$ROOT" "${CLEAN_PROOF_PROJECT:-flywheel}" \
  "${AGENT_NAME:-${FLYWHEEL_MAIL_AS:-Flywheel}}" || true

# bd-jp9z1.2: the witness for a freshly proved spec is THIS receipt, so the
# entries are written only after it exists.
if [ "$OK" -eq 0 ] && [ "$NO_CACHE" -eq 0 ] && [ -s "$CACHE_STORES" ]; then
  while IFS=$'\t' read -r store_key store_spec store_rows; do
    [ -n "$store_key" ] || continue
    python3 scripts/mutation-cache.py store --dir "$CACHE_DIR" --key "$store_key" \
      --spec "$store_spec" --rows "$store_rows" --witness "$RECEIPT" \
      --witness-sha "$PROVEN_SHA" >/dev/null 2>&1 || true
  done <"$CACHE_STORES"
  echo "clean-proof: cached $(wc -l <"$CACHE_STORES" | tr -d ' ') mutation spec(s) under $CACHE_DIR"
fi

if [ "$OK" -eq 0 ]; then
  echo "clean-proof: ok — $TMP"
  exit 0
fi
echo "clean-proof: FAILED at ${FAILED_STEP}; receipt $RECEIPT" >&2
exit 1
