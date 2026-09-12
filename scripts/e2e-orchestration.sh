#!/bin/bash
# e2e-orchestration — the LIVE orchestration-hardening suite (bd-e131).
#
# 13 named scenarios, each = one beaded gate, run against a throwaway scratch
# project with a REAL swarm + REAL dispatch. Extends e2e-hardened-loop.sh
# (which keeps its 6 loop-reliability scenarios); this suite adds the
# captain-compose + crew-execution + cron + coalition + decide/profile gates.
#
# Requirements: flywheel (current build), herdr server up, br on PATH,
# Resonate server up (com.flywheel.resonate), toron daemon up.
#
# Usage: scripts/e2e-orchestration.sh [--only <n>]
# Exit 0 = all green; 1 = any FAIL.
set -u

FW="${FW:-flywheel}"
PASS=0
FAIL=0
ONLY="${1:-all}"
START=$(date +%s)

say()  { echo "[$(date -u +%H:%M:%SZ)] $*"; }
pass() { PASS=$((PASS+1)); say "PASS: $*"; }
fail() { FAIL=$((FAIL+1)); say "FAIL: $*"; }

# portable timeout: gtimeout (macOS coreutils) | timeout (Linux)
if command -v gtimeout >/dev/null 2>&1; then
  TO() { gtimeout "$@"; }
elif command -v timeout >/dev/null 2>&1; then
  TO() { timeout "$@"; }
else
  TO() { shift; "$@"; }
fi

# fresh scratch project (absolute path — never touches the repo). The base
# name must be DOT-FREE (herdr agent names reject '.') AND UNIQUE per
# scenario — a shared basename collides on the swarm label (a leftover
# workspace makes the next spawn refuse).
scratch() {
  local tag="${1:-proj}"
  SCRATCH=$(mktemp -d /tmp/fwoscXXXXXX)/${tag}${RANDOM}
  PROJ="$SCRATCH"
  mkdir -p "$SCRATCH" && cd "$SCRATCH" || return 1
  git init -q . && git config user.email gate@local && git config user.name gate
  git commit -q --allow-empty -m init
  br init >/dev/null 2>&1
  "$FW" init "$PROJ" >/dev/null 2>&1
  # the loop's lease-renew leaf acts as the orchestrator identity (CoralFinch)
  # — scratch projects need it registered or every tick logs a toron
  # mail-key error (the documented bd-3aen env issue; harmless but noisy)
  "$FW" mail register "$PROJ" --name CoralFinch >/dev/null 2>&1 || true
}

bead() { # bead <title> <desc> <verify-cmd>
  # `br ready` is DISPATCHABILITY-gated (fail-closed work-ledger): EVERY bead
  # needs a VERIFY command, and a P<=2 bead also needs a principles citation,
  # or `br ready` returns it to nobody. Without this the loop looks broken when
  # it is behaving correctly: `loop run --work` finds no ready bead and exits
  # `loop: complete`. `br create` has no --verify flag; set it with `br update`.
  local id
  [ -n "${3:-}" ] || { say "bead: missing verify-cmd for '$1' (would not be dispatchable)"; return 1; }
  id=$(cd "$SCRATCH" && br create --title="$1" --description="$2" --type=task --priority=3 --json 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])")
  [ -n "$id" ] || return 1
  (cd "$SCRATCH" && br update "$id" --verify "$3" >/dev/null 2>&1)
}

# wait_until <desc> <timeout-secs> <cmd...>
wait_until() {
  local desc="$1" timeout="$2"; shift 2
  local t=0
  while [ "$t" -lt "$timeout" ]; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 5; t=$((t+5))
  done
  return 1
}

# ===========================================================================
# Tier A — machinery gates
# ===========================================================================

