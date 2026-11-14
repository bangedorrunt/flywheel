#!/usr/bin/env bash
# Sandboxed run of the final-review legs (bd-fbiza).
#
# Every leg runs with a throwaway HOME/XDG and deny-shims for launchctl and
# systemctl first on PATH, so a leg that boots the operator's real units is
# caught instead of obeyed. Every state dir flywheel and toron read is
# redirected. The live leg starts its own toron daemon on a spare port with its
# own TORON_DATA_DIR — :18080 / :18980 and the live store are refused outright.
#
# The rex server is NOT sandboxable by HOME (the CLI reaches the running
# server), so the live leg lives in one throwaway SESSION that it kills by label
# on entry and on exit. Nothing else in that server is touched.
#
# Toolchain: the binaries this script runs are resolved to their REAL absolute
# paths before HOME/XDG move, and the toolchain/mise state dirs are carried over
# from the caller. A sandboxed XDG would otherwise hide mise's trust store and
# every shim would die mid-run.
#
# Usage: bash scripts/e2e-live.sh [--mutate]
#   --mutate  run ONLY the live leg with FLYWHEEL_ALLOW_EXEC=0 and REQUIRE it
#             to fail: the seat must not move when the dry arm suppresses it.
# Exit: 0 all legs green, 1 a leg failed, 2 a tool or dir was missing.
set -euo pipefail
cd "$(dirname "$0")/.."

MUTATE=0
[ "${1:-}" = "--mutate" ] && MUTATE=1

REAL_HOME="$HOME"
die() { printf 'e2e-live: %s\n' "$*" >&2; exit 2; }

