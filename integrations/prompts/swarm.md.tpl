{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

/swarm-mode is a crew. The captain reads `flywheel-captain`. Each crew
agent reads `flywheel-crew`. One captain, up to 5 crew; the person's answer sets the shape.

## Playbook

0. ARRIVE then BRIEF. `flywheel arrive <project> --as <pin>` (helm, JOINING banner, FCFS captain, doorbell `--live`). Then
   `flywheel briefing "<objective>" --project <p> --json`; ask topology + N;
   `--commit` (`--yes`=defaults). Joinable world proposes `arrive`, never a second compose. `decide` is the only classifier: do not pin wave-swarm before briefing says swarm; steer on a multi-lane target is not an escape. One owner per lane. Confirm model routing per profile.

1. CLASSIFY + RUN: `flywheel workflow compose "<objective>"
   --parts wave-swarm --run --inputs '{"objective":"<target>"}'. Re-check the catalog; add lanes only for a declared multi-lane need. GATE->DISPATCH->EXECUTE->OBSERVE->VERIFY->SIGNOFF; parks at
   SIGNOFF, resolve via `flywheel workflow approve`. DISPATCH briefs
   `--stage ce-execute`, never raw. Bind `--inputs '{"captain_pin":"<pin>"}'` so escalations reach a live pin; ensure every spawned pin joined the steer channel (join-before-post).

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

5. BRIEF: lane dispatch authoritative. Peer wakes are signals, not tasks; peer-aware wake rides the doorbell. The loop is bead dispatch, not wake. Per lane: bead ids, files, pin, claim with the pin as
   assignee, `toron reserve conflicts` on the lane files first (narrow; overlap = re-plan/split, never wait), loop heartbeat (4min window), build slot, TDD RED first,
   commit-before-close with VERIFY tail, stage ONLY reserved paths, on
   [doorbell] pull then act, blocked >10m mail captain, TIMEBOX 90m
   partial report, prove-it-works. Every dispatch announces its vehicle first line; --raw for nudges only; no commits for ~2 polls -> nudge, third contact = finish or release.

9. REPORT: crews post status/commit/verify/deviations on lane threads.

6. CONFLICT: check `toron reserve conflicts` before edits; leases are
   180s heartbeats, so never wait on one. Soft: re-plan/split -> request-release -> respond -> escalate (captain LAST); hard: serialize/`with-lease` one edit; cross-sweep: flag.
   The shared crate is the real serializer: keep it compiling, announce
   red, land compile-green slices. never-block-on-the-human.

7. WAVE GATE: fresh-eyes verify -- suite+fmt+clippy AND live wiring.
   Close with evidence, then next wave. sequence-verifiable-units.

8. TEARDOWN: `toron reserve release-by-reason <bead>` (batch: `sweep`);
   `flywheel mail slot release <project> --slot cargo --as <pin>`;
   `flywheel kill <project> --force --force-working`; br sync; commit
   .beads.

## MATERIAL RECLASSIFY

Once pinned, re-run decide only on a material event (authoritative objective change, topology switch, new work-unit class, classifier miss, stage-chain exhaustion): `decide --json` + `workflow list --brief` + `stage list`, announce vehicle / `none: raw`. Sticky-open still applies.

## NO HARNESS DELEGATION

No harness-native delegation (agent/subagent/task/spawn/dispatch/worktree/delegate-shaped, e.g. Claude's Task): it bypasses the registry (doorbell, dispatcher, guards see nothing). Fence: `flywheel __subagent-guard <tool>` (exit 2 = deny); observe/stop and session-local todo stay legal; mcp__* (toron) allowed; crew panes exempt (`--role crew`). Dispatch via spawn / send / toron mail.

{{VERSION}} {{GENERATED}}