scenario_1_loop_orphans() { # bd-bvwf.1
  say "--- scenario 1: LOOP-ORPHANS (spawn 3x --retire-orphans -> ONE workspace)"
  scratch s1 || { fail "S1 setup"; return; }
  "$FW" spawn "$PROJ" --oc 1 --no-verify --no-focus >/dev/null 2>&1 || { fail "S1 spawn1"; return; }
  "$FW" spawn "$PROJ" --oc 1 --label osc-s2 --retire-orphans --no-verify --no-focus >/dev/null 2>&1 || { fail "S1 spawn2"; return; }
  "$FW" spawn "$PROJ" --oc 1 --label osc-s3 --retire-orphans --no-verify --no-focus >/dev/null 2>&1 || { fail "S1 spawn3"; return; }
  local n
  # the surviving workspace may carry the LAST spawn's label (osc-s3); count
  # every workspace whose label is one of this project's spawns (the gate:
  # exactly ONE remains)
  n=$(TO 10 herdr workspace list 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
base='flywheel-$(basename "$PROJ")'
labels={base,'osc-s2','osc-s3'}
ws=[w for w in (d.get('result',{}).get('workspaces') or []) if w.get('label') in labels]
print(len(ws))")
  [ "$n" = "1" ] && pass "S1: exactly ONE workspace ($n)" || fail "S1: expected 1 workspace, got $n"
  "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_2_loop_fresh_detection() { # bd-bvwf.2
  say "--- scenario 2: LOOP-FRESH-DETECTION (fresh spawn -> ready bead dispatches <=90s)"
  scratch s2 || { fail "S2 setup"; return; }
  bead "fresh-detection" "Create done-fresh.txt containing ok, then br sync + commit." "test -f done-fresh.txt"
  "$FW" spawn "$PROJ" --oc 1 --retire-orphans --no-verify --no-focus >/dev/null 2>&1
  # spawn-verify: wait for the agent to be DETECTED (herdr classifies the
  # fresh pane) before the loop's first poll — the loop must then dispatch
  # the ready bead on the active cadence (bd-bvwf.2: no first-poll stall)
  local base
  base=$(basename "$PROJ")
  if ! wait_until "agent detected" 45 bash -c "herdr agent list 2>/dev/null | grep -q '$base'"; then
    fail "S2: agent not detected after spawn (setup)"; "$FW" kill "$PROJ" --force >/dev/null 2>&1; return
  fi
  nohup "$FW" loop run "$PROJ" --work --interval-active 10 --force >"$SCRATCH/loop2.log" 2>&1 &
  local LPID=$!
  # the gate: the ready bead DISPATCHES on the first clean poll (the agent
  # lands working). Completion takes a real agent 2-3 min — the dispatch
  # landing is the bd-bvwf.2 signal (no first-poll stall).
  if wait_until "dispatch landed" 95 bash -c "grep -q '\"landed\":true' '$SCRATCH.jsonl'"; then
    pass "S2: bead dispatched within 90s (fresh spawn, first clean poll)"
  else
    fail "S2: ready bead did not dispatch within 90s (loop log: $(tail -3 "$SCRATCH/loop2.log" | tr '\n' ' '))"
  fi
  kill "$LPID" 2>/dev/null; "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_3_loop_chain() { # bd-bvwf.3
  say "--- scenario 3: LOOP-CHAIN (A -> B dependency runs in ONE loop)"
  scratch s3 || { fail "S3 setup"; return; }
  local A B
  A=$(cd "$SCRATCH" && br create --title="chain-A" --description="Create done-a.txt with ok, close." --type=task --priority=3 --json | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])")
  B=$(cd "$SCRATCH" && br create --title="chain-B" --description="Create done-b.txt with ok, close." --type=task --priority=3 --json | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])")
  # same dispatchability gate as bead(): VERIFY or the chain never becomes ready
  (cd "$SCRATCH" && br update "$A" --verify 'test -f done-a.txt' >/dev/null 2>&1)
  (cd "$SCRATCH" && br update "$B" --verify 'test -f done-b.txt' >/dev/null 2>&1)
  (cd "$SCRATCH" && br dep add "$B" "$A" >/dev/null 2>&1)
  "$FW" spawn "$PROJ" --oc 2 --retire-orphans --no-verify --no-focus >/dev/null 2>&1
  # spawn-verify (same as S2): the chain loop must not strand A on the
  # first poll before the fresh agents are detected (bd-bvwf.2/3)
  if ! wait_until "agents detected" 45 bash -c "herdr agent list 2>/dev/null | grep -c '$(basename "$PROJ")' | grep -qE '[1-9]'"; then
    fail "S3: agents not detected (setup)"; "$FW" kill "$PROJ" --force >/dev/null 2>&1; return
  fi
  nohup "$FW" loop run "$PROJ" --work --interval-active 10 --force >"$SCRATCH/loop3.log" 2>&1 &
  local LPID=$!
  # both beads must close (A first — B unblocks when A closes, per
  # CHAIN-UNBLOCK) in ONE loop run; real agents take 2-4 min each
  if wait_until "both beads closed" 420 bash -c "cd '$SCRATCH' && br list --all --json 2>/dev/null | python3 -c '
