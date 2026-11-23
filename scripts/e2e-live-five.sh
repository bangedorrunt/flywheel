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
CHECK_EXPORT=""
TARGET_REV="HEAD"
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY=1; shift ;;
        --keep) KEEP=1; shift ;;
        --selftest) SELFTEST=1; shift ;;
        --sha) [ $# -ge 2 ] || { echo "e2e-live-five: --sha needs a revision" >&2; exit 2; }; TARGET_REV="$2"; shift 2 ;;
        --head) TARGET_REV="HEAD"; shift ;;
        --check-target) [ $# -ge 2 ] || { echo "e2e-live-five: --check-target needs a dir" >&2; exit 2; }; CHECK_TARGET="$2"; shift 2 ;;
        --check-export) [ $# -ge 2 ] || { echo "e2e-live-five: --check-export needs a dir" >&2; exit 2; }; CHECK_EXPORT="$2"; shift 2 ;;
        -h | --help) sed -n '2,26p' "$0"; exit 0 ;;
        *) echo "e2e-live-five: unknown arg: $1" >&2; exit 2 ;;
    esac
done

die() { printf 'e2e-live-five: %s\n' "$*" >&2; exit 2; }

# bd-833ya: this script KILLS its project on exit. A live project name here
# would reap the real fleet's crews, so refuse before creating any session.
if [ -n "${FLYWHEEL_LIVE_PROJECT+x}" ]; then
    case "$FLYWHEEL_LIVE_PROJECT" in
        e2e-*) ;;
        *) die "FLYWHEEL_LIVE_PROJECT must be a non-empty e2e- throwaway name, got '$FLYWHEEL_LIVE_PROJECT'" ;;
    esac
fi

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

# bd-w5tgg: the live leg measures a REVISION, never the shared tree. Archive the
# named revision and its proof.toml-pinned siblings into a throwaway export and
# build/run every suite there, so a neighbour's WIP cannot break or fake a leg.
prepare_export() { # repo-dir -> sets PROVEN_SHA, TEST_REPO, EXPORT_ROOT
    local repo="$1" rel pin sib_repo
    # shellcheck source=proof-lib.sh
    . "$ROOT/scripts/proof-lib.sh"
    git -C "$repo" cat-file -e "$TARGET_REV^{commit}" 2>/dev/null \
        || die "rev not found: $TARGET_REV"
    PROVEN_SHA="$(git -C "$repo" rev-parse "$TARGET_REV")"
    EXPORT_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/e2e-live-five-export.XXXXXX")"
    mkdir -p "$EXPORT_ROOT/flywheel"
    git -C "$repo" archive "$PROVEN_SHA" | tar -x -C "$EXPORT_ROOT/flywheel" \
        || die "cannot archive $PROVEN_SHA"
    while IFS=$'\t' read -r rel pin; do
        [ -n "${rel:-}" ] || continue
        sib_repo="$(cd "$repo/$rel" && pwd)"
        proof_archive "$sib_repo" "$pin" "$EXPORT_ROOT/$(basename "$rel")" \
            || die "cannot archive sibling $rel at $pin"
    done < <(proof_siblings "$repo" "$PROVEN_SHA")
    TEST_REPO="$EXPORT_ROOT/flywheel"
}

if [ -n "$CHECK_TARGET" ]; then
    check_target_link "$CHECK_TARGET"
    exit $?
fi
if [ -n "$CHECK_EXPORT" ]; then
    prepare_export "$CHECK_EXPORT"
    printf '%s\n%s\n' "$PROVEN_SHA" "$EXPORT_ROOT"
    exit 0
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
    tests/live_events_delivery.rs
    tests/live_compose_join_dispatch.rs
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
# bd-w5tgg: when not named by the caller, flywheel is built from the export
# below, not resolved from the shared tree's installed shim.
FLYWHEEL_BIN="${FLYWHEEL_BIN:-}"
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
sha:      $(git -C "$ROOT" rev-parse "$TARGET_REV")
env:      FLYWHEEL_LIVE_E2E=1
          FLYWHEEL_LIVE_SESSION=$SESSION
          FLYWHEEL_LIVE_BLOCK=$BLOCK
          FLYWHEEL_LIVE_PIN=$PIN
          FLYWHEEL_LIVE_PROJECT=$PROJECT
          FLYWHEEL_LIVE_AS=$AS
          TORON_BIN=${TORON_BIN:-<resolve at run time>}
tests:    (from a clean export of the sha: $CARGO_BIN test --test <file> -- --ignored --nocapture)
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

