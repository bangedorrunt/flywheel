#!/usr/bin/env bash
# verify-0010b.sh — ADR-0010 Wave B (D5, D6, D15): attention triage.
# D5 gate structurally includes D0 (one Model, one wake) — this script runs the
# D0 tripwire first. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0010 Wave B (D5/D6/D15) =="

# D0 gate (D5 acceptance line: classification is a lie while a second
# producer exists) — delegate to the A.0 gate.
if bash "$(dirname "$0")/verify-0010a0.sh" >/dev/null 2>&1; then
  pass "D0 gate green (verify-0010a0.sh)"
else
  fail "D0 gate red — D5 unfinished by definition (run verify-0010a0.sh)"
fi

# D5 — AFK digest mode + batch window knob.
"$ROOT/target/debug/flywheel" doorbell afk --help 2>/dev/null | grep -qi "digest" \
  && pass "D5 doorbell afk surface" \
  || { BIN="$(command -v flywheel || true)"; \
       if [ -n "$BIN" ] && "$BIN" doorbell afk --help 2>/dev/null | grep -qi "digest"; then
         pass "D5 doorbell afk surface (installed binary)"
       else
         fail "D5 doorbell afk surface missing"
       fi; }
grep -rq "FLYWHEEL_DIGEST_SECS" "$ROOT/src/" \
  && pass "D5 digest-window knob" \
  || fail "D5 FLYWHEEL_DIGEST_SECS missing"

# D6 — OPEN DECISIONS / RECORD DIVERGENCE folds on helm.
BIN="${BIN:-$(command -v flywheel || echo "$ROOT/target/debug/flywheel")}"
"$BIN" helm --help 2>/dev/null | grep -q "OPEN DECISIONS" \
  && "$BIN" helm --help 2>/dev/null | grep -q "RECORD DIVERGENCE" \
  && pass "D6 helm folds present" \
  || fail "D6 helm missing OPEN DECISIONS / RECORD DIVERGENCE sections"
grep -rq "open_human" "$ROOT/src/workspace.rs" \
  && pass "D6 captain-hold cache in tick model" \
  || fail "D6 open_human cache missing from workspace.rs"

# D15 — ask-user-authority: worker never self-answers (template contract).
grep -qiE "ask-user|captain's intent" "$ROOT/integrations/peer-aware.md.tpl" \
  && pass "D15 ask-user policy in peer-aware template" \
  || fail "D15 ask-user policy missing from peer-aware.md.tpl"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi
