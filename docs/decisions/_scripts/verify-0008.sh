#!/usr/bin/env bash
# verify-0008.sh — thin compliance gate for ADR-0008 (GS-024 Accept)
# Honesty: live is_compose_token = wave-swarm | repair-until-verified ONLY
# until beads-lifecycle follow-up lands.
# governed-by: ADR-0008
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pass=0; fail=0
ok(){ echo "  [OK]   $1"; pass=$((pass+1)); }
bad(){ echo "  [FAIL] $1"; fail=$((fail+1)); }

ADR=docs/decisions/0008-workflow-compose-topology-agent-optimal-mental-model.md
echo "== ADR-0008 verification =="

[[ -f "$ADR" ]] && ok "ADR file exists" || bad "ADR file missing"
grep -qE '^status: accepted' "$ADR" && ok "frontmatter status: accepted" || bad "status not accepted"
grep -q '2026-09-05' "$ADR" && ok "date 2026-09-05 present" || bad "date missing"
grep -q 'beads-lifecycle' "$ADR" && ok "beads-lifecycle design section present" || bad "beads-lifecycle missing from ADR"
grep -qi 'implement pending' "$ADR" && ok "implement pending honesty present" || bad "missing implement-pending honesty"

grep -q '0008-workflow-compose-topology-agent-optimal-mental-model.md' docs/decisions/README.md \
  && ok "README indexes 0008" || bad "README missing 0008"
grep -E '0008.*Accepted|\*\*Accepted\*\*.*0008|0008.*\*\*Accepted\*\*' docs/decisions/README.md >/dev/null \
  && ok "README marks 0008 Accepted" || bad "README does not mark 0008 Accepted"

# Live gate honesty — two tokens only until follow-up
if grep -n 'pub fn is_compose_token' -A3 src/builtin_workflows.rs | grep -q 'wave-swarm' \
  && grep -n 'pub fn is_compose_token' -A3 src/builtin_workflows.rs | grep -q 'repair-until-verified'; then
  ok "is_compose_token gates wave-swarm | repair-until-verified"
else
  bad "is_compose_token missing expected live tokens"
fi

# If beads-lifecycle appears in is_compose_token, ADR must no longer say implement pending
if grep -n 'pub fn is_compose_token' -A6 src/builtin_workflows.rs | grep -q 'beads-lifecycle'; then
  if grep -qi 'implement pending' "$ADR"; then
    bad "beads-lifecycle in is_compose_token but ADR still says implement pending (update ADR speech)"
  else
    ok "is_compose_token includes beads-lifecycle and ADR no longer says implement pending"
  fi
else
  ok "beads-lifecycle NOT in is_compose_token (implement pending — honest)"
fi

# Broken local flywheel 0027 path must not appear as if local
if grep -nE '\]\(0027-one-workspace-tick-control-plane\.md\)' "$ADR" >/dev/null; then
  bad "ADR has broken local flywheel link to 0027 (cite toron path)"
else
  ok "no broken local 0027 relative link"
fi

echo "== result: $pass ok, $fail fail =="
[[ "$fail" -eq 0 ]]