PLANE_SESSION=""
SPAWNED_PIN=""
WAKE_PIN=""
# bd-833ya: spawn installs a per-project doorbell supervisor. Retiring the
# seats alone leaves the project registered, and the next stack install boots
# that supervisor back up. Drop both the registration and the unit.
retire_throwaway_project() { # project
    local label fw="${FLYWHEEL_BIN:-flywheel}"
    case "$1" in
        e2e-*) ;;
        *) echo "e2e-live-five: refuse to retire non-throwaway project '$1'" >&2; return 0 ;;
    esac
    "$fw" kill "$1" --force --force-working --reap-crews >/dev/null 2>&1 || true
    label="dev.flywheel.doorbell.$1"
    if [ "$(uname -s)" = Darwin ]; then
        launchctl bootout "gui/$(id -u)/$label" >/dev/null 2>&1 || true
        rm -f "$HOME/Library/LaunchAgents/$label.plist"
    else
        systemctl --user disable --now "$1.service" >/dev/null 2>&1 || true
        rm -f "$HOME/.config/systemd/user/$1.service"
    fi
}
cleanup() {
    # bd-t38jb: FLYWHEEL_BIN normally lives INSIDE the export. Deleting the
    # export first made every teardown verb below "command not found" (all
    # swallowed by `|| true`), leaving registry rows and a live session.
    if [ -n "$SPAWNED_PIN" ] && [ "$KEEP" = 0 ]; then
        "$FLYWHEEL_BIN" retire "$SPAWNED_PIN" --project e2e-live-five --force-working >/dev/null 2>&1 || true
    fi
    if [ -n "$WAKE_PIN" ] && [ "$KEEP" = 0 ]; then
        "$FLYWHEEL_BIN" retire "$WAKE_PIN" --project e2e-live-five --force-working >/dev/null 2>&1 || true
    fi
    if [ "$KEEP" = 0 ]; then
        retire_throwaway_project "$PROJECT"
    fi
    if [ -n "$PLANE_SESSION" ] && [ "$KEEP" = 0 ]; then
        "$REX_BIN" kill "$PLANE_SESSION" >/dev/null 2>&1 || true
    fi
    # An `if` (not `&&`) keeps cleanup's exit status 0 when a selftest has no
    # export — otherwise the EXIT trap flips the script's exit code.
    if [ -n "${EXPORT_ROOT:-}" ]; then rm -rf "$EXPORT_ROOT"; fi
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

prepare_export "$ROOT"
check_target_link "$TEST_REPO" || exit 2
export CARGO_TARGET_DIR="${E2E_LIVE_FIVE_TARGET_DIR:-$EXPORT_ROOT/target}"
if [ -z "$FLYWHEEL_BIN" ]; then
    (cd "$TEST_REPO" && "$CARGO_BIN" build -p flywheel --bin flywheel) \
        || die "cannot build flywheel from the export of $PROVEN_SHA"
    FLYWHEEL_BIN="$CARGO_TARGET_DIR/debug/flywheel"
fi
echo "e2e-live-five: proving $PROVEN_SHA (clean export $TEST_REPO)"
[ -x "$FLYWHEEL_BIN" ] || die "flywheel binary missing at $FLYWHEEL_BIN"
[ -n "$TORON_BIN" ] || die "TORON_BIN not found; name the toron binary"

export FLYWHEEL_LIVE_E2E=1
export FLYWHEEL_LIVE_SESSION="$SESSION_ID"
export FLYWHEEL_LIVE_BLOCK="$BLOCK_ID"
export FLYWHEEL_LIVE_PIN="$PIN"
export FLYWHEEL_LIVE_PROJECT="$PROJECT"
export FLYWHEEL_LIVE_AS="$AS"
# bd-znczi: the events-delivery witness reads the doorbell's per-pin events
# journal and ring journal from the daemon state dir (toron-core's state_dir).
export FLYWHEEL_LIVE_STATE_DIR="${FLYWHEEL_LIVE_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/toron}"
export TORON_BIN

# bd-oo7zi: the operator-typing guard needs a REAL harness pane: a bare zsh box
# classifies as Unknown, which may_inject allows. Spawn one in THIS project only,
# read its pin + pane_id from swarm status, and retire it in cleanup.
# bd-znczi/rrlo2: a grok seat rides the no-door events pump; spawn it in the
# SAME command so its registry row and token exist before the doorbell arms the
# pump (the pump reads the registry once at daemon start).
# bd-live-wake-pane-isolation: two opencode seats in the SAME spawn/session —
# the guard leg and the wake leg must never share a pane. A second spawn with
# --retire-orphans would close the first seat's workspace, so they are spawned
# together and separated by pin (grok is for the no-door events witness).
spawn_out="$("$FLYWHEEL_BIN" spawn e2e-live-five --oc 2 --gk 1 --model harness-default --escape --yes --retire-orphans 2>&1)" \
    || die "flywheel spawn failed: $spawn_out"
seated="$(FLYWHEEL_BIN="$FLYWHEEL_BIN" "$PYTHON_BIN" - <<'PY'
import json, os, subprocess
doc = json.loads(subprocess.check_output([
    os.environ["FLYWHEEL_BIN"], "swarm", "status", "e2e-live-five", "--json"
], text=True))
rows = doc.get("seats", doc if isinstance(doc, list) else [])
oc = [r for r in rows if str(r.get("harness", "")).lower() == "opencode" and r.get("pin") and r.get("pane_id")]
if len(oc) < 2:
    raise SystemExit(f"need two live opencode seats, got {len(oc)}")
for r in oc[:2]:
    print(f"{r['pin']}\t{r['pane_id']}")
PY
)" || die "cannot read the two opencode seats from swarm status"
SPAWNED_PIN="$(printf '%s\n' "$seated" | sed -n '1p' | cut -f1)"
SPAWNED_PANE="$(printf '%s\n' "$seated" | sed -n '1p' | cut -f2)"
WAKE_PIN="$(printf '%s\n' "$seated" | sed -n '2p' | cut -f1)"
WAKE_PANE="$(printf '%s\n' "$seated" | sed -n '2p' | cut -f2)"
SPAWNED_SESSION="$(printf '%s\n' "$spawn_out" | sed -nE 's/.*(session:[^ ]+) created.*/\1/p' | head -1)"
[ -n "$SPAWNED_PIN" ] || die "swarm status named no pin"
[ -n "$WAKE_PIN" ] || die "swarm status named no wake-leg pin"
[ -n "$SPAWNED_SESSION" ] || die "spawn did not name its workspace session"

