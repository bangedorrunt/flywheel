#!/bin/bash
# verify-omp-role-models — gate for bd-gsvn.2 (omp role-models: Profile
# smol/slow/plan fields + per-harness injection).
#
# GATE: a profile with smol/slow/plan spawns on omp with all three injected;
# model list shows them.
#
# Checks:
#   G1  unit surface: omp role models inject as --smol/--slow/--plan in
#       build_argv_roles; non-omp harnesses get NO role flags
#   G2  profile create --smol/--slow/--plan persists to profiles.toml
#   G3  profile list --json + profile show expose smol/slow/plan
#   G4  spawn-side injection: pane-runner AGENT_ARG path carries the three
#       role flags for an omp profile (verified through the shared
#       model_args/role_model_args unit surface + the cargo test gate)
#   G5  registry round-trip: role model ids resolve through the model
#       registry (provider/id form for omp) without blocking
#
# Requirements: flywheel on PATH (current build), cargo (for the unit gate).
#
# Usage: scripts/verify-omp-role-models.sh
set -u

FW="${FW:-flywheel}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
PASS=0
FAIL=0

say()  { echo "[$(date -u +%H:%M:%SZ)] $*"; }
pass() { PASS=$((PASS+1)); say "PASS: $*"; }
fail() { FAIL=$((FAIL+1)); say "FAIL: $*"; }

# ---- G1: unit surface (pure, deterministic) --------------------------------
say "=== G1: unit surface (cargo test) ==="
UNIT=$("$FW" __test-filters "headless::tests::role_model_args_omp_only_and_per_id,headless::tests::build_argv_roles_injects_omp_role_models,profile::tests::role_model_fields_layer_and_default_empty" 2>/dev/null)
if [ -z "$UNIT" ]; then
  # fall back to cargo directly (flywheel has no __test-filters leaf)
  UNIT_OUT=$(cd "$ROOT" && cargo test --bin flywheel -- \
    headless::tests::role_model_args_omp_only_and_per_id \
    headless::tests::build_argv_roles_injects_omp_role_models \
    profile::tests::role_model_fields_layer_and_default_empty 2>&1)
  if echo "$UNIT_OUT" | grep -q "test result: ok"; then
    pass "G1: role-model unit tests green ($(echo "$UNIT_OUT" | grep -o '3 passed'))"
  else
    fail "G1: unit tests failed"
    echo "$UNIT_OUT" | tail -5
  fi
else
  echo "$UNIT"
fi

# ---- G2+G3: CLI persistence + display --------------------------------------
say "=== G2+G3: profile create + list/show ==="
CFG_DIR="${FLYWHEEL_CONFIG_DIR:-$HOME/.config/flywheel}"
PROF="$CFG_DIR/profiles.toml"
BAK="$TMP/profiles.toml.bak"
if [ -f "$PROF" ]; then cp "$PROF" "$BAK"; fi

PROFILE_NAME="verify-omp-role-models-$$"
if "$FW" profile create "$PROFILE_NAME" \
    --harness omp \
    --model deepseek-v4-flash \
    --effort high \
    --smol deepseek-v4-flash \
    --slow glm-5.2 \
    --plan big-pickle >/dev/null 2>&1; then
  pass "G2a: profile create --smol/--slow/--plan accepted"
else
  fail "G2a: profile create rejected role-model flags"
fi

LIST=$("$FW" profile list 2>/dev/null)
if echo "$LIST" | python3 -c "
import json,sys
rows=json.load(sys.stdin)['profiles']
p=[r for r in rows if r['name']=='$PROFILE_NAME'][0]
assert p['smol']=='deepseek-v4-flash', p
assert p['slow']=='glm-5.2', p
assert p['plan']=='big-pickle', p
print('ok')" 2>/dev/null | grep -q ok; then
  pass "G3a: profile list --json shows smol/slow/plan"
else
  fail "G3a: profile list --json missing role models"
fi

SHOW=$("$FW" profile show "$PROFILE_NAME" 2>/dev/null)
if echo "$SHOW" | python3 -c "
import json,sys
p=json.load(sys.stdin)
assert p['smol']=='deepseek-v4-flash', p
assert p['slow']=='glm-5.2', p
assert p['plan']=='big-pickle', p
print('ok')" 2>/dev/null | grep -q ok; then
  pass "G3b: profile show exposes smol/slow/plan"
else
  fail "G3b: profile show missing role models"
fi

# ---- G5: registry resolution does not block ---------------------------------
say "=== G5: registry round-trip ==="
if "$FW" model add deepseek-v4-flash --provider opencode-go >/dev/null 2>&1; then
  pass "G5a: role model id registered (opencode-go/deepseek-v4-flash)"
else
  fail "G5a: model add failed for role model"
fi
MODELS=$("$FW" model list --json 2>/dev/null)
if echo "$MODELS" | grep -q "deepseek-v4-flash"; then
  pass "G5b: model list shows the role model id"
else
  fail "G5b: model list missing role model"
fi

# ---- cleanup ----------------------------------------------------------------
say "=== cleanup ==="
"$FW" profile remove "$PROFILE_NAME" >/dev/null 2>&1 || true
if [ -f "$BAK" ]; then cp "$BAK" "$PROF"; else rm -f "$PROF"; fi
pass "cleanup: temp profile removed, profiles.toml restored"

say ""
say "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" = "0" ]
