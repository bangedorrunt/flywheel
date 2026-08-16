#!/bin/bash
# e2e-close-detect — the ADR-0019 close-detect tick, LIVE against a scratch
# workspace (bd-nee7 + bd-vsca).
#
# Drives the REAL loop close-detect code paths — the leaf commands the
# durable loop's tick invokes (`__close-check`, `__reopen-bead`,
# `__loop-run-verify`) — against a throwaway scratch project (temp dir +
# `.beads` + real git) with the embedded in-process toron mailbox isolated
# via TORON_MCP_DATA_DIR. No herdr, no swarm, no mocks: real flywheel
# binary, real br, real git, real mailbox.
#
# Stages:
#   1. red-nocommit   — worker closes with no gate row + no commit carrying
#                       the id -> no-commit -> reopen -> exactly one mail.
#   2. green-legal    — commit + command-verified gate row (recorded AFTER
#                       close, the loop's order) -> legal -> stays closed.
#   3. loop-verify-pos— loop-runnable VERIFY executes (exit 0); with the
#                       command-verified row the bead stays closed.
#   4. loop-verify-neg— failing VERIFY -> failed-head marker -> never
#                       re-verified -> reopen.
#   5. cargo-tests    — the always-on CI gates for both e2e files.
#
# The `closed -> closed` gated self-transition in the scratch policy is
# REQUIRED: the loop records verdicts AFTER the worker closes, and `br gate
# report` validates the transition from the CURRENT status.
#
# Requirements: flywheel (current build), br on PATH, git.
# Usage: scripts/e2e-close-detect.sh
# Exit 0 = all green; 1 = any FAIL.
set -u

FW="${FW:-flywheel}"
PASS=0
FAIL=0
START=$(date +%s)

say()  { echo "[$(date -u +%H:%M:%SZ)] $*"; }
pass() { PASS=$((PASS+1)); say "PASS: $*"; }
fail() { FAIL=$((FAIL+1)); say "FAIL: $*"; }

POLICY=$'workflow:\n  transitions:\n    open:\n      - closed\n    closed:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n    closed -> closed:\n      require_all:\n        - command-verified\n'

# fresh scratch project: git + beads + isolated mailbox/config. ABSOLUTE
# path (the leaves take the project dir as the project key).
scratch() {
  local tag="${1:-proj}"
  local base
  base=$(mktemp -d "/tmp/fw-e2e-close-detect-${tag}-XXXXXX")
  mkdir -p "$base/proj" "$base/mailbox" "$base/cfg/flywheel"
  cat > "$base/cfg/flywheel/config.toml" <<'EOF'
[backend]
multiplexer = "herdr"
mail = "toron"

[agent_mail]
name = "e2e-planner"
EOF
  ( cd "$base/proj" && git init -q && br init >/dev/null 2>&1
    printf '%s' "$POLICY" > .beads/policy.yaml )
  echo "$base"
}

fwenv() {
  local base="$1"; shift
  XDG_CONFIG_HOME="$base/cfg" TORON_MCP_DATA_DIR="$base/mailbox" \
    FLYWHEEL_MAIL_NAME=e2e-planner "$@"
}

