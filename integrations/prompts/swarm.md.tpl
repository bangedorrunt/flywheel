{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

Run /swarm-mode: captain-led multi-crew execution. One captain, up to
5 crews; parallel lanes on disjoint files. Objective authoritative,
each lane verifiable.

## Playbook

0. ARRIVE then BRIEF (ADR-0011). `flywheel arrive <project> --as <pin>`
   (helm, JOINING banner if a live fleet, FCFS captain, doorbell
   `--live`; `arrival_triple` presence-publishes, role-binds, roster
   row). Then `flywheel briefing "<objective>" --project <p> --json`;
   ask topology + N, `--commit` (`--yes` = defaults). Joinable world
   proposes `arrive`, not a second compose. `decide` is the classifier
   leaf -- no second selector; do NOT pin wave-swarm before briefing
   says swarm; `decide` steer on a multi-lane target is NOT an escape.
   Cross-repo: one repo owner per lane. Ask operator for model routing
   per profile; blank keeps default.

1. CLASSIFY + RUN: `flywheel workflow compose "<objective>" --parts wave-swarm
   --run --inputs '{"objective": "<target>"}'. Re-check the catalog each
   objective; add lanes only for declared multi-lane need.
   GATE->DISPATCH->EXECUTE->OBSERVE->VERIFY->SIGNOFF; parks at SIGNOFF,
   resolve via `flywheel workflow approve`. DISPATCH briefs `--stage
   ce-execute`, never raw. Alias `workflow run wave-swarm --execute`
   (not preferred).

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit
   tracker. Waves ONE AT A TIME; epics are gates.

3. LANES: single-writer file ownership from bead SCOPE.
   builtins/main.rs/one-file-at-a-time = handoff chain; disjoint files
   = parallel. Max 5 crews: 3 coder + scout (read-only) + verifier
   (re-runs VERIFY). guard-the-context-window: bulk to subagents,
   summaries in main thread.

4. SPAWN (escape; un-waved ad-hoc lanes only): `flywheel spawn <proj>
   --profile <role> --count N --escape`; kind/count + --model/
   --provider without --escape warns. peer-aware=off -> `--no-peer-aware`.
   Escape forfeits SIGNOFF + artifacts: write the trail or it does not
   exist.

5. BRIEF: lane dispatch authoritative. Peer wakes are signals, not
   tasks; peer-aware wake rides the doorbell.
   Loop is bead dispatch, not wake. Per lane: bead ids, files, pin,
   claim (br update in_progress), reserve (toron reserve acquire 4min),
   build slot, TDD (RED first), commit-before-close with VERIFY tail,
   stage ONLY reserved paths (never git add -A), on [doorbell] pull
   then act, blocked >10m mail captain, TIMEBOX 90m partial report,
   public mail identity, prove-it-works. Every dispatch ANNOUNCES its
   vehicle (stage name, compose token, or "none: raw") first line.
   --raw for nudges/wakes only. Join steer at spawn (--join-channel).
   No commits for ~2 polls -> status+continue wake; third contact =
   finish or release.

9. REPORT: crews post status/commit/verify/deviations on lane threads.

6. CONFLICT: soft -> `toron reserve request-release` -> respond
   agreed|denied|deferred -> escalate on steer, captain LAST. Hard
   conflict -> serialize. Cross-sweep -> flag, victim re-commits.
   never-block-on-the-human.

7. WAVE GATE: captain verifies with fresh eyes -- suite+fmt+clippy AND
   actual wiring (live doorbell and swarm_loop call workspace::tick).
   Close the epic with evidence, then next wave.
   sequence-verifiable-units.

8. TEARDOWN: `toron reserve release --project <p> --agent <pin>`;
   `flywheel mail slot release <project> --slot cargo --as <pin>`;
   `flywheel kill <project> --force --force-working`; br sync; commit
   .beads.

## MATERIAL RECLASSIFY (GS-048)

Once pinned, do not casually re-run decide — **until** a material event (AUTHORITATIVE objective change, explicit topology switch, new work-unit class, classifier miss, stage-chain exhaustion). Then: `decide --json` + `workflow list --brief` + `stage list`, announce vehicle / `none: raw`. Sticky-open still applies if the session was open.

## NO HARNESS-NATIVE DELEGATION (ADR-0010 D13)

No harness-native delegation tools (agent/subagent/task/workflow/spawn/
dispatch/worktree/delegate-shaped -- e.g. Claude's Task): they bypass
the spawn registry (doorbell, dispatcher, guards see nothing). Fence:
`flywheel __subagent-guard <tool>` (exit 2 = deny); observe/stop and
session-local todo stay legal; mcp__* (toron) always allowed; crew
panes exempt (`--role crew`). Dispatch via `flywheel spawn` /
`flywheel send` / toron mail. DEFENSE-IN-DEPTH: the fence is the
guard, not this text.

{{VERSION}} {{GENERATED}}
