#!/usr/bin/env bash
# e2e-plane-parity.sh — the pane-plane parity gate (bd-ryst).
#
# One scenario list, run against whichever plane `FLYWHEEL_BACKEND` selects:
#
#   P1 spawn            a live pane exists and its pin is bound
#   P2 status + roster  the pin is listed AND routable (never `missing`)
#   P3 send             landed + receipt, and the agent RUNS the task (marker)
#   P4 mail             a live pin draws no false "may reach nobody" advisory
#   P5 steer --direct   the crew dispatches on this plane
#   P6 recover          a closed pane is REPLACED (new pane) and sends again
#   P7 kill             the workspace is gone (panes closed)
#
# Why a script and not `cargo test`: it spawns real agents and mutates a real
# workspace, like the other `e2e-*` operator doors.
#
# Usage:
#   scripts/e2e-plane-parity.sh                  # auto (rex primary on this box)
#   FW_BACKEND=herdr scripts/e2e-plane-parity.sh # same list, herdr baseline
#   scripts/e2e-plane-parity.sh --keep           # keep the scratch project
#
# Env: AGENT_NAME (required for sends), FW_BACKEND (auto|herdr|superlogical),
#      E2E_BOOT_SECS (default 240), E2E_SEND_MS (default 120000)
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

FW="${FW:-$ROOT/target/debug/flywheel}"
BACKEND="${FW_BACKEND:-auto}"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1
BOOT="${E2E_BOOT_SECS:-240}"
SEND_MS="${E2E_SEND_MS:-120000}"
MARK="REX-PARITY-OK"

