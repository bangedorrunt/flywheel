#!/usr/bin/env bash
# Sandboxed run of the final-review legs (bd-fbiza).
#
# Every leg runs in a throwaway HOME/XDG with deny-shims for launchctl and
# systemctl first on PATH, so a leg that boots the operator's real units is
# caught instead of obeyed. Every state dir flywheel and toron read is
# redirected. The live leg starts its own toron daemon on a spare port with its
# own TORON_DATA_DIR — :18080 / :18980 and the live store are refused outright.
#
# The rex server is NOT sandboxable by HOME (the CLI reaches the running
# server), so the live leg lives in one throwaway SESSION that it kills by label
# on entry and on exit. Nothing else in that server is touched.
#
# Usage: bash scripts/e2e-live.sh [--mutate]
#   --mutate  run the live leg with FLYWHEEL_ALLOW_EXEC=0 and REQUIRE it to
#             fail: the seat must not move when the dry arm suppresses it.
# Exit: 0 all legs green, 1 a leg failed.
set -euo pipefail
cd "$(dirname "$0")/.."

MUTATE=0
[ "${1:-}" = "--mutate" ] && MUTATE=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="/tmp/e2e-live-${STAMP}.log"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/e2e-sandbox.XXXXXX")"
TORON_PID=""
PLANE_SESSION=""

stop_plane() {
  [ -n "$PLANE_SESSION" ] && rex kill "$PLANE_SESSION" >/dev/null 2>&1
  PLANE_SESSION=""
  [ -n "$TORON_PID" ] && kill "$TORON_PID" 2>/dev/null
  wait "$TORON_PID" 2>/dev/null
  TORON_PID=""
}

