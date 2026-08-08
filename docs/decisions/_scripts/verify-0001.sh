#!/usr/bin/env bash
# verify-0001.sh — compliance gate for ADR-0001 (Event-Driven Agent Collaboration Spine)
# governed-by: ADR-0001
# Run from repo root. Exits 0 on pass, 1 on fail.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

pass=0; fail=0
ok()   { echo "  [OK]   $1"; }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }
warn() { echo "  [WARN] $1"; }

echo "== ADR-0001 verification =="

# (a) protocol forward-compat: pin >= 19. The constant moved into the
# toron-core path dependency when flywheel's own src/herdr.rs was deleted
# (3f55c94); resolve the dir from Cargo.toml so the check stays honest.
TORON_CORE_DIR=$(sed -n 's/.*toron-core = { path = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)
if [ -n "$TORON_CORE_DIR" ] && grep -qE 'pub const HDR_PROTOCOL: u64 = (19|[2-9][0-9]);' "$TORON_CORE_DIR/src/backend/herdr.rs"; then
  ok "HDR_PROTOCOL >= 19"
else
  bad "HDR_PROTOCOL not >= 19 (toron-core pins $(grep -oE 'pub const HDR_PROTOCOL: u64 = [0-9]+' "$TORON_CORE_DIR/src/backend/herdr.rs" 2>/dev/null || echo '?'); live herdr reports 19)"
fi

# (b) register-on-spawn wired into the spawn path
if grep -rq 'register_agent' src/main.rs src/durable.rs 2>/dev/null; then
  ok "register_agent called in spawn path"
else
  bad "register_agent not called in spawn path"
fi

# (c) event-driven dispatch present: the loop BOTH subscribes to push events
# (__poll_events leaf, forward-only latency path) AND keeps the heartbeat
# __poll snapshot as the correctness floor. The old check grepped for the word
# 'subscribe' which appears in comments — this checks actual call sites.
if grep -q 'poll_events,' src/durable.rs && grep -q 'poll_swarm,' src/durable.rs; then
  ok "event wake + heartbeat poll both wired in swarm_loop"
else
  bad "swarm_loop missing poll_events wake or poll_swarm heartbeat"
fi

# (d) bead DAG acyclic
if br dep cycles --json 2>/dev/null | grep -q '"cycles":\[\]'; then
  ok "bead dependency graph is acyclic"
else
  bad "bead dependency cycle exists (run: br dep cycles)"
fi

# (e) no pane-identity-bridge gap: identity file written under herdr pane id
if grep -rq 'agent-mail/identity' src/ 2>/dev/null; then
  ok "pane-identity bridge writes identity file"
else
  bad "pane-identity bridge missing (ADR-0001 A4 gap)"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: ADR-0001 verification PASSED"
  exit 0
else
  echo "RESULT: ADR-0001 verification FAILED ($fail failure(s))"
  exit 1
fi