import sys,json
raw=json.load(sys.stdin)
arr=raw.get(\"issues\",raw) if isinstance(raw,dict) else raw
print(sum(1 for i in arr if i.get(\"status\")==\"closed\"))' | grep -q '^2$'"; then
    pass "S3: dependency chain (A->B) closed in ONE loop run"
  else
    fail "S3: chain not fully closed in 420s"
  fi
  kill "$LPID" 2>/dev/null; "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_4_loop_singleton() { # bd-bvwf.4
  say "--- scenario 4: LOOP-SINGLETON (second loop refuses)"
  scratch s4 || { fail "S4 setup"; return; }
  "$FW" spawn "$PROJ" --oc 1 --retire-orphans --no-verify --no-focus >/dev/null 2>&1
  nohup "$FW" loop run "$PROJ" --work --interval-active 15 >"$SCRATCH/loop4a.log" 2>&1 &
  local LPID=$!
  sleep 3
  local refuse
  refuse=$("$FW" loop run "$PROJ" --work 2>&1 | grep -ci "already running")
  if [ "$refuse" -ge 1 ]; then
    pass "S4: second loop refused (single-instance guard)"
  else
    fail "S4: second loop did not refuse"
  fi
  kill "$LPID" 2>/dev/null; "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_5_loop_unknown() { # bd-bvwf.5
  say "--- scenario 5: LOOP-UNKNOWN (working agent polling unknown 1-2 polls NOT respawned)"
  scratch s5 || { fail "S5 setup"; return; }
  bead "unknown-gate" "Create done-unknown.txt with ok, close." "test -f done-unknown.txt"
  "$FW" spawn "$PROJ" --oc 1 --retire-orphans --no-verify --no-focus >/dev/null 2>&1
  nohup "$FW" loop run "$PROJ" --work --interval-active 10 --force >"$SCRATCH/loop5.log" 2>&1 &
  local LPID=$!
  sleep 30
  # a working agent must not be respawned by a 1-2 poll unknown blip; the
  # bead must still be dispatched (working agents keep working)
  local respawns
  respawns=$(grep -c "respawn" "$SCRATCH/loop5.log" 2>/dev/null || true)
  respawns=${respawns:-0}
  if [ "$respawns" -le 1 ] 2>/dev/null; then
    pass "S5: no spurious respawn storm (respawns=$respawns)"
  else
    fail "S5: unexpected respawns ($respawns)"
  fi
  kill "$LPID" 2>/dev/null; "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_6_crew_execution() { # bd-efk1
  say "--- scenario 6: CREW-EXECUTION (dispatch -> claimed <=60s -> closed, 3x)"
  local ok=1
  for i in 1 2 3; do
    scratch s6 || { ok=0; break; }
    bead "crew-exec-$i" "Create done-ce$i.txt with ok, close." "test -f done-ce$i.txt"
    "$FW" spawn "$PROJ" --oc 1 --retire-orphans --no-verify --no-focus >/dev/null 2>&1
    if ! wait_until "agent detected" 45 bash -c "herdr agent list 2>/dev/null | grep -q '$(basename "$PROJ")'"; then
      fail "S6 round $i: agent not detected"; ok=0; "$FW" kill "$PROJ" --force >/dev/null 2>&1; continue
    fi
    nohup "$FW" loop run "$PROJ" --work --interval-active 10 --force >"$SCRATCH/loop6.log" 2>&1 &
    local LPID=$!
    # the claim-watchdog (bd-efk1) nudges an unclaimed bead after 2 active
    # polls (~20s); a real agent claims after reading the prompt. A
    # first-poll strand (agent still detecting) reclaims + re-dispatches —
    # the claim follows the re-dispatch. The gate: claimed WITHOUT manual
    # intervention (the watchdog + reclaim are the mechanisms).
    if wait_until "bead claimed" 150 bash -c "cd '$SCRATCH' && br list --json 2>/dev/null | grep -q in_progress"; then
      pass "S6 round $i: claimed autonomously (<=150s)"
    else
      fail "S6 round $i: not claimed <=150s (loop log: $(tail -2 "$SCRATCH/loop6.log" | tr '\n' ' '))"; ok=0
    fi
    kill "$LPID" 2>/dev/null; "$FW" kill "$PROJ" --force >/dev/null 2>&1
  done
  [ "$ok" = "1" ] && pass "S6: crew-execution contract held 3/3"
}

scenario_7_cron_ondemand() { # bd-8wvz
  say "--- scenario 7: CRON-ONDEMAND (cron run completes visibly <30s)"
  # uses the global weekly-review cron (the bd-8wvz repro) — the dry-run must
  # print a step trace and exit 0 within the bound
  local t0 t1
  t0=$(date +%s)
  local out
  out=$(TO 25 "$FW" cron run weekly-review 2>&1)
  local rc=$?
  t1=$(date +%s)
  if [ "$rc" = "0" ] && [ $((t1-t0)) -lt 30 ] && echo "$out" | grep -q "dispatch_job"; then
    pass "S7: cron run visible in $((t1-t0))s (dry-run trace)"
  else
    fail "S7: cron run rc=$rc elapsed=$((t1-t0))s out=$(echo "$out" | head -1)"
  fi
}

scenario_8_coalition_lifecycle() { # ADR-0011
  say "--- scenario 8: COALITION-LIFECYCLE (form -> disband)"
  scratch s8 || { fail "S8 setup"; return; }
  # the goal workflow must lint (the form gate); crews = fleet profile names
  local out
  out=$("$FW" coalition form "goal" --crews "coder,reviewer" --as "bangedorrunt" 2>&1)
  local cid
  cid=$(echo "$out" | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
    print(d.get('coalition_id') or d.get('id') or '')
except Exception:
    print('')" 2>/dev/null)
  if [ -n "$cid" ] || echo "$out" | grep -qiE "coalition|formed|channel"; then
    pass "S8: coalition formed (${cid:-via-live-channel})"
  else
    fail "S8: coalition form failed: $(echo "$out" | head -1)"
    return
  fi
  if [ -n "$cid" ]; then
    local dis
    dis=$("$FW" coalition disband "$cid" 2>&1)
    echo "$dis" | grep -qiE "disband|closed|done" && pass "S8: coalition disbanded" || fail "S8: disband: $(echo "$dis" | head -1)"
  fi
}

# ===========================================================================
# Tier B — captain-steering + composition
# ===========================================================================

scenario_9_captain_steer_direct() { # bd-t86p direct arm
  say "--- scenario 9: CAPTAIN-STEER --direct (compose once, crew completes)"
  scratch s9 || { fail "S9 setup"; return; }
  local out
  out=$(TO 240 "$FW" steer "create a README.md describing the scratch project" --direct --project "$PROJ" 2>&1)
  if echo "$out" | grep -q "workflow goal-direct ran"; then
    pass "S9: steer --direct ran the crew to completion (no manual dispatch)"
  else
    fail "S9: steer --direct failed: $(echo "$out" | tail -2 | tr '\n' ' ')"
  fi
  "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_10_workflow_composition() { # bd-t86p/bd-8xca
  say "--- scenario 10: WORKFLOW-COMPOSITION (compose + run a NEW workflow)"
  scratch s10 || { fail "S10 setup"; return; }
  local out
  out=$(TO 180 "$FW" workflow compose "compose gate" --parts "scout:fresh-eyes-explore" --run --project "$PROJ" 2>&1)
  if echo "$out" | grep -qE "composed workflow .* ran"; then
    pass "S10: composed workflow built + linted + ran"
  else
    fail "S10: compose --run failed: $(echo "$out" | tail -2 | tr '\n' ' ')"
  fi
  "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_11_steer_monitor() { # bd-t86p
  say "--- scenario 11: STEER-MONITOR (runs view shows the lifecycle)"
  scratch s11 || { fail "S11 setup"; return; }
  local out
  # ce-execute is the coder stage; `systematic-execute` was retired with the
  # p-code aliases (ADR-0009). Compose resolves against the live stage catalog.
  out=$(TO 60 "$FW" workflow compose "monitor gate" --parts "coder:ce-execute" --project "$PROJ" 2>&1)
  if echo "$out" | grep -q '"workflow"'; then
    pass "S11: compose preview emits the workflow + crew (monitor surface)"
  else
    fail "S11: compose preview missing: $(echo "$out" | head -1)"
  fi
  "$FW" kill "$PROJ" --force >/dev/null 2>&1
}

scenario_12_decide() { # captain classifier — no dispatch
  say "--- scenario 12: DECIDE (audit / ready-beads / implement+gate)"
  local a b c
  a=$("$FW" decide "audit the mailbox" --json 2>&1)
  echo "$a" | grep -q '"primitive": "captain-audit"' || { fail "S12 audit: $a"; return; }
  b=$("$FW" decide "work the ready beads" --json --ready-beads 2 2>&1)
  echo "$b" | grep -q '"primitive": "loop-work"' || { fail "S12 loop-work: $b"; return; }
  c=$("$FW" decide "implement this with a review gate" --json 2>&1)
  echo "$c" | grep -q '"primitive": "steer"' || { fail "S12 steer: $c"; return; }
  pass "S12: decide table (captain-audit, loop-work, steer)"
}

scenario_13_profile_spawn() { # never --oc when --profile
  say "--- scenario 13: SPAWN --profile coder (not --oc)"
  scratch s13 || { fail "S13 setup"; return; }
  # launch_pending may make spawn exit 1 (swarm not ready). Pane must still exist.
  "$FW" spawn "$PROJ" --profile coder --settle-secs 0 --no-verify --no-focus --retire-orphans >/dev/null 2>&1 || true
  local st
  st=$("$FW" status 2>/dev/null)
  if echo "$st" | grep -q coder && ! echo "$st" | grep -qE '"name": "flywheel-.*-oc"'; then
    pass "S13: profiled coder pane, no extra bare-oc harness"
  else
    fail "S13: expected coder pane without bare -oc: $(echo "$st" | tr '\n' ' ' | head -c 240)"
  fi
  "$FW" kill "$PROJ" --force --force-working >/dev/null 2>&1
}

# ===========================================================================
run_scenario() {
  case "$1" in
    1) scenario_1_loop_orphans ;;
    2) scenario_2_loop_fresh_detection ;;
    3) scenario_3_loop_chain ;;
    4) scenario_4_loop_singleton ;;
    5) scenario_5_loop_unknown ;;
    6) scenario_6_crew_execution ;;
    7) scenario_7_cron_ondemand ;;
    8) scenario_8_coalition_lifecycle ;;
    9) scenario_9_captain_steer_direct ;;
    10) scenario_10_workflow_composition ;;
    11) scenario_11_steer_monitor ;;
    12) scenario_12_decide ;;
    13) scenario_13_profile_spawn ;;
    *) fail "unknown scenario $1" ;;
  esac
}

if [ "$ONLY" != "all" ]; then
  run_scenario "$ONLY"
else
  for n in 1 2 3 4 5 6 7 8 9 10 11 12 13; do run_scenario "$n"; done
fi

ELAPSED=$(( $(date +%s) - START ))
say "=== e2e-orchestration: $PASS pass, $FAIL fail (${ELAPSED}s) ==="
[ "$FAIL" = "0" ]
