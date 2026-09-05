#!/usr/bin/env bash
# verify-0006.sh — compliance gate for ADR-0006 (parameterized stage payloads
# + wave-scoped dispatch + wave-swarm recipe)
# governed-by: ADR-0006
# Run from repo root. Exits 0 on pass, 1 on fail.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

pass=0; fail=0
ok()   { echo "  [OK]   $1"; pass=$((pass+1)); }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }

echo "== ADR-0006 verification =="

# §7 checkbox: unit-test entrypoints from the four landed beads. One cargo
# invocation per bead filter keeps a failure attributable to its lane.
echo "-- D1 stage params (bd-stage-params-pqo9) --"
if cargo test --offline stage_param >/dev/null 2>&1; then
  ok "D1 --param substitution + required-slot + golden tests pass (stage_param*)"
else
  bad "D1 stage_param tests fail"
fi

echo "-- D2 wave-scoped dispatch (bd-wave-scoped-dispatch-yj3d) --"
if cargo test --offline wave_scope >/dev/null 2>&1; then
  ok "D2 wave_scope fixtures pass (labeled/unlabeled/mixed/lower-wave-open/same-wave)"
else
  bad "D2 wave_scope fixtures fail"
fi

echo "-- D3 wave-swarm recipe (bd-wave-swarm-recipe-wsot) --"
if cargo test --offline wave_swarm >/dev/null 2>&1; then
  ok "D3 recipe expands GATE/DISPATCH/OBSERVE/VERIFY/SIGNOFF and lints clean"
else
  bad "D3 wave_swarm tests fail"
fi
if cargo test --offline verify_close_reason >/dev/null 2>&1; then
  ok "VERIFY-stage close-reason gate rejects missing tail / bare citations"
else
  bad "verify_close_reason fixtures fail"
fi

echo "-- compose CLI lint gate (--parts wave-swarm) --"
if cargo build --offline -q; then
  if ./target/debug/flywheel workflow compose "adr-0006 self-check" --parts wave-swarm \
      > /tmp/verify0006_compose.json 2>/dev/null; then
    ok "workflow compose --parts wave-swarm assembles + lints green"
  else
    bad "compose --parts wave-swarm failed lint"
  fi
  if grep -q '"planner"' /tmp/verify0006_compose.json \
     && grep -q '"scout"' /tmp/verify0006_compose.json \
     && grep -q '"reviewer"' /tmp/verify0006_compose.json; then
    ok "composed crew infers planner/scout/reviewer from the recipe"
  else
    bad "recipe crew inference incomplete"
  fi
  rm -f /tmp/verify0006_compose.json
else
  bad "cargo build failed"
fi

echo "-- SIGNOFF is the captain's gate (§7 --run --keep line) --"
echo "  CAPTAIN-GATE: run manually — flywheel workflow compose \"<fixture objective>\" --parts wave-swarm --run --keep wave-swarm --project <fixture-project> (needs herdr+mail panes; swarm authority = captain)"

echo "-- Skill Sync Contract (Commands enum compiles = ground truth; surfaces here) --"
if grep -q 'stage_params' src/main.rs && grep -q 'wave_dispatch' src/main.rs \
   && grep -q 'wave_swarm_compose_token' src/main.rs; then
  ok "robot_capabilities carries D1+D2+D3 entries"
else
  bad "robot_capabilities missing D1/D2/D3 entries in src/main.rs"
fi
if grep -q 'ADR-0006 D1' AGENTS.md && grep -q 'ADR-0006 D2' AGENTS.md \
   && grep -q 'ADR-0006 D3' AGENTS.md; then
  ok "AGENTS.md Critical overrides cover D1/D2/D3"
else
  bad "AGENTS.md missing an ADR-0006 override one-liner"
fi
SKILL="$HOME/.agents/skills/flywheel/SKILL.md"
if [[ -f "$SKILL" ]] && grep -q 'ADR-0006 D1' "$SKILL" \
   && grep -q -- '--wave-label' "$SKILL" && grep -q 'wave-swarm' "$SKILL"; then
  ok "SKILL.md commands/compose sections cover D1/D2/D3"
else
  bad "SKILL.md missing ADR-0006 surface ($SKILL)"
fi

echo
echo "== verify-0006: $pass passed, $fail failed =="
[[ $fail -eq 0 ]]
