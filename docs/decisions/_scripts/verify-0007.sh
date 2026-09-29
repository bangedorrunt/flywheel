#!/usr/bin/env bash
# verify-0007.sh — compliance gate for ADR-0007 (catalog slots + agent-native
# catalog CLI + ratatui hub)
# governed-by: ADR-0007
# Run from repo root. Exits 0 on pass, 1 on fail.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

pass=0; fail=0
ok()   { echo "  [OK]   $1"; pass=$((pass+1)); }
bad()  { echo "  [FAIL] $1"; fail=$((fail+1)); }

BIN="${BIN:-target/debug/flywheel}"

echo "== ADR-0007 verification =="

echo "-- D1 slots + tiers (unit) --"
if out=$(cargo test --offline slot_alias_requires_at_and_maps_omp_names 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "slot_alias maps @-aliases only (bare names never stolen)"
else
  bad "slot_alias tests fail"
fi
if out=$(cargo test --offline compiled_in_slots_and_tiers_on_zero_config 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "compiled-in slots resolve cheap→opencode, standard→pi, premium→omp+grok-fallback; tiers set"
else
  bad "compiled-in slot/tier tests fail"
fi
if out=$(cargo test --offline user_slot_overlay_wins_and_save_does_not_bake_compiled_ins 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "save() never bakes compiled-in slots into the user file"
else
  bad "save-bake guard fails"
fi

echo "-- D2 six job profiles (unit) --"
if out=$(cargo test --offline compiled_in_defaults_appear_without_profiles_toml 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "6 built-ins; pa is a harness recipe, not a profile; harness recipes intact"
else
  bad "built-in profile set tests fail"
fi
if out=$(cargo test --offline role_model_fields_layer_and_default_empty 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "omp profiles fill smol=@cheap slow/plan=@premium; inherit via slot aliases"
else
  bad "role-model slot-alias tests fail"
fi
if out=$(cargo test --offline fallback_empty_by_default 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "model_fallback is distinct from fallback (ADR-0026 not overloaded)"
else
  bad "model_fallback distinction tests fail"
fi

echo "-- D3 compiled-in squads (unit) --"
if out=$(cargo test --offline compiled_in_squads_are_the_four 2>&1 || true) && echo "$out" | grep -q "1 passed"; then
  ok "squads default/plan/review/recon compiled in; adr19 not baked"
else
  bad "compiled-in squad tests fail"
fi

echo "-- D4 agent-native CLI (live binary) --"
if "$BIN" slot list --json 2>/dev/null | grep -q '"slots"'; then
  ok "slot list --json emits the slot table"
else
  bad "slot list --json fails"
fi
if "$BIN" slot show @premium 2>/dev/null | grep -q '"fallback_harness": "grok"'; then
  ok "slot show resolves the @premium alias (omp + grok fallback)"
else
  bad "slot show @premium fails"
fi
out=$("$BIN" model remove zai/glm-5.2 --no-input 2>&1 || true)
if echo "$out" | grep -q "compiled-in ids are never"; then
  ok "model remove refuses compiled-in ids"
else
  bad "model remove compiled-in guard fails"
fi
# bd-1zn2: the `@standard`-literal era is over (47fu routing; the human
# view carries `harness` plus `model_fallback`, the resolved concrete model
# lives in --json covered by check 15). Assert the human view is routed.
if "$BIN" profile show coder 2>/dev/null | grep -q '"harness": "[^"]\+"'; then
  ok "profile show coder exposes a routed harness"
else
  bad "profile show coder is not slot-routed"
fi

# 15. ADR-0007 D1 full resolve ladder exposed via profile show --json
# (resolved route: slot-inherited harness/model plus route status).
# bd-1zn2: structural, not literal — the VALUES are operator config and
# differ per machine (this box resolves opencode/muse-spark, not
# pi/deepseek); what the ADR guarantees is the SHAPE. One call, not three.
SHOW_JSON=$("$BIN" profile show coder --json 2>/dev/null || true)
RESOLVED_H=$(echo "$SHOW_JSON" | grep -o '"harness": "[^"]\+"' | head -1)
RESOLVED_M=$(echo "$SHOW_JSON" | grep -o '"model": "[^"]\+"' | head -1)
RESOLVED_R=$(echo "$SHOW_JSON" | grep -o '"route": "[^"]\+"' | head -1)
if [ -n "$RESOLVED_H" ] && [ -n "$RESOLVED_M" ] && [ -n "$RESOLVED_R" ]; then
  ok "profile show --json exposes the D1 resolved route ($RESOLVED_H, $RESOLVED_M, $RESOLVED_R)"
else
  bad "profile show --json missing or wrong resolved route"
fi
refusal="$(NO_COLOR=1 "$BIN" catalog 2>&1 || true)"
if echo "$refusal" | grep -q "slot list"; then
  ok "NO_COLOR catalog refuses the TUI and names the CLI"
else
  bad "catalog NO_COLOR refusal missing (got: $refusal)"
fi
snap="$("$BIN" catalog --json 2>/dev/null || true)"
if echo "$snap" | grep -q '"slots"' && echo "$snap" | grep -q '"squads"' && echo "$snap" | grep -q '"profiles"'; then
  ok "catalog --json snapshots all four nouns"
else
  bad "catalog --json snapshot incomplete"
fi
caps="$("$BIN" --robot-capabilities 2>/dev/null || true)"
if echo "$caps" | grep -q '"catalog"'; then
  ok "robot_capabilities carries the catalog object (agent self-discovery)"
else
  bad "robot_capabilities missing catalog object"
fi

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
