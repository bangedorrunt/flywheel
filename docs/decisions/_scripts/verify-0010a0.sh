#!/usr/bin/env bash
# verify-0010a0.sh — ADR-0010 Wave A.0 (D0): one Model / one wake.
# Gate per ADR §13: poll_mail_digests deleted as a wake producer (rg tripwire);
# no live WorkspaceModel::default() in the swarm_loop body; the bd-3wzl guard
# assert survives. Fail-closed: any FAIL exits non-zero.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0010 Wave A.0 (D0) =="

# 1. poll_mail_digests is not a function anywhere (producer deleted).
if grep -rqE "fn poll_mail_digests" "$ROOT/src/"; then
  fail "poll_mail_digests still defined as a function (rg tripwire)"
else
  pass "poll_mail_digests not defined anywhere in src/"
fi

# 2. The honesty guard (only legal mention: the assert that the digest leaf
#    never runs in the live loop) survives in durable.rs.
grep -q "poll_mail_digests" "$ROOT/src/durable.rs" \
  && grep -q "live loop must not ctx.run the digest leaf" "$ROOT/src/durable.rs" \
  && pass "guard assert present (digest leaf may not run in live loop)" \
  || fail "bd-3wzl digest-leaf guard assert missing from durable.rs"

# 3. No WorkspaceModel::default() inside the swarm_loop body.
DEFAULTS_IN_LOOP="$(awk '/fn swarm_loop/,/^\}$/' "$ROOT/src/durable.rs" \
  | grep -c "WorkspaceModel::default()" || true)"
[ "$DEFAULTS_IN_LOOP" -eq 0 ] \
  && pass "no second live WorkspaceModel in swarm_loop body" \
  || fail "WorkspaceModel::default() constructed inside swarm_loop ($DEFAULTS_IN_LOOP sites)"

# 4. The allowed wrap still exists (tick_loop_owner is the single non-test
#    wrap site named by D0).
grep -q "fn tick_loop_owner" "$ROOT/src/durable.rs" \
  && pass "tick_loop_owner wrap present (durable.rs)" \
  || fail "tick_loop_owner missing from durable.rs"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
