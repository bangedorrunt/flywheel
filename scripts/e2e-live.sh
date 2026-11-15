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
CONTENTION_ONLY=0
case "${1:-}" in
  --mutate) MUTATE=1 ;;
  --contention) CONTENTION_ONLY=1 ;;
esac

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
  if [ -n "${E2E_KEEP:-}" ]; then
    echo "e2e-live: sandbox kept at $SANDBOX (E2E_KEEP)" >&2
    return 0
  fi
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
LEG_PROJECT="e2e-live-seat"
fail() { echo "  FAIL  $*" | tee -a "$LOG"; status=1; }

# Live hygiene (bd-yd74j): the leg must leave the operator's world
# byte-identical. Panes are split by the running rex server with ITS env, so a
# spawn that does not pin the caller's state roots writes live state from
# inside the pane. These snapshots are the assertion that it does not.
snapshot_live() { # path -> "rel<TAB>sha256" per file, sorted
  "$PYTHON_BIN" - "$1" <<'SNAP'
import hashlib, os, sys
root = sys.argv[1]
rows = []
if os.path.isfile(root):
    with open(root, "rb") as f:
        rows.append(f"{os.path.basename(root)}\t{hashlib.sha256(f.read()).hexdigest()}")
elif os.path.isdir(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            try:
                with open(path, "rb") as f:
                    rows.append(f"{rel}\t{hashlib.sha256(f.read()).hexdigest()}")
            except OSError as e:
                rows.append(f"{rel}\tunreadable:{e.errno}")
print("\n".join(rows))
SNAP
}

LIVE_SNAP="$SANDBOX/live-before"
LIVE_ROOTS=("$REAL_HOME/.local/state/flywheel" "$REAL_HOME/.config/flywheel")

# The fleet writes state continuously, so a whole-tree byte comparison would
# fail for reasons that have nothing to do with the leg. The assertions are
# therefore: (1) the two stable surfaces are byte-identical, and (2) nothing
# under the live roots mentions the sandbox project at all. The defect this
# gates (bd-yd74j) wrote project files and LaunchAgents, both of which these
# catch.
stable_surfaces() { # tag
  local tag="$1"
  snapshot_live "$REAL_HOME/Library/LaunchAgents" >"$LIVE_SNAP.$tag.agents"
  snapshot_live "$REAL_HOME/.local/state/flywheel/doorbell/subscriptions.json" \
    >"$LIVE_SNAP.$tag.subs"
}
# Leak guard for the MAILBOX, not just the filesystem (bd-0uzrq.7). The toron
# CLI resolves its source by precedence, and TORON_DATA_DIR alone does NOT keep
# it off a running daemon: the durable :18080 answers, and a leg then registers
# identities and claims leases in the OPERATOR store. The filesystem assertion
# below cannot see any of that. These are READ-ONLY calls against the operator
# daemon (never the store): the lease boards of the projects the legs use, and
# the registry row for every pin a leg can mint.
TORON_LIVE_URL="http://127.0.0.1:18080/mcp"
LEAK_PROJECTS=(e2e-contention e2e-live-seat e2e-idle-page probe)
LEAK_PINS=(QuartzTester OnyxTester CobaltRidge)

toron_live_snapshot() { # tag
  local tag="$1" p pin
  local out="$SANDBOX/toron-live.$tag"
  : >"$out"
  if ! ( exec 3<>"/dev/tcp/127.0.0.1/18080" ) >/dev/null 2>&1; then
    printf 'unreachable: no daemon on :18080\n' >>"$out"
    return 0
  fi
  for p in "${LEAK_PROJECTS[@]}"; do
    printf 'leases %s\n' "$p" >>"$out"
    TORON_MCP_URL="$TORON_LIVE_URL" "$TORON_BIN" reserve list --project "$p" --json 2>/dev/null |
      "$PYTHON_BIN" -c '
import json, sys
try:
    rows = json.load(sys.stdin).get("reservations") or []
except Exception:
    print("  unreadable"); raise SystemExit(0)
for r in sorted(rows, key=lambda r: (r.get("id") or "", r.get("path") or "")):
    print("  {} {} {} {}".format(r.get("id"), r.get("path"), r.get("holder"), r.get("reason")))
' >>"$out" 2>/dev/null || printf '  unreadable\n' >>"$out"
  done
  for p in "${LEAK_PROJECTS[@]}"; do
    for pin in "${LEAK_PINS[@]}"; do
      printf 'registry %s %s\n' "$p" "$pin" >>"$out"
      local row
      row="$(TORON_MCP_URL="$TORON_LIVE_URL" "$TORON_BIN" mail whois "$p" "$pin" --json 2>/dev/null |
        "$PYTHON_BIN" -c '
import json, sys
try:
    print(json.load(sys.stdin).get("npub") or "unnamed")
except Exception:
    raise SystemExit(1)
' || true)"
      printf '  %s\n' "${row:-absent}" >>"$out"
    done
  done
}

stable_surfaces before
toron_live_snapshot before

assert_live_unchanged() {
  stable_surfaces after
  toron_live_snapshot after
  local surface changed=0
  if grep -q '^unreachable' "$SANDBOX/toron-live.before"; then
    echo "e2e-live: note: no operator daemon on :18080, so the mailbox leak guard saw nothing" >&2
  elif ! cmp -s "$SANDBOX/toron-live.before" "$SANDBOX/toron-live.after"; then
    echo "e2e-live: the leg changed the operator MAILBOX (bd-0uzrq.7):" >&2
    diff -u "$SANDBOX/toron-live.before" "$SANDBOX/toron-live.after" 2>/dev/null | head -40 >&2 || true
    changed=1
  fi
  for surface in agents subs; do
    if ! cmp -s "$LIVE_SNAP.before.$surface" "$LIVE_SNAP.after.$surface"; then
      echo "e2e-live: the leg changed the operator's $surface (bd-yd74j):" >&2
      diff -u "$LIVE_SNAP.before.$surface" "$LIVE_SNAP.after.$surface" 2>/dev/null |
        head -40 >&2 || true
      changed=1
    fi
  done
  # A file NAMED for the project is the leak shape (inbox baselines, facts,
  # doorbell audit/state, LaunchAgents). Content is not scanned: live agents
  # discuss this project in their own journals, and that is not a leak.
  local root leaked=""
  for root in "${LIVE_ROOTS[@]}"; do
    [ -e "$root" ] || continue
    leaked="$leaked$(find "$root" -name "*$LEG_PROJECT*" 2>/dev/null || true)"
  done
  if [ -n "$leaked" ]; then
    echo "e2e-live: live state mentions $LEG_PROJECT (bd-yd74j):" >&2
    printf '%s\n' "$leaked" | head -20 >&2
    changed=1
  fi
  [ "$changed" -eq 0 ] ||
    fail "live state changed: a LaunchAgent, the subscription list, or a file naming $LEG_PROJECT under ~/.local/state/flywheel or ~/.config/flywheel"
}

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

if [ "$MUTATE" -eq 0 ] && [ "$CONTENTION_ONLY" -eq 0 ]; then
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
  stop_plane
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

# bd-iscat: the captain watch must not page for a pin that is merely idle — it
# pages on a working -> idle TRANSITION. This leg holds a real unread captain
# brief (and a real unrung deferral row) against a real, statically idle pin and
# requires the watch to stay armed: a false page here would page the captain on
# every poll. The transition half is the pure fold (unit + mutation proof);
# a sandbox has no authenticated harness to make a pane genuinely work.
idle_leg() {
  : "${AGENT_NAME:?set AGENT_NAME to your pin — the send refuses without a sender}"
  local project="e2e-idle-page"
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

  local pin
  pin="$("$FLYWHEEL_BIN" swarm status "$project" --json | "$PYTHON_BIN" -c '
import json, sys
print(json.load(sys.stdin)["seats"][0]["pin"])')"

  "$FLYWHEEL_BIN" mail send --project "$project" --to "$pin" \
    --subject "[e2e] brief nobody rang" --body "unread on purpose" >>"$LOG" 2>&1 ||
    { live_fail "sending the brief to $pin failed (log $LOG)"; return 1; }

  # A real pending deferral, in the dispatcher's own row shape and file name.
  local state="$XDG_STATE_HOME/flywheel"
  mkdir -p "$state"
  printf '{"pin":"%s","line":"[doorbell] plane-P mail. Pull: toron next"}\n' "$pin" \
    >>"$state/doorbell-deferred-$project.jsonl"

  : >"$SANDBOX/watch.out"
  "$FLYWHEEL_BIN" captain watch "$project" --interval 1 --json \
    >"$SANDBOX/watch.out" 2>>"$LOG" &
  local watch_pid=$!
  local waited=0
  while [ "$waited" -lt 6 ]; do
    if ! kill -0 "$watch_pid" 2>/dev/null; then
      kill "$watch_pid" 2>/dev/null
      wait "$watch_pid" 2>/dev/null
      live_fail "the watch paged on a statically idle pin: $(cat "$SANDBOX/watch.out")"
      return 1
    fi
    waited="$((waited + 1))"
    sleep 1
  done
  kill "$watch_pid" 2>/dev/null
  wait "$watch_pid" 2>/dev/null
  if [ -s "$SANDBOX/watch.out" ]; then
    live_fail "the watch printed a page while the pin never worked: $(cat "$SANDBOX/watch.out")"
    return 1
  fi
  echo "  PASS  idle-page ($pin idle with an unread brief and a deferral, no page)" | tee -a "$LOG"
  return 0
}

# bd-0uzrq.7: the peer-contention matrix, sandboxed. Four legs, each with the
# claim it makes and the claim it CANNOT make here:
#
#   L1 simultaneous claim — exactly one grant, and the loser is handed the
#      winner's pin AND its fencing epoch. Two PROCESSES at one instant, so this
#      is the cross-process case, not the same-task `join!` the unit test uses.
#   L2 dead holder — a lease whose holder stopped heartbeating expires, and the
#      next claim is granted without anyone force-releasing it.
#   L3 idle holder — a request-release against an idle pin is DELIVERED (it
#      rings), not queued behind a stale action.
#   L4 busy holder — the wake is DEFERRED into the one handoff and rung at the
#      pin's next at-prompt transition.
#
# L4's DEFER half needs a pane that is genuinely working, which a sandbox has no
# authenticated harness to produce (the limit bd-iscat already names). So L4
# proves the RING half live from a real deferral row in the dispatcher's own
# shape, and the defer half is the pure fold already RED-proven by
# `scripts/mutations/doorbell-deferral.spec.sh`.
contention_legs() {
  local project="e2e-contention"
  local p="$project"
  LIVE_PROJECT="$project"
  local rc=0

  if ! start_plane; then
    live_fail "the sandboxed toron daemon did not come up (log $SANDBOX/toron.log)"
    return 1
  fi
  export FLYWHEEL_TORON_URL="http://127.0.0.1:$PORT"
  # The toron CLI resolves its source by precedence, and TORON_DATA_DIR alone
  # does NOT keep it off a running daemon: the durable default :18080 answers,
  # and these legs would then claim leases in the OPERATOR store. TORON_MCP_URL
  # is the explicit daemon URL and outranks every other arm, so the legs hit
  # the sandboxed one. (Measured the hard way: a probe run put a lease in the
  # live store through exactly this gap.)
  export TORON_MCP_URL="http://127.0.0.1:$PORT/mcp"

  # --- L1: simultaneous claim ------------------------------------------------
  # A lease needs a registered identity: `reserve acquire` refuses an unknown
  # pin ("no local key"), which is the onboarding path bootstrap covers.
  # Names that exist nowhere else: BlueLake/RoseDune are REAL fleet pins, and a
  # pin is one key (ADR-0037), so re-registering one fails on a host that does
  # not hold its key.
  local holders=(QuartzTester OnyxTester)
  local pin
  for pin in "${holders[@]}"; do
    "$TORON_BIN" agent bootstrap --project "$p" --as "$pin" >>"$LOG" 2>&1 ||
      { live_fail "registering $pin failed (log $LOG)"; return 1; }
  done
  mkdir -p "$SANDBOX/l1"
  local barrier pids=()
  barrier="$(( $(date +%s) + 2 ))"
  for pin in "${holders[@]}"; do
    (
      while [ "$(date +%s)" -lt "$barrier" ]; do sleep 0.02; done
      "$TORON_BIN" reserve acquire --project "$p" --as "$pin" \
        --paths src/contended.rs --exclusive --reason e2e-l1 --json \
        >"$SANDBOX/l1/$pin.json" 2>>"$LOG"
    ) &
    pids+=("$!")
  done
  # Only these two: a bare `wait` would also wait for the sandboxed daemon.
  wait "${pids[@]}"
  if ! "$PYTHON_BIN" - "$SANDBOX/l1/${holders[0]}.json" "$SANDBOX/l1/${holders[1]}.json" <<'L1' >>"$LOG" 2>&1
import json, sys
rows = {}
for path in sys.argv[1:]:
    try:
        rows[path] = json.load(open(path))
    except Exception as e:
        print(f"unreadable acquire output {path}: {e}")
        raise SystemExit(1)
grants = {p: len(r.get("granted") or []) for p, r in rows.items()}
print(f"L1 grants {grants}")
if sum(grants.values()) != 1:
    print(f"L1 wants exactly ONE grant across two simultaneous claims, got {grants}")
    raise SystemExit(1)
loser = next(p for p, n in grants.items() if n == 0)
conflicts = rows[loser].get("conflicts") or []
print(f"L1 loser {loser} conflicts {conflicts}")
if not conflicts:
    print("L1 the loser was not told anything")
    raise SystemExit(1)
c = conflicts[0]
if not (c.get("holders") or []):
    print(f"L1 the refusal does not name the winner: {c}")
    raise SystemExit(1)
if len(c.get("epochs") or []) != len(c["holders"]):
    print(f"L1 the refusal does not carry one epoch per holder: {c}")
    raise SystemExit(1)
L1
  then
    live_fail "L1 simultaneous claim (log $LOG)"
    rc=1
  else
    echo "  PASS  L1 simultaneous claim (one grant, the loser names the winner and its epoch)" | tee -a "$LOG"
  fi

  # --- L2: dead holder ------------------------------------------------------
  # A hold is a heartbeat, not a deed: an EXPIRED lease is kept while its holder
  # is still live in the durable activity snapshot, and that window is
  # DEFAULT_RESERVATION_TTL_SECS (180s). A 1s TTL alone therefore does NOT
  # expire anything (measured: the next claim still conflicted at 4s). The leg
  # waits the window out rather than shrink a production constant for a test.
  mkdir -p "$SANDBOX/l2"
  "$TORON_BIN" reserve acquire --project "$p" --as "${holders[0]}" \
    --paths src/dead.rs --exclusive --ttl-secs 1 --reason e2e-l2 --json \
    >"$SANDBOX/l2/blue.json" 2>>"$LOG" || true
  sleep 185
  "$TORON_BIN" reserve acquire --project "$p" --as "${holders[1]}" \
    --paths src/dead.rs --exclusive --reason e2e-l2 --json \
    >"$SANDBOX/l2/rose.json" 2>>"$LOG" || true
  if ! "$PYTHON_BIN" - "$SANDBOX/l2/rose.json" <<'L2' >>"$LOG" 2>&1
import json, sys
r = json.load(open(sys.argv[1]))
print(f"L2 second claim: {r}")
if len(r.get("granted") or []) != 1:
    print("L2 the expired lease still blocks a new claim")
    raise SystemExit(1)
if r.get("conflicts"):
    print(f"L2 the expired lease was reported as a conflict: {r['conflicts']}")
    raise SystemExit(1)
L2
  then
    live_fail "L2 dead holder (log $LOG)"
    rc=1
  else
    echo "  PASS  L2 dead holder (1s lease + 185s of no heartbeat, the next claim granted)" | tee -a "$LOG"
  fi

  # --- L3/L4: one live seat, idle at a prompt -------------------------------
  local session="flywheel-$project"
  "$REX_BIN" kill "$session" >/dev/null 2>&1 || true
  "$FLYWHEEL_BIN" init "$project" >>"$LOG" 2>&1
  if ! "$FLYWHEEL_BIN" spawn "$project" --kind opencode=1 --skip-preflight --no-focus \
    --no-verify --retire-orphans >>"$LOG" 2>&1; then
    live_fail "spawn of $project failed (log $LOG)"
    return 1
  fi
  PLANE_SESSION="$session"

  local seat
  seat="$("$FLYWHEEL_BIN" swarm status "$project" --json | "$PYTHON_BIN" -c '
import json, sys
print(json.load(sys.stdin)["seats"][0]["pin"])')"

  # L3: a held path asked for while the holder is NOT at a prompt.
  # `request-release` is a sealed plane-P ask; against a pin that cannot take a
  # prompt it is DEFERRED into the one handoff, never dropped. That is the busy
  # arm of the matrix, live through the real CLI.
  local deferred="$XDG_STATE_HOME/flywheel/doorbell-deferred-$project.jsonl"
  rm -f "$deferred"
  "$TORON_BIN" agent bootstrap --project "$project" --as CobaltRidge >>"$LOG" 2>&1 ||
    { live_fail "registering the requester failed (log $LOG)"; rc=1; }
  "$TORON_BIN" reserve acquire --project "$project" --as "$seat" \
    --paths src/asked.rs --exclusive --reason e2e-l3 --json >>"$LOG" 2>&1 || true
  "$FLYWHEEL_BIN" peer request-release --project "$project" --path src/asked.rs \
    --bead e2e-l3 --as CobaltRidge "$seat" >>"$LOG" 2>&1 || true
  if [ -f "$deferred" ] && grep -qF "$seat" "$deferred" &&
    grep -qF "p2p:request-release" "$deferred"; then
    echo "  PASS  L3 busy holder (request-release to $seat deferred into the handoff, not dropped)" | tee -a "$LOG"
  else
    live_fail "L3 the ask to $seat was neither delivered nor deferred (row: $(cat "$deferred" 2>/dev/null | head -2))"
    rc=1
  fi

  # L4: that deferral is TAKEN by the owning host and journaled — a refusal,
  # not a silent drop. The RING needs an exact Idle verdict (ADR-0010 D3: an
  # Unknown presence never prompts) and a sandbox pane reports `unknown`, so
  # this leg stops at the handoff outcome. The at-prompt ring is the pure fold,
  # mutation-proven (`scripts/mutations/doorbell-deferral.spec.sh`).
  local journals="$XDG_STATE_HOME/flywheel/journals"
  local waited=0 taken=0 journaled=0
  while [ "$waited" -lt 60 ]; do
    if [ -s "$deferred" ]; then
      waited="$((waited + 1))"
      sleep 1
      continue
    fi
    taken=1
    if grep -qs "PromptRefused" "$journals"/doorbell-journal-*.jsonl 2>/dev/null; then
      journaled=1
      break
    fi
    waited="$((waited + 1))"
    sleep 1
  done
  if [ "$taken" -eq 1 ] && [ "$journaled" -eq 1 ]; then
    echo "  PASS  L4 deferred wake (taken by the host and journaled as PromptRefused for $seat)" | tee -a "$LOG"
  else
    live_fail "L4 the deferral was not taken and journaled within 60s (taken=$taken journaled=$journaled)"
    rc=1
  fi
  return "$rc"
}

if [ "$CONTENTION_ONLY" -eq 1 ]; then
  set +e
  contention_legs
  cont_rc=$?
  set -e
  [ "$cont_rc" -eq 0 ] || fail "contention legs failed (rc=$cont_rc, log $LOG)"
  if [ -s "$SANDBOX/denied.log" ]; then
    fail "a contention leg called a supervisor tool: $(cat "$SANDBOX/denied.log")"
  fi
  assert_live_unchanged
  echo "e2e-live: receipt $LOG"
  exit "$status"
fi

if [ "$MUTATE" -eq 0 ]; then
  echo "e2e-live: contention" | tee -a "$LOG"
  set +e
  contention_legs
  cont_rc=$?
  set -e
  if [ "$cont_rc" -ne 0 ]; then
    fail "contention legs failed (rc=$cont_rc, log $LOG)"
  fi
fi

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

if [ "$MUTATE" -eq 0 ]; then
  echo "e2e-live: idle-page" | tee -a "$LOG"
  set +e
  idle_leg
  idle_rc=$?
  set -e
  if [ "$idle_rc" -ne 0 ]; then
    fail "idle-page leg failed (rc=$idle_rc, log $LOG)"
  fi
fi

if [ -s "$SANDBOX/denied.log" ]; then
  fail "a leg called a supervisor tool: $(cat "$SANDBOX/denied.log")"
fi

assert_live_unchanged

echo "e2e-live: receipt $LOG"
exit "$status"
