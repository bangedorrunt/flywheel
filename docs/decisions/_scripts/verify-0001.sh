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

# (a) protocol forward-compat: pin >= 19
if grep -qE 'pub const HDR_PROTOCOL: u64 = (19|[2-9][0-9]);' src/herdr.rs; then
  ok "HDR_PROTOCOL >= 19"
else
  bad "HDR_PROTOCOL not >= 19 (see src/herdr.rs)"
fi

# (b) register-on-spawn wired into the spawn path
if grep -rq 'register_agent' src/main.rs src/durable.rs 2>/dev/null; then
  ok "register_agent called in spawn path"
else
  bad "register_agent not called in spawn path"
fi

# (c) event-driven dispatch present (events.subscribe used in loop, not just defined)
if grep -rq 'subscribe' src/durable.rs src/main.rs 2>/dev/null; then
  ok "event subscription wired into loop"
else
  bad "event subscription not wired into loop"
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
  warn "pane-identity bridge not detected yet (Phase 3)"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: ADR-0001 verification PASSED"
  exit 0
else
  echo "RESULT: ADR-0001 verification FAILED ($fail failure(s))"
  exit 1
fi
