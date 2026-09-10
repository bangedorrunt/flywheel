#!/usr/bin/env bash
# verify-0010c.sh — ADR-0010 Wave C (D7, D8, D9, D13): channel + authority.
# Structural + CLI surface probes per ADR §13. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0010 Wave C (D7/D8/D9/D13) =="

# D7 — control plane is verbs-only; operational marker exists.
BIN="${BIN:-$(command -v flywheel || echo "$ROOT/target/debug/flywheel")}"
"$BIN" crew control --help 2>/dev/null | grep -q "interrupt|exit|relaunch" \
  && pass "D7 crew control verbs-only surface" \
  || fail "D7 crew control missing allowlisted verbs"
grep -rq "FLYWHEEL_OP" "$ROOT/src/" \
  && pass "D7 FLYWHEEL_OP operational marker in src" \
  || fail "D7 FLYWHEEL_OP marker missing"

# D8 — degraded receipts + corr-preserving resend (toron side).
TORON_ROOT="$(cd "$ROOT/../toron" 2>/dev/null && pwd)"
if [ -n "$TORON_ROOT" ] && [ -f "$TORON_ROOT/crates/toron-core/src/mailbox/receipts.rs" ]; then
  grep -q "degraded" "$TORON_ROOT/crates/toron-core/src/mailbox/receipts.rs" \
    && pass "D8 degraded receipt state (toron receipts.rs)" \
    || fail "D8 degraded state missing from toron receipts.rs"
else
  fail "toron sibling not found for D8 probe"
fi
"$BIN" mail resend --help 2>/dev/null | grep -q -- "--corr" \
  && pass "D8 corr-preserving resend surface" \
  || fail "D8 mail resend --corr surface missing"

# D9 — structural done-hold tag (toron side).
if [ -n "${TORON_ROOT:-}" ] && [ -f "$TORON_ROOT/crates/toron-core/src/mailbox/send_tags.rs" ]; then
  grep -q -- "--resolve-bead" "$TORON_ROOT/crates/toron-core/src/mailbox/send_tags.rs" \
    && pass "D9 --resolve-bead in toron send_tags.rs" \
    || fail "D9 --resolve-bead missing from toron send_tags.rs"
else
  fail "toron sibling not found for D9 probe"
fi

# D13 — delegation-shaped deny standing orders (the hook is per-home
# untracked by design; the tracked defense-in-depth is the templates).
for tpl in open loop swarm; do
  grep -qiE "task|subagent|spawn|delegat" "$ROOT/integrations/prompts/$tpl.md.tpl" \
    && pass "D13 standing order in $tpl.md.tpl" \
    || fail "D13 standing order missing from $tpl.md.tpl"
done

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