PASS=0
FAIL=0
say() { printf '\n=== %s\n' "$*"; }
pass() { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL: %s\n' "$*"; }
die() { fail "$*"; echo "RESULT: FAIL"; exit 1; }

: "${AGENT_NAME:?set AGENT_NAME to your pin — flywheel send refuses without a sender}"

say "plane: FLYWHEEL_BACKEND=$BACKEND"
"$FW" backend list 2>/dev/null | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(" active:", [b["name"] for b in d["backends"] if b.get("active")])
' || true

BASE="${TMPDIR:-/tmp}/rexparity.$$"
PROJ="$BASE/$(date +%H%M%S)"
# Pin the plane for every flywheel child (spawn/status/send/steer/recover).
export FLYWHEEL_BACKEND="$BACKEND"
cleanup() {
  if [ "$KEEP" = 0 ] && [ -d "$BASE" ]; then rm -rf "$BASE"; fi
}
trap cleanup EXIT

say "scratch project $PROJ"
mkdir -p "$PROJ" && cd "$PROJ" || die "scratch mkdir"
git init -q . && git config user.email gate@local && git config user.name gate
git commit -q --allow-empty -m init
br init >/dev/null 2>&1
"$FW" init "$PROJ" >/dev/null 2>&1 || die "flywheel init"
# The sender must exist in THIS project (one pin, one key). A FRESH pin per
# run: reusing one key across scratch projects reuses the same producer seq
# range in the shared inbox index, and the child workflow's appends collide
# (`payload conflict on <npub>:40:0`) with the previous run's rows.
# Pins are AdjectiveNoun (no digits — the namespace refuses them), so a
# fresh identity per run is a random word pair.
CAPTAIN_ADJ=(Amber Azure Brisk Coral Dusty Ember Frosty Golden Hazy Ivory
              Lilac Misty Noble Ochre Plum Quiet Rustic Silver Tidal Umber)
CAPTAIN_NOUN=(Anchor Badger Cove Delta Fern Gorge Heron Inlet Juniper Kestrel
               Lantern Marlin Nectar Orchard Pebble Quarry Reef Sparrow Thicket)
CAPTAIN="${CAPTAIN_ADJ[$((RANDOM % ${#CAPTAIN_ADJ[@]}))]}${CAPTAIN_NOUN[$((RANDOM % ${#CAPTAIN_NOUN[@]}))]}"
toron agent bootstrap --project "$PROJ" --as "$CAPTAIN" >/dev/null 2>&1 \
  || die "bootstrap $CAPTAIN"
# The caller's pin only satisfies the send gate; every dispatch in THIS
# scratch project must act as the pin registered here, or steer/mail refuse
# `unarrived <caller>`.
export AGENT_NAME="$CAPTAIN"
export FLYWHEEL_MAIL_AS="$CAPTAIN"

say "P1 spawn"
SPAWN=$("$FW" spawn "$PROJ" --kind opencode=1 --yes --escape 2>&1) \
  || die "spawn failed: $SPAWN"
printf '%s\n' "$SPAWN" | grep -qE 'block:|:[pw][0-9]+' || die "spawn reported no pane"
pass "spawn created a pane"

status_json() { "$FW" status "$PROJ" 2>/dev/null; }
wait_pin() { # wait_pin <secs>
  local t=0
  while [ "$t" -lt "$1" ]; do
    local out
    out=$(status_json)
    if printf '%s' "$out" | python3 -c '
import json,sys
d=json.load(sys.stdin)
rows=[a for a in d.get("agents",[]) if a.get("observed_pin")]
sys.exit(0 if rows else 1)
' 2>/dev/null; then return 0; fi
    sleep 5; t=$((t + 5))
  done
  return 1
}
wait_pin 60 || die "no pin bound within 60s"
PIN=$(status_json | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(next(a["observed_pin"] for a in d["agents"] if a.get("observed_pin")))
')
WS=$(status_json | python3 -c 'import json,sys; print(json.load(sys.stdin)["workspace_id"])')
PANE=$(status_json | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(next(a["pane_id"] for a in d["agents"] if a.get("observed_pin")))
')
say "pin=$PIN pane=$PANE ws=$WS"
pass "status lists the pin with its pane"

say "P2 doorbell roster is plane-true (not 'missing')"
ROSTER=$("$FW" doorbell roster "$PROJ" --json 2>/dev/null)
STATE=$(printf '%s' "$ROSTER" | python3 -c '
import json,sys
pin=sys.argv[1]
rows=json.load(sys.stdin)
hit=[r for r in rows if r.get("pin")==pin]
print(hit[0]["state"] if hit else "absent")
' "$PIN")
[ "$STATE" = "missing" ] && die "roster says the live pin is MISSING (herdr-only liveness)"
[ "$STATE" = "absent" ] && die "roster does not list the pin at all"
pass "roster state=$STATE (pane is routable)"

say "P3 send: landed + receipt + the agent runs it"
# The pane reads idle as a bare shell BEFORE the TUI boots, so an idle check
# is not evidence the agent can take a prompt. A busy rex prompt is refused
# with nothing typed, so the honest shape is: try, and retry on refusal.
HEAD_SHA=$(git rev-parse --short HEAD)
OUT=""
landed=0
for _attempt in 1 2 3 4 5 6 7 8; do
  OUT=$("$FW" send "$PROJ" --to "$PIN" --raw \
    "Live parity test. Run 'git rev-parse --short HEAD' here, then reply with exactly: $MARK $HEAD_SHA" \
    --wait --timeout-ms "$SEND_MS" 2>&1)
  if printf '%s\n' "$OUT" | grep -q '"landed": true' \
     && printf '%s\n' "$OUT" | grep -q '"receipt": true'; then
    landed=1
    break
  fi
  sleep 30
done
[ "$landed" = 1 ] || die "send never landed with a receipt: $OUT"
pass "send landed with a receipt"

capture_pane() {
  if [ "$BACKEND" = "herdr" ]; then
    herdr agent read "$PANE" --lines 200 2>/dev/null
  else
    local session="flywheel-$(basename "$PROJ")"
    rex capture -s "$session" -b "${PANE#block:}" 2>/dev/null
  fi
}
t=0
seen=0
while [ "$t" -lt "$BOOT" ]; do
  if capture_pane | grep -q "$MARK $HEAD_SHA"; then seen=1; break; fi
  sleep 8; t=$((t + 8))
done
[ "$seen" = 1 ] || die "the agent never printed '$MARK $HEAD_SHA'"
pass "agent ran the task and printed the marker"

say "P4 mail to a live pin draws no false dead-pin advisory"
MAIL=$("$FW" mail send "$PROJ" --to "$PIN" --subject "parity probe" --body "no action" 2>&1)
printf '%s\n' "$MAIL" | grep -q "may reach nobody" \
  && die "false dead-pin advisory: $MAIL"
pass "mail send saw a live pane"

say "P5 steer --direct dispatches the crew on this plane"
STEER=$("$FW" steer "Report the short HEAD; write nothing." --direct \
  --project "$PROJ" --reuse "$WS" --yes --json 2>&1) \
  || die "steer failed: $STEER"
printf '%s' "$STEER" | python3 -c '
import json,sys
raw=sys.stdin.read()
i=raw.find("{")
assert i >= 0, "no JSON object in steer output"
json.JSONDecoder().raw_decode(raw[i:])
' || die "steer did not return JSON: $STEER"
# A steer can return JSON while its dispatch step failed (unarrived sender,
# refused prompt). That is not a dispatch.
# The gate owns the pane DISPATCH (build/review/ship). The goal driver's
# ideate/plan child DMs can fail on a toron outbox seq race
# ((toron outbox seq race)); that is not a pane-plane failure, so
# name it loudly instead of failing the parity claim on it.
DISPATCH=$(printf '%s' "$STEER" | python3 -c '
import json,sys
raw=sys.stdin.read(); i=raw.find("{")
d=json.JSONDecoder().raw_decode(raw[i:])[0]
out=d.get("output",{})
ok=all(
    out.get(step,{}).get("ok") is True
    and out.get(step,{}).get("send",{}).get("landed") is True
    for step in ("build","review","ship")
)
print("ok" if ok else "failed")
')
[ "$DISPATCH" = "ok" ] || die "steer pane dispatch failed: $STEER"
if printf '%s' "$STEER" | grep -q '"failed_step"'; then
  echo "WARN: goal-driver child steps failed (known toron outbox race, see (toron outbox seq race)) — pane dispatch is green"
fi
CREW=$(status_json | python3 -c '
import json,sys
d=json.load(sys.stdin)
print(len([a for a in d.get("agents",[]) if a.get("observed_pin")]))
')
[ "$CREW" -ge 2 ] || die "steer dispatched no crew (pins=$CREW)"
pass "steer dispatched $CREW pinned panes"

say "P6 recover replaces a closed pane"
VICTIM=$(status_json | python3 -c '
import json,sys
d=json.load(sys.stdin)
rows=[a for a in d.get("agents",[]) if a.get("observed_pin") and a.get("status")!="working"]
print(rows[-1]["pane_id"] + " " + rows[-1]["observed_pin"])
')
V_PANE=${VICTIM%% *}
V_PIN=${VICTIM##* }
if [ "$BACKEND" = "herdr" ]; then
  herdr pane close "$V_PANE" >/dev/null 2>&1 || die "herdr pane close"
else
  rex block close "${V_PANE#block:}" -s "flywheel-$(basename "$PROJ")" >/dev/null 2>&1 \
    || die "rex block close"
fi
sleep 4
REC=$("$FW" recover "$PROJ" 2>&1) || die "recover failed: $REC"
RECOVERED=$(printf '%s' "$REC" | python3 -c '
import json,sys
raw=sys.stdin.read(); i=raw.find("{")
print(json.JSONDecoder().raw_decode(raw[i:])[0]["recovered"])
' 2>/dev/null || echo 0)
[ "$RECOVERED" -ge 1 ] || die "recover recovered nothing: $REC"
# The registry row moves to the minted pane; the roster joins the registry
# with the plane, so it names the new pane even while the harness boots.
NEW_PANE=$("$FW" doorbell roster "$PROJ" --json 2>/dev/null | python3 -c '
import json,sys
pin=sys.argv[1]
rows=json.load(sys.stdin)
hit=[r for r in rows if r.get("pin")==pin]
print(hit[0]["pane"] if hit else "")
' "$V_PIN")
[ -n "$NEW_PANE" ] || die "registry lost the recovered pin $V_PIN"
[ "$NEW_PANE" = "$V_PANE" ] && die "registry still names the dead pane $V_PANE"
pass "recover minted $NEW_PANE for $V_PIN"

# Reachability is the real claim: a send must resolve and land on it. The
# recovered harness may still be booting, so retry while it settles.
SEND2=""
landed2=0
for _attempt in 1 2 3 4 5 6; do
  SEND2=$("$FW" send "$PROJ" --to "$V_PIN" --raw "Reply with exactly: RECOVER-PARITY-OK" \
    --wait --timeout-ms 90000 2>&1)
  if printf '%s\n' "$SEND2" | grep -q '"landed": true'; then
    landed2=1
    break
  fi
  sleep 25
done
[ "$landed2" = 1 ] || die "send to the recovered pane failed: $SEND2"
pass "send reaches the recovered pane"

say "P7 kill closes the workspace"
"$FW" kill "$PROJ" --force --force-working --reap-crews >/dev/null 2>&1 \
  || die "kill failed"
sleep 3
if status_json | python3 -c '
import json,sys
d=json.load(sys.stdin)
sys.exit(0 if any(a.get("pane_id") for a in d.get("agents",[])) else 1)
' 2>/dev/null; then
  die "workspace still reports panes after kill"
fi
pass "workspace closed"

echo
echo "RESULT: $PASS passed, $FAIL failed (plane $BACKEND)"
[ "$FAIL" = 0 ]