cleanup() {
  stop_plane
  chmod -R u+w "$SANDBOX" 2>/dev/null || true
  find "$SANDBOX" -delete 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$SANDBOX/bin" "$SANDBOX/home" "$SANDBOX/config" "$SANDBOX/state" \
  "$SANDBOX/data/toron" "$SANDBOX/workspace"
for tool in launchctl systemctl; do
  printf '#!/usr/bin/env bash\necho "denied: %s $*" >>"%s/denied.log"\nexit 1\n' \
    "$tool" "$SANDBOX" >"$SANDBOX/bin/$tool"
  chmod +x "$SANDBOX/bin/$tool"
done
export PATH="$SANDBOX/bin:$PATH"
export HOME="$SANDBOX/home"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export MBX_CMD="${MBX_CMD:-mbx +nightly-aarch64-apple-darwin}"
export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_STATE_HOME="$SANDBOX/state"
export XDG_DATA_HOME="$SANDBOX/data"
export TORON_DATA_DIR="$SANDBOX/data/toron"
export CARGO_TARGET_DIR="$SANDBOX/target"
export FLYWHEEL_ROOT="$SANDBOX/workspace"
: >"$LOG"

status=0
fail() { echo "  FAIL  $*" | tee -a "$LOG"; status=1; }

run_test_leg() { # name, dir, filter, cargo selector
  local name="$1" dir="$2" filter="$3" selector="$4"
  echo "e2e-live: $name" | tee -a "$LOG"
  set +e
  # shellcheck disable=SC2086  # the selector is a flag list on purpose
  ( cd "$dir" && $MBX_CMD test $selector "$filter" -- --exact "$filter" >>"$LOG" 2>&1 )
  local rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    echo "  PASS  $name" | tee -a "$LOG"
  else
    fail "$name (rc=$rc, log $LOG)"
  fi
}

run_test_leg witness ../toron wave_swarm_gate_result_timeout_is_a_journaled_typed_failure \
  "-p toron-workflow --test durable_host"
run_test_leg polish ../toron beads_lifecycle_polish_stops_on_an_unchanged_digest \
  "-p toron-workflow --test durable_host"
run_test_leg respawn . supervisor_host::tests::respawn_replays_one_effect_key_and_never_doubles_a_holder \
  "-p flywheel --bin flywheel"

spare_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

start_plane() {
  PORT="$(spare_port)"
  while [ "$PORT" = "18080" ] || [ "$PORT" = "18980" ] ||
    lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; do
    PORT="$((PORT + 1))"
  done
  toron serve --mcp-http "127.0.0.1:$PORT" >"$SANDBOX/toron.log" 2>&1 &
  TORON_PID=$!
  local i
  for i in $(seq 1 60); do
    if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$TORON_PID" 2>/dev/null || return 1
    sleep 1
  done
  return 1
}

live_fail() { echo "  FAIL  live: $*" | tee -a "$LOG"; }

panes_for() { # session, pin -> live panes labelled with the pin
  rex block list -s "$1" 2>/dev/null | awk -v p="$2" 'NR > 1 && $4 == p' | wc -l | tr -d ' '
}

seat_field() { # project, pin, field
  flywheel swarm status "$1" --json | python3 -c '
import json, sys
pin, field = sys.argv[1], sys.argv[2]
seats = json.load(sys.stdin)["seats"]
row = next(s for s in seats if s["pin"] == pin)
print(row[field] or "")
' "$2" "$3"
}

live_leg() { # mutant (1 = suppress the seat arm after spawn)
  local mutant="$1"
  : "${AGENT_NAME:?set AGENT_NAME to your pin — spawn refuses without a sender}"
  local project="e2e-live-seat"
  local session="flywheel-$project"

  if ! start_plane; then
    live_fail "the sandboxed toron daemon did not answer on 127.0.0.1:$PORT (log $SANDBOX/toron.log)"
    return 1
  fi
  export FLYWHEEL_TORON_URL="http://127.0.0.1:$PORT"

  rex kill "$session" >/dev/null 2>&1 || true
  flywheel init "$project" >>"$LOG" 2>&1
  if ! flywheel spawn "$project" --kind opencode=2 --skip-preflight --no-focus --no-verify \
    --retire-orphans >>"$LOG" 2>&1; then
    live_fail "spawn of $project failed (log $LOG)"
    return 1
  fi
  PLANE_SESSION="$session"

  local pin before after moved
  pin="$(flywheel swarm status "$project" --json | python3 -c '
import json, sys
print(json.load(sys.stdin)["seats"][0]["pin"])')"
  before="$(seat_field "$project" "$pin" pane_id)"
  if [ "$(panes_for "$session" "$pin")" != "1" ]; then
    live_fail "before the patch, $pin has $(panes_for "$session" "$pin") live panes, want 1"
    return 1
  fi

  [ "$mutant" -eq 1 ] && export FLYWHEEL_ALLOW_EXEC=0
  : >"$SANDBOX/samples"
  ( while :; do
      panes_for "$session" "$pin" >>"$SANDBOX/samples" 2>/dev/null || true
      sleep 0.2
    done ) &
  local sampler=$!
  local apply_rc=0
  flywheel swarm apply "$project" --patch "$pin.harness=pi" --json >>"$LOG" 2>&1 || apply_rc=$?
  kill "$sampler" 2>/dev/null
  wait "$sampler" 2>/dev/null

  after="$(seat_field "$project" "$pin" pane_id)"
  unset FLYWHEEL_ALLOW_EXEC
  moved=0
  [ "$after" != "$before" ] && moved=1
  local observed_max
  observed_max="$(sort -n "$SANDBOX/samples" | tail -1)"

  if [ "$apply_rc" -ne 0 ]; then
    live_fail "swarm apply failed (log $LOG)"
    return 1
  fi
  if [ "$moved" -ne 1 ]; then
    live_fail "$pin kept pane $before — the respawn did not land"
    return 1
  fi
  if [ "$(panes_for "$session" "$pin")" != "1" ]; then
    live_fail "$pin has $(panes_for "$session" "$pin") live panes after the respawn, want 1"
    return 1
  fi
  if [ -n "$observed_max" ] && [ "$observed_max" -gt 1 ]; then
    live_fail "the sampler saw $observed_max live panes for $pin during the respawn"
    return 1
  fi
  if [ "$(seat_field "$project" "$pin" harness)" != "pi" ]; then
    live_fail "$pin did not land the patched harness"
    return 1
  fi
  echo "  PASS  live ($pin $before -> $after, max panes $observed_max)" | tee -a "$LOG"
  return 0
}

echo "e2e-live: live$([ "$MUTATE" -eq 1 ] && echo ' --mutate')" | tee -a "$LOG"
set +e
live_leg "$MUTATE"
leg_rc=$?
set -e
if [ "$MUTATE" -eq 1 ]; then
  if [ "$leg_rc" -eq 0 ]; then
    fail "live mutant stayed green with the seat arm suppressed — the leg cannot fail"
  else
    echo "  PASS  live mutant (red as required, rc=$leg_rc)" | tee -a "$LOG"
  fi
elif [ "$leg_rc" -ne 0 ]; then
  fail "live leg failed (rc=$leg_rc, log $LOG)"
fi

if [ -s "$SANDBOX/denied.log" ]; then
  fail "a leg called a supervisor tool: $(cat "$SANDBOX/denied.log")"
fi

echo "e2e-live: receipt $LOG"
exit "$status"
