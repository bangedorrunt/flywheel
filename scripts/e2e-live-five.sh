#!/usr/bin/env bash
# The five live suites against ONE throwaway rex pane (bd-4z7mb follow-up).
#
# Authoring-only companion of scripts/e2e-live.sh: `e2e-live.sh --five` execs
# this. It creates one session, runs every tests/live_*.rs against it, prints a
# per-test table, writes a JSON receipt under /tmp, and always tears the session
# down (trap on EXIT/INT/TERM/HUP). Nothing is sandboxed here that the tests
# need to be live — the pane IS the live thing, and `--dry-run` never touches it.
#
# Env it needs (all named, no ambient defaults beyond the session name):
#   FLYWHEEL_LIVE_E2E=1        the gate every live test checks
#   FLYWHEEL_LIVE_SESSION      the throwaway rex session (created + killed here)
#   FLYWHEEL_LIVE_BLOCK        the block/label the pane lives in
#   FLYWHEEL_LIVE_PIN          the pin the pane runs as
#   FLYWHEEL_LIVE_PROJECT      the project the suites act on
#   FLYWHEEL_LIVE_AS           the identity the suites send mail as
#   TORON_BIN                  the toron binary the suites shell out to
#
# Usage: scripts/e2e-live-five.sh [--dry-run] [--keep]
#   --dry-run  print the session, env, commands and receipt path; create nothing
#   --keep     leave the session alive on exit (debugging only)
# Exit: 0 all five passed, 1 a test failed, 2 usage/tool error.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DRY=0
KEEP=0
SELFTEST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY=1; shift ;;
        --keep) KEEP=1; shift ;;
        --selftest) SELFTEST=1; shift ;;
        -h | --help) sed -n '2,26p' "$0"; exit 0 ;;
        *) echo "e2e-live-five: unknown arg: $1" >&2; exit 2 ;;
    esac
done

die() { printf 'e2e-live-five: %s\n' "$*" >&2; exit 2; }

STAMP="$(date +%Y%m%dT%H%M%SZ)"
SESSION="${FLYWHEEL_LIVE_SESSION:-flywheel-live-$STAMP-$$}"
BLOCK="${FLYWHEEL_LIVE_BLOCK:-live-five}"
PIN="${FLYWHEEL_LIVE_PIN:-AmberProbe}"
PROJECT="${FLYWHEEL_LIVE_PROJECT:-e2e-live-five}"
AS="${FLYWHEEL_LIVE_AS:-$PIN}"
RECEIPT="${E2E_LIVE_FIVE_RECEIPT:-/tmp/e2e-live-five-$STAMP.json}"

TESTS=(
    tests/live_adr0027_toron_flags.rs
    tests/live_collab_patterns.rs
    tests/live_doorbell_operator_guard.rs
    tests/live_event_poll_parity.rs
    tests/live_serialize_worktree.rs
)

resolve_tool() {
    local name="$1" found
    if found="$(command -v "$name" 2>/dev/null)" && [ -n "$found" ]; then
        printf '%s\n' "$found"
        return 0
    fi
    for found in "$HOME/.local/bin/$name" "$HOME/.cargo/bin/$name" \
        /opt/homebrew/bin/"$name" /usr/local/bin/"$name"; do
        if [ -x "$found" ]; then
            printf '%s\n' "$found"
            return 0
        fi
    done
    return 1
}

REX_BIN="$(resolve_tool rex)" || die "rex not found (PATH, ~/.local/bin, homebrew, /usr/local/bin)"
CARGO_BIN="$(resolve_tool cargo)" || die "cargo not found"
if [ -x /usr/bin/python3 ]; then
    PYTHON_BIN=/usr/bin/python3
else
    PYTHON_BIN="$(resolve_tool python3)" || die "python3 not found"
fi
TORON_BIN="${TORON_BIN:-$(resolve_tool toron || true)}"

if [ "$DRY" = 1 ]; then
    cat <<EOF
e2e-live --five --dry-run: nothing is created, nothing runs.
session:  $SESSION   (then: $REX_BIN kill $SESSION on exit)
pane:     first pane of $SESSION, block $BLOCK, pin $PIN
env:      FLYWHEEL_LIVE_E2E=1
          FLYWHEEL_LIVE_SESSION=$SESSION
          FLYWHEEL_LIVE_BLOCK=$BLOCK
          FLYWHEEL_LIVE_PIN=$PIN
          FLYWHEEL_LIVE_PROJECT=$PROJECT
          FLYWHEEL_LIVE_AS=$AS
          TORON_BIN=${TORON_BIN:-<resolve at run time>}
