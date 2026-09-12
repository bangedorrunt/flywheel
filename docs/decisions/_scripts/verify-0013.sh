#!/usr/bin/env bash
# verify-0013.sh — compliance gate for ADR-0013 (agent-profile lifecycle and
# journaled fallback rebind: single peer-aware SoT, AgentRef/AgentBinding,
# journaled rebind, spawn/register/presence as effects).
# governed-by: ADR-0013
# Run from repo root. Exits 0 on pass, 1 on fail.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

pass=0; fail=0
ok()   { echo "  [OK]   $1"; pass=$((pass+1)); }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }

BIN="${BIN:-target/debug/flywheel}"

echo "== ADR-0013 verification =="

echo "-- D1 single peer-aware SoT + thin chrome --"
if out=$(bash scripts/check-peer-aware-sot.sh 2>&1 || true) && echo "$out" | grep -q "OK: peer-aware single SoT gate"; then
  ok "one spawn SoT body; prompts/ chrome stays thin and names it; ADR-0010 markers intact"
else
  bad "peer-aware SoT gate fails: $out"
fi

echo "-- D2 / A1 AgentRef identity freeze --"
if out=$(cargo test --offline npub_freezes_once_some 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "AgentRef::with_npub freezes once Some (a second bind never replaces identity)"
else
  bad "npub-freeze test fails"
fi
if out=$(cargo test --offline unbound_pin_fails_closed 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "an unbound pin fails closed after settle"
else
  bad "unbound-pin fail-closed test fails"
fi

echo "-- D3 / I4-I5 / D4 rebind journal + hop taxonomy --"
if out=$(cargo test --offline timeout_alone_never_allows_rebind 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "timeout / empty evidence never licenses a hop"
else
  bad "timeout-honesty test fails"
fi
if out=$(cargo test --offline fallback_empty_by_default 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "model_fallback[] stays distinct from fallback[] (model ids vs profile names)"
else
  bad "model_fallback distinction test fails"
fi

echo "-- D5 / D6 CLI surface --"
if "$BIN" profile set --help 2>/dev/null | grep -q -- "--model-fallback"; then
  ok "profile set authors fallback[] and model_fallback[] as distinct flags"
else
  bad "profile set --model-fallback missing"
fi
if "$BIN" binding-rebind --help 2>/dev/null | grep -q -- "--pin"; then
  ok "binding-rebind is the live-pane hop verb (distinct from profile set / rebind)"
else
  bad "binding-rebind CLI missing"
fi
if out=$(cargo test --offline binding_rebind_model_fails_loud_not_write_only_settled 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "--model on binding-rebind fails loud; no Settled for a write-only field"
else
  bad "fail-loud --model test fails"
fi

echo "-- D7 / C1 evidence redaction --"
if out=$(cargo test --offline effect_journal_redacts_evidence 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "effect-journal evidence is redacted before journal persist / eprintln"
else
  bad "effect-journal redaction test fails"
fi
if out=$(cargo test --offline record_reroute_audit_redacts_evidence 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "reroute audit redacts like journal_rebind"
else
  bad "reroute-audit redaction test fails"
fi

echo "-- D9 spawn / register / presence as effects --"
if out=$(cargo test --offline unresolved_apply_journals_unknown_not_settled 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "an apply with no evidence journals Unknown, never an invented Settled"
else
  bad "Unknown-not-Settled test fails"
fi
if out=$(cargo test --offline dropped_unresolved_guard_journals_failed 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "a spawn path that returns early journals Failed (no silent absence)"
else
  bad "guard-failure test fails"
fi
if out=$(cargo test --offline adr0013_spawn_effect_tests 2>&1 || true) && echo "$out" | grep -q "test result: ok"; then
  ok "spawn journals intent before dispatch; wake is separate evidence; register + presence per pin"
else
  bad "spawn-effect wiring tests fail: $out"
fi

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