# create a P3 bead with a VERIFY fence; returns (base dir, bead id, sha)
bead() {
  local base="$1" tag="$2" verify="$3" with_commit="${4:-0}"
  local id
  id=$(cd "$base/proj" && br create --title "$tag" --slug "$tag" -p 3 \
    -d "work

## VERIFY
\`\`\`
$verify
\`\`\`" --json 2>/dev/null | jq -r .id)
  local created_by
  created_by=$(cd "$base/proj" && br show "$id" --json 2>/dev/null | jq -r '.[0].created_by')
  fwenv "$base" "$FW" mail register "$base/proj" --name e2e-planner >/dev/null 2>&1
  fwenv "$base" "$FW" mail register "$base/proj" --name "$created_by" >/dev/null 2>&1
  local sha=""
  if [ "$with_commit" = 1 ]; then
    ( cd "$base/proj" && echo "# $tag" > README.md && git add -A \
      && git -c user.email=t@t -c user.name=t commit -q -m "feat: $tag ($id)" )
    sha=$(cd "$base/proj" && git rev-parse HEAD)
  fi
  echo "$base|$id|$sha|$created_by"
}

mail_count() {
  local base="$1" id="$2" as="$3"
  fwenv "$base" "$FW" mail inbox "$base/proj" --as "$as" --limit 100 2>/dev/null \
    | jq -c '[.[] | select(.subject == "['"$id"'] unverified close reopened" and .thread_id == "'"$id"'")] | length'
}

status() {
  ( cd "$1" && br show "$2" --json 2>/dev/null | jq -r '.[0].status' )
}

# ---- stage 1: RED no-commit -------------------------------------------------
say "stage 1/5: red-nocommit — no gate row + no commit -> reopen + one mail"
B=$(scratch red-nocommit)
IFS='|' read -r BASE ID SHA BY < <(bead "$B" red-nocommit 'test -f README.md' 0)
( cd "$BASE/proj" && br close "$ID" --reason "worker says done" >/dev/null 2>&1 )
S=$(fwenv "$BASE" "$FW" __close-check "$BASE/proj" "$ID" 2>/dev/null | jq -r .status)
[ "$S" = "no-commit" ] && pass "close-check -> no-commit" || fail "close-check: got $S"
R=$(fwenv "$BASE" "$FW" __reopen-bead "$BASE/proj" "$ID" "" 2>/dev/null)
[ "$(echo "$R" | jq -r .reopened)" = "true" ] && pass "reopen ok" || fail "reopen: $R"
[ "$(status "$BASE/proj" "$ID")" = "open" ] && pass "status back to open" || fail "status: $(status "$BASE/proj" "$ID")"
C=$(mail_count "$BASE" "$ID" "$BY")
[ "$C" = "1" ] && pass "exactly one mail (got $C)" || fail "mail count: $C"
rm -rf "$BASE" 2>/dev/null || true

# ---- stage 2: GREEN legal ---------------------------------------------------
say "stage 2/5: green-legal — commit + gate row (after close) -> stays closed"
B=$(scratch green-legal)
IFS='|' read -r BASE ID SHA BY < <(bead "$B" green-legal 'test -f README.md' 1)
( cd "$BASE/proj" && br close "$ID" --reason "worker done" >/dev/null 2>&1 )
fwenv "$BASE" "$FW" __loop-run-verify "$BASE/proj" "$ID" --sha "$SHA" >/dev/null 2>&1
( cd "$BASE/proj" && br gate report "$ID" --gate command-verified \
  --provider flywheel-toron-loop1 --status pass \
  --note "sha=$SHA receipt=$BASE/proj/.flywheel/receipts/$ID-$SHA.command-verified.txt" >/dev/null 2>&1 )
S=$(fwenv "$BASE" "$FW" __close-check "$BASE/proj" "$ID" 2>/dev/null | jq -r .status)
[ "$S" = "legal" ] && pass "close-check -> legal" || fail "close-check: got $S"
[ "$(status "$BASE/proj" "$ID")" = "closed" ] && pass "stays closed" || fail "status: $(status "$BASE/proj" "$ID")"
[ ! -d "$BASE/proj/.flywheel/reopened" ] && pass "no reopen artifacts" || fail "reopened artifacts present"
rm -rf "$BASE" 2>/dev/null || true

# ---- stage 3: loop-verify positive -----------------------------------------
say "stage 3/5: loop-verify-pos — cheap band executes, verified close stays closed"
B=$(scratch loop-verify-pos)
IFS='|' read -r BASE ID SHA BY < <(bead "$B" loop-verify-pos 'test -f README.md' 1)
( cd "$BASE/proj" && br close "$ID" --reason "worker done" >/dev/null 2>&1 )
L=$(fwenv "$BASE" "$FW" __loop-run-verify "$BASE/proj" "$ID" --sha "$SHA" --timeout-secs 30 2>/dev/null)
[ "$(echo "$L" | jq -r .band)" = "loop-runnable" ] && pass "classified loop-runnable" || fail "band: $L"
[ "$(echo "$L" | jq -r .exit)" = "0" ] && pass "verify exit 0" || fail "exit: $L"
( cd "$BASE/proj" && br gate report "$ID" --gate command-verified \
  --provider flywheel-toron-loop1 --status pass \
  --note "sha=$SHA receipt=$BASE/proj/.flywheel/receipts/$ID-$SHA.command-verified.txt" >/dev/null 2>&1 )
S=$(fwenv "$BASE" "$FW" __close-check "$BASE/proj" "$ID" 2>/dev/null | jq -r .status)
[ "$S" = "legal" ] && pass "close-check -> legal (stays closed)" || fail "close-check: got $S"
rm -rf "$BASE" 2>/dev/null || true

# ---- stage 4: loop-verify negative -----------------------------------------
say "stage 4/5: loop-verify-neg — failing VERIFY -> marker -> reopen"
B=$(scratch loop-verify-neg)
IFS='|' read -r BASE ID SHA BY < <(bead "$B" loop-verify-neg 'test -f NO_SUCH_FILE_9cf0' 1)
( cd "$BASE/proj" && br close "$ID" --reason "worker done" >/dev/null 2>&1 )
L=$(fwenv "$BASE" "$FW" __loop-run-verify "$BASE/proj" "$ID" --sha "$SHA" 2>/dev/null)
[ "$(echo "$L" | jq -r .exit)" = "1" ] && pass "verify exit non-zero" || fail "exit: $L"
[ -f "$BASE/proj/.flywheel/loop-run-verify-failed/$ID-$SHA" ] && pass "failed-head marker written" || fail "marker missing"
L2=$(fwenv "$BASE" "$FW" __loop-run-verify "$BASE/proj" "$ID" --sha "$SHA" 2>/dev/null)
[ "$(echo "$L2" | jq -r .skipped)" = "failed-head" ] && pass "never re-verifies broken head" || fail "re-run: $L2"
R=$(fwenv "$BASE" "$FW" __reopen-bead "$BASE/proj" "$ID" "$SHA" 2>/dev/null)
[ "$(echo "$R" | jq -r .reopened)" = "true" ] && pass "reopened" || fail "reopen: $R"
rm -rf "$BASE" 2>/dev/null || true

# ---- stage 5: cargo gates ---------------------------------------------------
say "stage 5/5: cargo gates — both e2e files green"
FW_DIR="$(cd "$(dirname "$0")/.." && pwd)"
( cd "$FW_DIR" && cargo test -p flywheel --test e2e_verdict_reopen --test e2e_loop_run_verify >/tmp/fw-e2e-close-detect-tests.log 2>&1 )
if [ $? -eq 0 ]; then
  pass "cargo test --test e2e_verdict_reopen --test e2e_loop_run_verify"
else
  fail "cargo tests failed — see /tmp/fw-e2e-close-detect-tests.log"
  tail -20 /tmp/fw-e2e-close-detect-tests.log
fi

END=$(date +%s)
say "done in $((END-START))s — PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ] || exit 1