resolve_tool() { # name -> absolute real binary, searched without trusting shims
  local name="$1" found
  if found="$(command -v "$name" 2>/dev/null)" && [ -n "$found" ]; then
    printf '%s\n' "$found"
    return 0
  fi
  local dir
  for dir in "$REAL_HOME/.local/bin" "$REAL_HOME/.cargo/bin" /opt/homebrew/bin /usr/local/bin; do
    if [ -x "$dir/$name" ]; then
      printf '%s\n' "$dir/$name"
      return 0
    fi
  done
  for found in "$REAL_HOME/.local/share/mise/installs/"*/latest/*/"$name" \
    "$REAL_HOME/.local/share/mise/installs/"*/latest/"$name" \
    "$REAL_HOME/.local/share/mise/installs/"*/*/"$name"; do
    if [ -x "$found" ]; then
      printf '%s\n' "$found"
      return 0
    fi
  done
  return 1
}

CARGO_BIN="$(resolve_tool cargo)" || die "cargo not found (looked on PATH, ~/.cargo/bin, mise installs)"
TORON_BIN="$(resolve_tool toron)" || die "toron not found (looked on PATH, ~/.cargo/bin, mise installs)"
FLYWHEEL_BIN="$(resolve_tool flywheel)" || die "flywheel not found (looked on PATH, ~/.cargo/bin, mise installs)"
REX_BIN="$(resolve_tool rex)" || die "rex not found (looked on PATH, ~/.local/bin)"
if [ -x /usr/bin/python3 ]; then
  PYTHON_BIN=/usr/bin/python3
else
  PYTHON_BIN="$(resolve_tool python3)" || die "python3 not found (looked at /usr/bin, PATH)"
fi

# mbx is the house cargo wrapper (it keeps a build cache); plain cargo is the
# same command for this purpose because both repos pin rust-toolchain.toml.
if MBX_BIN="$(resolve_tool mbx)"; then
  CARGO_CMD=("$MBX_BIN" +nightly-aarch64-apple-darwin)
else
  CARGO_CMD=("$CARGO_BIN")
  echo "e2e-live: note: mbx not found — building with plain cargo (no build cache)" >&2
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="/tmp/e2e-live-${STAMP}.log"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/e2e-sandbox.XXXXXX")"
TORON_PID=""
PLANE_SESSION=""
LIVE_PROJECT=""

stop_plane() {
  if [ -n "${PLANE_SESSION:-}" ]; then
    "$REX_BIN" kill "$PLANE_SESSION" >/dev/null 2>&1
  fi
  PLANE_SESSION=""
  if [ -n "${TORON_PID:-}" ]; then
    kill "$TORON_PID" 2>/dev/null
    wait "$TORON_PID" 2>/dev/null
  fi
  TORON_PID=""
  return 0
}

# `flywheel spawn` starts a detached doorbell for the workspace; it outlives the
# script and keeps writing into the sandbox, so it is stopped before the sweep.
stop_doorbell() {
  if [ -n "${LIVE_PROJECT:-}" ] && [ -x "${FLYWHEEL_BIN:-}" ]; then
    "$FLYWHEEL_BIN" doorbell stop "$LIVE_PROJECT" >/dev/null 2>&1
  fi
  return 0
}

cleanup() {
  set +e
  stop_plane
  stop_doorbell
  local tries=0
  while [ "$tries" -lt 5 ]; do
    tries="$((tries + 1))"
    chmod -R u+w "$SANDBOX" 2>/dev/null
    rm -rf "$SANDBOX" 2>/dev/null
    [ -e "$SANDBOX" ] || break
    sleep 1
  done
  if [ -e "$SANDBOX" ]; then
    echo "e2e-live: WARNING sandbox survived cleanup: $SANDBOX" >&2
  fi
}

mkdir -p "$SANDBOX/bin" "$SANDBOX/home" "$SANDBOX/config" "$SANDBOX/state" \
  "$SANDBOX/data/toron" "$SANDBOX/workspace"
for tool in launchctl systemctl; do
  printf '#!/usr/bin/env bash\necho "denied: %s $*" >>"%s/denied.log"\nexit 1\n' \
    "$tool" "$SANDBOX" >"$SANDBOX/bin/$tool"
  chmod +x "$SANDBOX/bin/$tool"
done

# The toolchain and mise state stay the operator's (read-only for us): cargo's
# registry cache and mise's trust store are what make the shims resolvable at
# all inside the sandbox.
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
export MISE_DATA_DIR="${MISE_DATA_DIR:-$REAL_HOME/.local/share/mise}"
export MISE_STATE_DIR="${MISE_STATE_DIR:-$REAL_HOME/.local/state/mise}"
export MISE_CONFIG_DIR="${MISE_CONFIG_DIR:-$REAL_HOME/.config/mise}"
TOOL_PATH="$(dirname "$CARGO_BIN"):$(dirname "$TORON_BIN"):$(dirname "$FLYWHEEL_BIN"):$(dirname "$REX_BIN")"
export PATH="$SANDBOX/bin:$TOOL_PATH:$PATH:/usr/bin:/bin:/usr/sbin:/sbin"
export HOME="$SANDBOX/home"
export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_STATE_HOME="$SANDBOX/state"
export XDG_DATA_HOME="$SANDBOX/data"
export TORON_DATA_DIR="$SANDBOX/data/toron"
export CARGO_TARGET_DIR="$SANDBOX/target"
export FLYWHEEL_ROOT="$SANDBOX/workspace"
trap cleanup EXIT
: >"$LOG"

status=0
fail() { echo "  FAIL  $*" | tee -a "$LOG"; status=1; }

run_test_leg() { # name, dir, filter, --, cargo selector
  local name="$1" dir="$2" filter="$3"
  shift 3
  [ "${1:-}" = "--" ] && shift
  local selector=("$@")
  echo "e2e-live: $name" | tee -a "$LOG"
  set +e
  ( cd "$dir" && "${CARGO_CMD[@]}" test "${selector[@]}" "$filter" -- --exact "$filter" \
    >>"$LOG" 2>&1 )
  local rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    echo "  PASS  $name" | tee -a "$LOG"
  else
    fail "$name (rc=$rc, log $LOG)"
  fi
}

if [ "$MUTATE" -eq 0 ]; then
  run_test_leg witness ../toron wave_swarm_gate_result_timeout_is_a_journaled_typed_failure \
    -- -p toron-workflow --test durable_host
  run_test_leg polish ../toron beads_lifecycle_polish_stops_on_an_unchanged_digest \
    -- -p toron-workflow --test durable_host
  run_test_leg respawn . supervisor_host::tests::respawn_replays_one_effect_key_and_never_doubles_a_holder \
    -- -p flywheel --bin flywheel
fi

port_open() { # port -> 0 when something is listening
  ( exec 3<>"/dev/tcp/127.0.0.1/$1" ) >/dev/null 2>&1
}

pick_port() { # prints a spare loopback port, or fails with a reason
  local port tries=0
  while [ "$tries" -lt 64 ]; do
    port="$((20000 + RANDOM % 20000))"
    tries="$((tries + 1))"
    case "$port" in
      18080 | 18980) continue ;;
    esac
    port_open "$port" && continue
    printf '%s\n' "$port"
    return 0
  done
  echo "no spare loopback port found in 20000-39999 after 64 tries" >&2
  return 1
}

start_plane() {
  local picked
  if ! picked="$(pick_port)"; then
    return 1
  fi
  PORT="$picked"
  "$TORON_BIN" serve --mcp-http "127.0.0.1:$PORT" >"$SANDBOX/toron.log" 2>&1 &
  TORON_PID=$!
  local waited=0
  while [ "$waited" -lt 60 ]; do
    waited="$((waited + 1))"
    if port_open "$PORT"; then
      return 0
    fi
    if ! kill -0 "$TORON_PID" 2>/dev/null; then
      echo "the toron daemon exited before it listened (see $SANDBOX/toron.log)" >&2
      return 1
    fi
    sleep 1
  done
  echo "the toron daemon never listened on 127.0.0.1:$PORT within 60s" >&2
  return 1
}

panes_for() { # session, pin -> live panes labelled with the pin
  "$REX_BIN" block list -s "$1" 2>/dev/null | awk -v p="$2" 'NR > 1 && $4 == p' | wc -l | tr -d ' '
}

seat_field() { # project, pin, field
  "$FLYWHEEL_BIN" swarm status "$1" --json | "$PYTHON_BIN" -c '
import json, sys
pin, field = sys.argv[1], sys.argv[2]
seats = json.load(sys.stdin)["seats"]
row = next(s for s in seats if s["pin"] == pin)
print(row[field] or "")
' "$2" "$3"
}

live_fail() { echo "  FAIL  live: $*" | tee -a "$LOG"; }

live_leg() { # mutant (1 = suppress the seat arm after spawn)
  local mutant="$1"
  : "${AGENT_NAME:?set AGENT_NAME to your pin — spawn refuses without a sender}"
  local project="e2e-live-seat"
  local session="flywheel-$project"
  LIVE_PROJECT="$project"

  if ! start_plane; then
    live_fail "the sandboxed toron daemon did not come up (log $SANDBOX/toron.log)"
    return 1
  fi
  export FLYWHEEL_TORON_URL="http://127.0.0.1:$PORT"

  "$REX_BIN" kill "$session" >/dev/null 2>&1 || true
  "$FLYWHEEL_BIN" init "$project" >>"$LOG" 2>&1
  if ! "$FLYWHEEL_BIN" spawn "$project" --kind opencode=2 --skip-preflight --no-focus \
    --no-verify --retire-orphans >>"$LOG" 2>&1; then
    live_fail "spawn of $project failed (log $LOG)"
    return 1
  fi
  PLANE_SESSION="$session"

  local pin before after moved
  pin="$("$FLYWHEEL_BIN" swarm status "$project" --json | "$PYTHON_BIN" -c '
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
  "$FLYWHEEL_BIN" swarm apply "$project" --patch "$pin.harness=pi" --json >>"$LOG" 2>&1 ||
    apply_rc=$?
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