tests:    ($CARGO_BIN test --test <file> -- --ignored --nocapture)
EOF
    for t in "${TESTS[@]}"; do
        printf '          %s\n' "$t"
    done
    cat <<EOF
receipt:  $RECEIPT
exit:     the session is killed by a trap on EXIT/INT/TERM/HUP unless --keep
EOF
    exit 0
fi

[ "${FLYWHEEL_LIVE_E2E:-}" = 1 ] || die "set FLYWHEEL_LIVE_E2E=1 to run the live suites deliberately"
[ -n "$TORON_BIN" ] || die "TORON_BIN not found; name the toron binary"

PLANE_SESSION=""
cleanup() {
    if [ -n "$PLANE_SESSION" ] && [ "$KEEP" = 0 ]; then
        "$REX_BIN" kill "$PLANE_SESSION" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM HUP

# One pane for all five. The session is created by LABEL and its ids come from
# rex's own JSON (session.create's output schema: session_id, initial_windows
# [{window_id, block_ids[]}]) - the CLI has no -s/--name/--block flags. A stale
# session of the same label is killed first so a crashed earlier run cannot make
# this one green against its leftovers.
"$REX_BIN" kill "$SESSION" >/dev/null 2>&1 || true
created="$("$REX_BIN" new "$SESSION" --json)" || die "rex could not create session $SESSION"
parsed="$(CREATED="$created" "$PYTHON_BIN" - <<'PY'
import json, os, sys
raw = os.environ["CREATED"]
try:
    doc = json.loads(raw)
    session = doc["session_id"]
    blocks = [b for w in (doc.get("initial_windows") or []) for b in (w.get("block_ids") or [])]
except Exception as exc:  # noqa: BLE001 - the refusal carries the raw shape
    sys.exit(f"cannot parse session.create output ({exc}): {raw}")
if not blocks:
    sys.exit(f"session.create returned no block: {raw}")
print(f"{session}\t{blocks[0]}")
PY
)" || die "cannot parse the created session: $created"
SESSION_ID="${parsed%%$'\t'*}"
BLOCK_ID="${parsed##*$'\t'}"
PLANE_SESSION="$SESSION"
export FLYWHEEL_LIVE_SESSION="$SESSION_ID"
export FLYWHEEL_LIVE_BLOCK="$BLOCK_ID"

if [ "$SELFTEST" = 1 ]; then
    printf 'selftest: session=%s block=%s\n' "$SESSION_ID" "$BLOCK_ID"
    exit 0
fi

export FLYWHEEL_LIVE_E2E=1
export FLYWHEEL_LIVE_SESSION="$SESSION_ID"
export FLYWHEEL_LIVE_BLOCK="$BLOCK_ID"
export FLYWHEEL_LIVE_PIN="$PIN"
export FLYWHEEL_LIVE_PROJECT="$PROJECT"
export FLYWHEEL_LIVE_AS="$AS"
export TORON_BIN

printf 'e2e-live-five: session %s (block %s), receipt %s\n' "$SESSION_ID" "$BLOCK_ID" "$RECEIPT"
results=""
passed=0
failed=0
for t in "${TESTS[@]}"; do
    log="/tmp/e2e-live-five-$STAMP-$(basename "$t" .rs).log"
    if "$CARGO_BIN" test --test "$(basename "$t" .rs)" -- --ignored --nocapture >"$log" 2>&1; then
        printf '  PASS  %s\n' "$t"
        passed=$((passed + 1))
        results="$results{\"test\":\"$t\",\"ok\":true,\"log\":\"$log\"},"
    else
        printf '  FAIL  %s  (log %s)\n' "$t" "$log"
        failed=$((failed + 1))
        results="$results{\"test\":\"$t\",\"ok\":false,\"log\":\"$log\"},"
    fi
done

{
    printf '{"schema":"flywheel-e2e-live-five.v1","stamp":"%s",' "$STAMP"
    printf '"session":"%s","block":"%s","pin":"%s","project":"%s","as":"%s",' \
        "$SESSION" "$BLOCK" "$PIN" "$PROJECT" "$AS"
    printf '"passed":%d,"failed":%d,"results":[%s]}\n' \
        "$passed" "$failed" "${results%,}"
} >"$RECEIPT"

printf 'e2e-live-five: %d passed, %d failed — receipt %s\n' "$passed" "$failed" "$RECEIPT"
[ "$failed" -eq 0 ]
