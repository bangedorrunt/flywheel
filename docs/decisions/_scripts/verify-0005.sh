#!/usr/bin/env bash
# verify-0005.sh — compliance gate for ADR-0005 (always-on collab + doorbell)
# governed-by: ADR-0005
# Run from repo root. Exits 0 on pass, 1 on fail.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

pass=0; fail=0
ok()   { echo "  [OK]   $1"; pass=$((pass+1)); }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }

ADR=docs/decisions/0005-always-on-collab-baseline-and-command-surface.md
TPL=integrations/peer-aware.md.tpl
SWARM=integrations/prompts/swarm.md.tpl
LOOP=integrations/prompts/loop.md.tpl

echo "== ADR-0005 verification =="

if grep -q '^\*\*Status\*\* | Accepted' "$ADR" || grep -q '| \*\*Status\*\* | Accepted' "$ADR"; then
  ok "ADR-0005 status is Accepted"
else
  bad "ADR-0005 status is not Accepted"
fi

if grep -q '### D5 — Captain with and without a loop' "$ADR"; then
  ok "D5 captain modes present"
else
  bad "D5 captain with/without loop missing"
fi

for sc in SC-D1 SC-D2 SC-D3 SC-D4 SC-D5 SC-D6 SC-D7 SC-D8 SC-D9 SC-D10; do
  if grep -q "$sc" "$ADR"; then
    ok "scenario $sc listed"
  else
    bad "scenario $sc missing from ADR §9"
  fi
done

if [[ -f src/doorbell.rs ]]; then
  ok "src/doorbell.rs exists"
else
  bad "src/doorbell.rs missing"
fi

if grep -q 'mod doorbell;' src/main.rs; then
  ok "main.rs includes doorbell"
else
  bad "main.rs missing mod doorbell"
fi

if grep -q '\[doorbell\]' "$TPL"; then
  ok "peer-aware template is a doorbell handler"
else
  bad "peer-aware template missing [doorbell]"
fi

if grep -q 'every {poll_interval_minutes} minutes$' "$TPL" && grep -A1 'every {poll_interval_minutes} minutes$' "$TPL" | grep -q 'of idle'; then
  bad "peer-aware template still drives a 5-min idle poll"
else
  ok "peer-aware template is not a 5-min idle timer"
fi

if grep -q 'Loop is bead dispatch, not wake' "$SWARM" || grep -q 'no `loop run` required' "$SWARM"; then
  ok "swarm playbook: loop optional for peer wake"
else
  bad "swarm playbook still implies loop is required for peer wake"
fi

if grep -q 'inbox every 5 min' "$LOOP"; then
  bad "loop playbook still teaches 5-min inbox poll"
else
  ok "loop playbook is not a 5-min inbox timer"
fi

if grep -q 'SC-D9' tests/sc_doorbell_adr0005.rs && grep -q 'sc_d1_kind9_at_self_rings_channel' src/doorbell.rs; then
  ok "e2e + ring-rule tests named"
else
  bad "sc_doorbell_adr0005.rs or doorbell ring tests missing"
fi

if grep -q 'fn sc_d1_live_kind9_channel_post_rings' tests/sc_doorbell_adr0005.rs; then
  ok "live SC-D1 kind-9 channel post test present"
else
  bad "live SC-D1 test missing"
fi

if grep -q 'HearTracker' src/doorbell.rs && grep -q 'unanswered pin=' src/doorbell.rs; then
  ok "heard-or-deaf tracker present"
else
  bad "HearTracker / unanswered missing"
fi

if grep -q 'show you heard' integrations/peer-aware.md.tpl || grep -q 'FIRST, show you heard' integrations/peer-aware.md.tpl; then
  ok "handler template requires a heard signal"
else
  bad "handler template missing heard-or-deaf step"
fi

if grep -q 'doorbell::identity_prime' src/main.rs && grep -q 'fn identity_prime' src/doorbell.rs; then
  ok "identity prime injects without handler"
else
  bad "identity_prime missing (SC-D7)"
fi

if grep -q 'doorbell_sweep_inbox' src/main.rs && grep -q 'parse_inbox_item' src/doorbell.rs; then
  ok "plane P inbox --since sweep wired"
else
  bad "plane P fetch_inbox sweep missing"
fi

if grep -q 'fn take_allowed' src/doorbell.rs && grep -q 'take_allowed' src/main.rs; then
  ok "dispatcher enforces 5/30/3 budget"
else
  bad "PinBudget / take_allowed missing"
fi

if grep -q 'if !subscribed' src/main.rs; then
  ok "herdr poll only when subscribe is down"
else
  bad "dispatcher still polls herdr unconditionally"
fi

if grep -q 'dispatcher_status_line' src/doorbell.rs && grep -q '"dispatcher": dispatcher' src/main.rs; then
  ok "status + digest expose dispatcher up|down"
else
  bad "dispatcher liveness not on flywheel status"
fi

if grep -q 'peer-aware=off' "$SWARM" && grep -q -- '--no-peer-aware' "$SWARM"; then
  ok "swarm peer-aware=off maps to --no-peer-aware"
else
  bad "swarm missing peer-aware=off mapping"
fi

if grep -q 'poll inbox 5min' "$SWARM"; then
  bad "swarm playbook still teaches poll inbox 5min"
else
  ok "swarm brief is not a 5-min inbox poll"
fi

if grep -q '\$ARGUMENTS' integrations/prompts/frontmatter-grok.md.tpl; then
  bad "grok frontmatter claims \$ARGUMENTS"
else
  ok "grok frontmatter does not claim \$ARGUMENTS"
fi

if cargo test --offline --bin flywheel doorbell:: -- --quiet \
  && cargo test --offline --test sc_doorbell_adr0005 -- --quiet; then
  ok "cargo doorbell:: + sc_doorbell_adr0005"
else
  bad "cargo doorbell / sc_doorbell_adr0005 failed"
fi

echo
if [[ "$fail" -eq 0 ]]; then
  echo "RESULT: ADR-0005 verification PASSED ($pass checks)"
  exit 0
else
  echo "RESULT: ADR-0005 verification FAILED ($fail failed, $pass passed)"
  exit 1
fi
