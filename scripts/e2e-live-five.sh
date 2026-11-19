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
CHECK_TARGET=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY=1; shift ;;
        --keep) KEEP=1; shift ;;
        --selftest) SELFTEST=1; shift ;;
        --check-target) [ $# -ge 2 ] || { echo "e2e-live-five: --check-target needs a dir" >&2; exit 2; }; CHECK_TARGET="$2"; shift 2 ;;
        -h | --help) sed -n '2,26p' "$0"; exit 0 ;;
        *) echo "e2e-live-five: unknown arg: $1" >&2; exit 2 ;;
    esac
done

die() { printf 'e2e-live-five: %s\n' "$*" >&2; exit 2; }

# bd-o2uol: the five suites run plain cargo against this repo's `target`. An
# mbx cache cleanup can leave `target` as a symlink to a directory that no
# longer exists; cargo then fails each of the five tests with a misleading
# "Not a directory". Diagnose it ONCE, before creating any live pane.
check_target_link() { # dir -> 0 when target is usable, 1 with one diagnosis line
    local dir="$1"
    local target="$dir/target"
    if [ -L "$target" ] && [ ! -e "$target" ]; then
        printf 'e2e-live-five: %s is a dangling symlink to %s (mbx cache cleanup?) — remove it or recreate the destination\n' \
            "$target" "$(readlink "$target")" >&2
        return 1
    fi
    return 0
}

if [ -n "$CHECK_TARGET" ]; then
    check_target_link "$CHECK_TARGET"
    exit $?
fi

STAMP="$(date +%Y%m%dT%H%M%SZ)"
PROJECT="${FLYWHEEL_LIVE_PROJECT:-e2e-live-five}"
# bd-oo7zi: crew prompt resolves the pane by OWNED workspace label - the
# session must start with flywheel-<project>- or it answers "no flywheel swarm".
SESSION="${FLYWHEEL_LIVE_SESSION:-flywheel-$PROJECT-$STAMP-$$}"
BLOCK="${FLYWHEEL_LIVE_BLOCK:-live-five}"
PIN="${FLYWHEEL_LIVE_PIN:-AmberProbe}"
AS="${FLYWHEEL_LIVE_AS:-$PIN}"
RECEIPT="${E2E_LIVE_FIVE_RECEIPT:-/tmp/e2e-live-five-$STAMP.json}"

TESTS=(
    tests/live_adr0027_toron_flags.rs
    tests/live_collab_patterns.rs
    tests/live_doorbell_operator_guard.rs
    tests/live_event_poll_parity.rs
    tests/live_serialize_worktree.rs
    tests/live_opencode_api_wake.rs
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
CARGO_BIN="${CARGO_BIN:-$(resolve_tool cargo)}" || die "cargo not found"
FLYWHEEL_BIN="${FLYWHEEL_BIN:-$(resolve_tool flywheel)}" || die "flywheel not found"
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

check_target_link "$ROOT" || exit 2
[ "${FLYWHEEL_LIVE_E2E:-}" = 1 ] || die "set FLYWHEEL_LIVE_E2E=1 to run the live suites deliberately"
[ -n "$TORON_BIN" ] || die "TORON_BIN not found; name the toron binary"

PLANE_SESSION=""
SPAWNED_PIN=""
cleanup() {
    if [ -n "$SPAWNED_PIN" ] && [ "$KEEP" = 0 ]; then
        "$FLYWHEEL_BIN" retire "$SPAWNED_PIN" --project e2e-live-five --force-working >/dev/null 2>&1 || true
    fi
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
# bd-oo7zi: flywheel names a rex pane by its BLOCK LABEL, not the session or the
# window. Rename the throwaway block to the pin so `flywheel send --to PIN`
# resolves it. (The earlier --window attempt renamed the wrong object.)
"$REX_BIN" block rename -s "$SESSION" "$BLOCK_ID" "$PIN" >/dev/null 2>&1 \
    || die "rex could not rename the throwaway block to $PIN"
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

# bd-oo7zi: the operator-typing guard needs a REAL harness pane: a bare zsh box
# classifies as Unknown, which may_inject allows. Spawn one in THIS project only,
# read its pin + pane_id from swarm status, and retire it in cleanup.
spawn_out="$("$FLYWHEEL_BIN" spawn e2e-live-five --oc 1 --model harness-default --escape --yes --retire-orphans 2>&1)" \
    || die "flywheel spawn failed: $spawn_out"
seated="$(FLYWHEEL_BIN="$FLYWHEEL_BIN" "$PYTHON_BIN" - <<'PY'
import json, os, subprocess
doc = json.loads(subprocess.check_output([
    os.environ["FLYWHEEL_BIN"], "swarm", "status", "e2e-live-five", "--json"
], text=True))
rows = doc.get("seats", doc if isinstance(doc, list) else [])
for r in rows:
    if r.get("pin") and r.get("pane_id"):
        print(f"{r['pin']}\t{r['pane_id']}")
        break
PY
)" || die "cannot read the spawned seat from swarm status"
SPAWNED_PIN="${seated%%$'\t'*}"
SPAWNED_PANE="${seated##*$'\t'}"
SPAWNED_SESSION="$(printf '%s\n' "$spawn_out" | sed -nE 's/.*(session:[^ ]+) created.*/\1/p' | head -1)"
[ -n "$SPAWNED_PIN" ] || die "swarm status named no pin"
[ -n "$SPAWNED_SESSION" ] || die "spawn did not name its workspace session"

# A freshly spawned agent stays WORKING while it boots; the send door correctly
# refuses "not at a prompt". Poll ITS status until it is idle (up to 2 min).
SPAWNED_STATE=""
for _ready in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24; do
    SPAWNED_STATE="$(STATUS_JSON="$("$FLYWHEEL_BIN" swarm status e2e-live-five --json 2>/dev/null)" STATUS_PIN="$SPAWNED_PIN" "$PYTHON_BIN" - <<'PY'
import json, os
try:
    doc = json.loads(os.environ["STATUS_JSON"])
    rows = doc.get("seats", doc if isinstance(doc, list) else [])
    print(next((r.get("status") or "") for r in rows if r.get("pin") == os.environ["STATUS_PIN"]))
except Exception:
    print("")
PY
)"
    case "$SPAWNED_STATE" in
        idle | done)
            if [ "${_idle_seen:-}" = yes ]; then break; fi
            _idle_seen=yes
            ;;
        *) _idle_seen=no ;;
    esac
    sleep 5
done

printf 'e2e-live-five: session %s (block %s), receipt %s\n' "$SESSION_ID" "$BLOCK_ID" "$RECEIPT"
results=""
passed=0
failed=0
for t in "${TESTS[@]}"; do
    log="/tmp/e2e-live-five-$STAMP-$(basename "$t" .rs).log"
    if [ "$t" = "tests/live_doorbell_operator_guard.rs" ] || [ "$t" = "tests/live_opencode_api_wake.rs" ]; then
        export FLYWHEEL_LIVE_SESSION="$SPAWNED_SESSION"
        export FLYWHEEL_LIVE_PIN="$SPAWNED_PIN"
        export FLYWHEEL_LIVE_BLOCK="$SPAWNED_PANE"
        export FLYWHEEL_LIVE_AS="$SPAWNED_PIN"
    else
        export FLYWHEEL_LIVE_SESSION="$SESSION_ID"
        export FLYWHEEL_LIVE_PIN="$PIN"
        export FLYWHEEL_LIVE_BLOCK="$BLOCK_ID"
        export FLYWHEEL_LIVE_AS="$AS"
    fi
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
