{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

/swarm-mode: captain-led multi-crew execution. One captain, up to 5
crews; parallel lanes on disjoint files. Objective authoritative, every
lane verifiable.

## Playbook

0. ARRIVE then BRIEF. `flywheel arrive <project> --as <pin>` (helm,
   JOINING banner if a live fleet, FCFS captain, doorbell `--live`;
   `arrival_triple`: presence-publishes, role-binds, roster row). Then
   `flywheel briefing "<objective>" --project <p> --json`; ask topology + N;
   `--commit` (`--yes`=defaults). A joinable world proposes `arrive`, never a second
   compose. `decide` is the classifier leaf, no
   second selector: do NOT pin wave-swarm before briefing says swarm;
   steer on a multi-lane target is NOT an escape. Cross-repo: one owner
   per lane. Confirm model routing per profile (blank=default).

1. CLASSIFY + RUN: `flywheel workflow compose "<objective>"
   --parts wave-swarm --run --inputs '{"objective":"<target>"}'. Re-check the
   catalog each objective; add lanes only for a declared multi-lane
   need. GATE->DISPATCH->EXECUTE->OBSERVE->VERIFY->SIGNOFF; parks at
   SIGNOFF, resolve via `flywheel workflow approve`. DISPATCH briefs
   `--stage ce-execute`, never raw. Alias: `workflow run wave-swarm
   --execute`. Bind `--inputs '{"captain_pin":"<pin>"}'` so escalations
   reach a live pin; ensure the steer channel exists and every spawned
   pin joined it (join-before-post).

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit
   tracker. Waves ONE AT A TIME; epics are gates.

3. LANES: single-writer file ownership from bead SCOPE.
   one-file-at-a-time = handoff chain; disjoint = parallel. Default crew
   planner+coder+reviewer (cap 5). Remix `--crew role:n`.
   guard-the-context-window: bulk to subagents, summaries in main thread.

4. SPAWN (escape; un-waved ad-hoc lanes only): `flywheel spawn <proj>
   --profile <role> --count N --escape`; kind/count + --model/--provider
   need --escape. peer-aware=off -> --no-peer-aware. Escape forfeits
   SIGNOFF + artifacts: write the trail.

5. BRIEF: lane dispatch authoritative. Peer wakes are signals, not
   tasks; peer-aware wake rides the doorbell. The loop is bead dispatch,
   not wake. Per lane: bead ids, files, pin, claim with the pin as
   assignee, 4min reserve, build slot, TDD RED first,
   commit-before-close with VERIFY tail, stage ONLY reserved paths, on
   [doorbell] pull then act, blocked >10m mail captain, TIMEBOX 90m
   partial report, prove-it-works. Every dispatch ANNOUNCES its vehicle first
   line (stage name, compose token, or "none: raw"). --raw for
   nudges only. No commits for ~2 polls -> nudge; third contact = finish
   or release.

9. REPORT: crews post status/commit/verify/deviations on lane threads.

6. CONFLICT: soft -> request-release -> respond -> escalate, captain
   LAST; hard -> serialize; cross-sweep -> flag.
   never-block-on-the-human.

7. WAVE GATE: fresh-eyes verify -- suite+fmt+clippy AND live wiring.
   Close with evidence, then next wave. sequence-verifiable-units.

8. TEARDOWN: `toron reserve release --project <p> --agent <pin>`;
   `flywheel mail slot release <project> --slot cargo --as <pin>`;
   `flywheel kill <project> --force --force-working`; br sync; commit
   .beads.

## MATERIAL RECLASSIFY

Once pinned, do not re-run decide casually except on a material event
(AUTHORITATIVE objective change, explicit topology switch, new work-unit
class, classifier miss, stage-chain exhaustion): `decide --json` +
`workflow list --brief` + `stage list`, then announce vehicle / `none: raw`.
Sticky-open still applies to open sessions.

## NO HARNESS DELEGATION

No harness-native delegation tools (agent/subagent/task/spawn/dispatch/
worktree/delegate-shaped, e.g. Claude's Task): they bypass the registry
(doorbell, dispatcher, guards see nothing). Fence: `flywheel
__subagent-guard <tool>` (exit 2 = deny); observe/stop and session-local
todo stay legal; mcp__* (toron) always allowed; crew panes exempt
(`--role crew`). Dispatch via spawn / send / toron mail.

{{VERSION}} {{GENERATED}}