# bd-znczi: the events witness needs a NO-DOOR seat — only those get a daemon
# events pump and an events-<pin>.jsonl journal row (pi/opencode ride their own
# MCP config instead). Find the grok seat spawned above.
GROK_PIN="$(FLYWHEEL_BIN="$FLYWHEEL_BIN" "$PYTHON_BIN" - <<'PY'
import json, os, subprocess
doc = json.loads(subprocess.check_output([
    os.environ["FLYWHEEL_BIN"], "swarm", "status", "e2e-live-five", "--json"
], text=True))
rows = doc.get("seats", doc if isinstance(doc, list) else [])
for r in rows:
    if str(r.get("harness", "")).lower() == "grok" and r.get("pin"):
        print(r["pin"])
        break
PY
)" || die "cannot read the grok seat from swarm status"
[ -n "$GROK_PIN" ] || die "swarm status named no grok seat"

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
    if [ "$t" = "tests/live_doorbell_operator_guard.rs" ]; then
        export FLYWHEEL_LIVE_SESSION="$SPAWNED_SESSION"
        export FLYWHEEL_LIVE_PIN="$SPAWNED_PIN"
        export FLYWHEEL_LIVE_BLOCK="$SPAWNED_PANE"
        export FLYWHEEL_LIVE_AS="$SPAWNED_PIN"
    elif [ "$t" = "tests/live_opencode_api_wake.rs" ]; then
        export FLYWHEEL_LIVE_SESSION="$SPAWNED_SESSION"
        export FLYWHEEL_LIVE_PIN="$WAKE_PIN"
        export FLYWHEEL_LIVE_BLOCK="$WAKE_PANE"
        export FLYWHEEL_LIVE_AS="$WAKE_PIN"
    else
        export FLYWHEEL_LIVE_SESSION="$SESSION_ID"
        export FLYWHEEL_LIVE_PIN="$PIN"
        export FLYWHEEL_LIVE_BLOCK="$BLOCK_ID"
        export FLYWHEEL_LIVE_AS="$AS"
    fi
    if [ "$t" = "tests/live_events_delivery.rs" ]; then
        export FLYWHEEL_LIVE_EVENTS_PIN="$GROK_PIN"
    else
        unset FLYWHEEL_LIVE_EVENTS_PIN
    fi
    if (cd "$TEST_REPO" && "$CARGO_BIN" test --test "$(basename "$t" .rs)" -- --ignored --nocapture >"$log" 2>&1); then
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
    printf '{"schema":"flywheel-e2e-live-five.v1","stamp":"%s","sha":"%s",' \
        "$STAMP" "$PROVEN_SHA"
    printf '"session":"%s","block":"%s","pin":"%s","project":"%s","as":"%s",' \
        "$SESSION" "$BLOCK" "$PIN" "$PROJECT" "$AS"
    printf '"passed":%d,"failed":%d,"results":[%s]}\n' \
        "$passed" "$failed" "${results%,}"
} >"$RECEIPT"

printf 'e2e-live-five: %d passed, %d failed — receipt %s\n' "$passed" "$failed" "$RECEIPT"
[ "$failed" -eq 0 ]
