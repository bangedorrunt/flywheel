{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

Run /swarm-mode: captain-led multi-crew execution.

Args: objective, scope, crew shape, flags. Parallel counterpart to
/loop-mode (solo). One captain, up to 5 crews; lanes run parallel when
files don't overlap. Keep objective authoritative, each lane verifiable.

## Playbook

ARRIVE then BRIEF (ADR-0011). Do not pin wave-swarm before briefing
sees joinable / ready-beads / N.

0. ARRIVE: `flywheel arrive <project> --as <pin>` (helm + JOINING banner
   if a live fleet exists, FCFS captain, doorbell `--live`). With the
   `arrival_triple` flag on, arriving also presence-publishes, role-binds
   from your spawn-registry profile, and puts your roster row in every
   digest — no extra command. Then
   `flywheel briefing "<objective>" --project <p> --json`. Print the
   proposal, ask topology + N, `--commit` (or `--yes` for defaults).
   A joinable world proposes `arrive`, not a second compose. `decide`
   is the classifier leaf inside briefing — do not invent a second
   selector.
CLASSIFY once if unclear (`flywheel briefing "<target>" --project <p>
--json`; sequence-verifiable-units). /swarm-mode does NOT pin
wave-swarm until briefing says swarm.

`decide` returning steer on a multi-lane target is NOT an escape: run
wave-swarm. Cross-repo lanes: register the pin on both projects, one
repo owner per lane (08-29/09-03 swarms).

Ask operator for model routing per profile before dispatch; blank keeps default.

1. RUN: `flywheel workflow compose "<objective>" --parts wave-swarm --run --inputs
   '{"objective": "<target>"}'`. Slash /swarm-mode pins this compose token. Beads-lifecycle hands off here after integrate (`until=integrate`).
   GATE->DISPATCH->EXECUTE->OBSERVE->VERIFY->SIGNOFF. The run parks at
   SIGNOFF: resolve via `flywheel workflow approve`. DISPATCH briefs
   `--stage ce-execute` onto coder panes, never raw. Legacy
   alias: `workflow run wave-swarm --execute` (not preferred). Swarm =
   supervision topology, never "swarm workflow".

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit tracker.
   Map bead graph. Waves ONE AT A TIME; epics are gates.

3. LANES: single-writer file ownership from bead SCOPE. builtins/main.rs/
   one-file-at-a-time = handoff chain; disjoint files = parallel. Max 5
   crews: 3 coder + scout (read-only) + verifier (re-runs VERIFY,
   posts VERIFIED/FAILED). guard-the-context-window: bulk to subagents,
   summaries in main thread.

4. SPAWN (escape; un-waved ad-hoc lanes only): `flywheel spawn <proj>
   --profile <role> --count N --escape`; kind/count + --model/--provider
   without --escape warns. `/swarm-mode peer-aware=off` maps to spawn
   `--no-peer-aware`. Escape forfeits SIGNOFF + artifacts: write the
   trail (timeline/beads) or it does not exist.

5. BRIEF: lane dispatch authoritative. Peer wakes are signals, not tasks;
   peer-aware wake rides the doorbell. Loop is bead dispatch, not wake.
   Bead ids, files, pin, claim (br update in_progress), reserve (toron
   reserve acquire 4min), build slot, TDD (RED first), commit-before-close
   with VERIFY tail, stage ONLY reserved paths (never git add -A),
   on [doorbell] pull then act, blocked >10m mail captain, TIMEBOX 90m
   partial report. Use public mail identity. prove-it-works. Every
   dispatch ANNOUNCES its vehicle (stage name, compose token such as
   wave-swarm, or "none: raw") in the brief's first line; crews restate
   it in their report (status, sha, verify, deviations, stage). Coder
   execution dispatches
   --stage ce-execute, never raw; --raw is for
   nudges/wakes only. Join the steer channel at spawn (--join-channel)
   so "post to steer channel" is real. Turn-based crews
   stall silently: when a claimed bead shows no commits for ~2 polls,
   send a status+continue wake; third contact = finish or release.

6. CONFLICT: soft -> `toron reserve request-release` -> respond
   agreed|denied|deferred -> escalate on steer channel, captain LAST.
   Hard conflict -> serialize. Design question -> collab peer or
   interrogate/debate. Cross-sweep -> flag, victim re-commits.
   never-block-on-the-human.

7. WAVE GATE: captain verifies with fresh eyes -- suite+fmt+clippy AND the
   actual wiring. Live doorbell and swarm_loop must call workspace::tick
   (slice reducers stay private). Verify dropped-behavior lists (grep
   `let _ =` on action vecs). Close the epic with evidence, then next wave.

8. TEARDOWN: release reservations + slot, kill --force-working, br sync,
   commit .beads.
   - `toron reserve release --project <p> --agent <pin>`
   - `flywheel mail slot release <project> --slot cargo --as <pin>`
   - `flywheel kill <project> --force --force-working`

9. REPORT: each crew posts status/commit/verify/deviations on lane thread.

## NO HARNESS-NATIVE DELEGATION (ADR-0010 D13)

This pane is the dispatcher-owning primary: it MUST NOT spawn work through
harness-native delegation tools (agent/subagent/task/workflow/spawn/dispatch/
worktree/delegate-shaped — e.g. Claude's Task, harness subagent spawns).
Those bypass the spawn registry: the doorbell, dispatcher, and guards cannot
see the work. The tool-surface fence is `flywheel __subagent-guard <tool>`
(exit 2 = deny); observe/stop verbs and session-local todo stay legal, and
mcp__* (toron) is always allowed. Crew panes are exempt
(`--role crew` / pane env `FLYWHEEL_ROLE=crew`). Dispatch all work via
`flywheel spawn` / `flywheel send` / toron mail so the registry tracks it.
This standing order is DEFENSE-IN-DEPTH — the fence is the guard, not this
text.

{{VERSION}} {{GENERATED}}
