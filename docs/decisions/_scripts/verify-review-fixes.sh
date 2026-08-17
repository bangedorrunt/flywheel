#!/usr/bin/env bash
# verify-review-fixes.sh — regression gate for the 2026-08-05 review-fix batch
# (bd-review-fix-epic-m0b, F1-F8). Each check maps to a fix that was shipped
# and must NOT silently regress. Run: bash docs/decisions/_scripts/verify-review-fixes.sh
# governed-by: ADR-0001, ADR-0002
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pass=0; fail=0
ok()  { echo "  [OK]   $1"; }
bad() { echo "  [FAIL] $1"; fail=$((fail+1)); }

echo "== review-fix regression gate (bd-review-fix-epic-m0b) =="

# F1: harvest_latest reads the reply BODY (search_messages must request it).
# Post-se2.3 the local MailBackend trait is deleted (ADR-0007 MailSurface
# consume) — the filter struct now lives in mail_shim.rs (MailSearchFilter)
# and the wire args in backend/wire.rs (search_args). Both must carry
# include_body_md; main.rs must request it (lines below).
grep -q 'include_body_md: bool' src/mail_shim.rs \
  && ok "F1: MailSearchFilter (mail_shim.rs) carries include_body_md" \
  || bad "F1: MailSearchFilter lost include_body_md"
grep -q 'include_body_md: bool' src/backend/wire.rs \
  && ok "F1: search_args (backend/wire.rs) carries include_body_md" \
  || bad "F1: search_args lost include_body_md"
grep -q 'body_md' src/main.rs \
  && ok "F1: harvest reads body_md (extract_latest_body)" \
  || bad "F1: harvest body extraction missing"
# bd-debate-harvest-dsww rewrote harvest_latest onto the inbox projection
# (search_messages no longer involved): the body request is now
# include_bodies: true on the InboxQuery. Scope the grep to the function
# so an unrelated include_bodies elsewhere cannot false-pass.
awk '/^fn harvest_latest\(/,/^}/' src/main.rs | grep -q 'include_bodies: true' \
  && ok "F1: harvest_latest requests bodies (inbox include_bodies)" \
  || bad "F1: harvest_latest no longer requests bodies"

# F2: plan-review gate is a real state machine (no dead phase, no never_loop)
grep -q 'fn plan_review_gate_loops_on_changes_then_verifies' src/main.rs \
  && ok "F2: gate-loop test present" \
  || bad "F2: gate-loop test missing"
if cargo clippy --workspace 2>&1 | grep -q 'never_loop'; then
  bad "F2: never_loop clippy error returned"
else
  ok "F2: no never_loop clippy error"
fi

# F3: xproj --to-project is used (routing context), not silently dropped
grep -q 'to_project_validated' src/main.rs \
  && ok "F3: xproj send uses to_project (validated, surfaced)" \
  || bad "F3: xproj to_project dropped again"

# F4: T3 escalation halts the loop (bounded)
grep -q 'on_blocked_tier_exit' src/durable.rs \
  && ok "F4: bounded-halt decision fn present" \
  || bad "F4: on_blocked_tier_exit missing"
grep -q 'blocked_t3_halts_loop_mail_mode_only' src/durable.rs \
  && ok "F4: T3-halt test present" \
  || bad "F4: T3-halt test missing"

# F5: xproj loop-triggered spawn is background (no focus steal). The spawn
# lives in cmd_internal_xproj_spawn; scope the grep to that function so a
# no_focus elsewhere cannot false-pass this gate.
awk '/^fn cmd_internal_xproj_spawn\(/,/^}/' src/main.rs | grep -q 'no_focus: true' \
  && ok "F5: xproj spawn passes no_focus=true" \
  || bad "F5: xproj spawn focus regression"

# F6: __send leaf routes through the factory (coded landing semantics).
# The method was renamed `agent_prompt_coded` -> `prompt_coded` in the
# toron-core Backend migration (3df3398); the gate greps follow the code.
grep -q 'backend\.prompt_coded' src/main.rs \
  && ok "F6: __send uses factory coded prompt (prompt_coded)" \
  || bad "F6: __send regressed to concrete client"
if grep -n 'Client::connect.*prompt_coded\|\.agent_prompt_coded\|\.prompt_coded' src/main.rs | grep -v 'backend\.prompt_coded' >/dev/null 2>&1; then
  bad "F6: coded prompt routed through a concrete client"
else
  ok "F6: coded prompt only via the factory trait"
fi
if grep -n 'Client::connect' src/durable.rs | grep -v '^[0-9]*:#' >/dev/null 2>&1; then
  bad "F6: concrete Client::connect in durable.rs"
else
  ok "F6: durable.rs stays backend-agnostic"
fi

# F7: debate/plan-review defaults resolve to spawned names
grep -q 'resolve_agent_default' src/main.rs \
  && ok "F7: default-agent resolution helper present" \
  || bad "F7: resolve_agent_default missing"

# F8: legacy dead code stays gone; clippy clean
if grep -rn 'track_unknown\|UnknownAction\|unknown_decision\|SelfHeal' src/ >/dev/null 2>&1; then
  bad "F8: legacy heal symbols returned"
else
  ok "F8: no legacy heal symbols"
fi
WARNINGS=$(cargo clippy --workspace 2>&1 | grep -cE '^warning: ' || true)
if [ "$WARNINGS" -eq 0 ]; then
  ok "F8: clippy --workspace 0 warnings"
else
  bad "F8: clippy --workspace has $WARNINGS warnings"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "RESULT: review-fix regression gate PASSED"
  exit 0
else
  echo "RESULT: review-fix regression gate FAILED ($fail)"
  exit 1
fi
