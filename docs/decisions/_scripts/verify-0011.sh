#!/usr/bin/env bash
# verify-0011.sh — Wave A compliance gate for ADR-0011 (crew budget N)
# governed-by: ADR-0011
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pass=0; fail=0
ok(){ echo "  [OK]   $1"; pass=$((pass+1)); }
bad(){ echo "  [FAIL] $1"; fail=$((fail+1)); }

ADR=docs/decisions/0011-session-conductor-crew-budget-captain-fcfs-join-loud.md
echo "== ADR-0011 verification =="

[[ -f "$ADR" ]] && ok "ADR file exists" || bad "ADR file missing"
grep -qE '^status: accepted' "$ADR" && ok "frontmatter status: accepted" || bad "status not accepted"
grep -q '2026-09-08' "$ADR" && ok "date 2026-09-08 present" || bad "date missing"
grep -q 'crew.max' "$ADR" && ok "crew.max named" || bad "crew.max missing"

grep -q '0011-session-conductor-crew-budget-captain-fcfs-join-loud.md' docs/decisions/README.md \
  && ok "README indexes 0011" || bad "README missing 0011"
grep -E '0011.*Accepted|\*\*Accepted\*\*.*0011|0011.*\*\*Accepted\*\*' docs/decisions/README.md >/dev/null \
  && ok "README marks 0011 Accepted" || bad "README does not mark 0011 Accepted"

grep -q 'pub const DEFAULT_CREW_MAX: u32 = 5' src/crew.rs \
  && ok "DEFAULT_CREW_MAX = 5" || bad "DEFAULT_CREW_MAX missing"
grep -q 'pub fn pack_crew' src/crew.rs && ok "pack_crew exists" || bad "pack_crew missing"
grep -q 'fn bump_role' src/crew.rs && grep -A4 'fn bump_role' src/crew.rs | grep -q 'or_insert(1)' \
  && ok "unique-role infer (or_insert 1)" || bad "occurrence-count bump still present"
grep -q 'long)]' src/main.rs && grep -n 'crew strategy: auto | <N total panes>' src/main.rs >/dev/null \
  && ok "compose/run --crew speech is total N" || bad "--crew help not updated"

echo "== Wave B (briefing.v1) =="
grep -q 'flywheel-briefing.v1' src/briefing.rs && ok "briefing.v1 schema" || bad "briefing schema missing"
grep -q 'fn cmd_briefing' src/main.rs && ok "clap briefing" || bad "cmd_briefing missing"
grep -q 'pub fn topology_for_primitive' src/decide.rs && ok "decide emits topology" || bad "decide topology missing"
grep -q 'ARRIVE then BRIEF' integrations/prompts/swarm.md.tpl \
  && ok "slash /swarm starts arrive+briefing" || bad "swarm.tpl still pins wave-swarm first"
grep -q 'ARRIVE then BRIEF' integrations/prompts/loop.md.tpl \
  && ok "slash /loop starts arrive+briefing" || bad "loop.tpl missing arrive+briefing"
grep -q 'Arrive then brief' integrations/prompts/open.md.tpl \
  && ok "slash /open starts arrive+briefing" || bad "open.tpl missing arrive+briefing"

echo "== Wave C (join-loud / FCFS / --live) =="
grep -q 'CoalitionCmd::Join' src/main.rs && ok "coalition join CLI" || bad "coalition join missing"
grep -q 'join: bool' src/main.rs && ok "spawn --join" || bad "spawn --join missing"
grep -q 'fn cmd_arrive' src/main.rs && ok "arrive command" || bad "cmd_arrive missing"
grep -q 'watch", "--live"' src/main.rs && ok "doorbell watch --live" || bad "watch --live missing"
grep -q 'previous holder stale' src/captain_slot.rs && ok "FCFS stale claim line" || bad "FCFS claim line missing"
grep -q 'fn read_mail_counts' src/snapshot.rs && ok "helm mail not hardcoded empty" || bad "mail still hardcoded"
grep -q 'fn read_held_paths' src/snapshot.rs && ok "helm reservations not hardcoded empty" || bad "reservations still hardcoded"
grep -q 'JOINING — not spawning' src/briefing.rs && ok "join banner" || bad "join banner missing"

echo "== Wave D (rot / WaitCorr Sweep / nudge cap / SIGNOFF) =="
grep -q 'ADR-0011 D7: WaitCorr also drains on Sweep' src/main.rs \
  && ok "Sweep drain_waits=true" || bad "Sweep drain not wired"
grep -q 'pub fn interrupt_nudge' src/rot.rs && ok "nudge cap 3" || bad "interrupt_nudge missing"
grep -q 'pub fn auto_pass' src/signoff.rs && ok "SIGNOFF auto_pass predicate" || bad "auto_pass missing"
grep -q 'signoff::observe' src/pane.rs && ok "SIGNOFF auto-pass hooked" || bad "auto-pass not hooked in drainer"
grep -q 'LoopMsg::Rot' src/durable.rs && ok "rot scorer on tick" || bad "rot not on tick"

echo "== result: $pass ok, $fail fail =="
[[ "$fail" -eq 0 ]]
