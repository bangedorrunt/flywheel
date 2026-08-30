{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

Run /swarm-mode: captain-led multi-crew execution.

Args: objective, scope, crew shape, flags. The captain steers crews via
current harness/profile per role. Parallel counterpart to /loop (solo).
Swarm for lanes and waves; loop for one objective in one pane.

One captain, up to 5 crews; lanes run parallel when files don't overlap.
Keep objective authoritative, each lane verifiable.

## Playbook

Ask operator for model routing per profile (planner/coder/reviewer/scout)
before spawn/dispatch. Show default + registry alternatives; blank keeps
default, explicit provider/model overrides. Use consistently.

1. CLASSIFY: `flywheel decide "<target>" --json --ready-beads N`.
   Follow primitive (sequence-verifiable-units).

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit tracker.
   Map bead graph (deps, waves). Waves ONE AT A TIME; epics are gates.

3. LANES: single-writer file ownership from bead SCOPE. builtins/main.rs/
   one-file-at-a-time = handoff chain; disjoint files = parallel. Max 5
   crews: 3 coder + scout (read-only) + verifier (re-runs VERIFY,
   posts VERIFIED/FAILED). guard-the-context-window: bulk to subagents,
   summaries in main thread.

4. WAVES + SPAWN (waves are the default for supervised multi-lane
   work; spawn is the manual escape): run the wave as a recipe:
   `flywheel workflow compose --parts wave-swarm --run --inputs
   '{"objective": "<target>"}'` -- GATE->DISPATCH->OBSERVE->VERIFY->
   SIGNOFF; swarm-mode is the crew, wave-swarm is the recipe. The run
   parks at SIGNOFF: you resolve the approval (flywheel runs list).
   MANUAL ESCAPE (un-waved ad-hoc lanes): `flywheel spawn <proj>
   --profile <role> --count N`; explicit provider/model only on
   override. Keep wake separate from supervision.

5. BRIEF: lane dispatch authoritative. Peer wakes are signals, not tasks.
   Bead ids, files, pin, claim (br update in_progress), reserve (toron
   reserve acquire 4min), build slot, TDD (RED first), commit-before-close
   with VERIFY tail, stage ONLY reserved paths (never git add -A),
   on [doorbell] pull then act, post to steer channel, blocked >10m mail
   captain, TIMEBOX 90m partial report. Use public mail identity.
   prove-it-works.
   Supervised wave execution: `flywheel workflow compose --parts wave-swarm` -- swarm-mode is the crew, wave-swarm is the recipe.

6. CONFLICT: soft -> `toron reserve request-release` -> respond
   agreed|denied|deferred -> escalate on steer channel, captain LAST.
   Hard conflict -> serialize. Design question -> collab peer or
   interrogate/debate. Cross-sweep -> flag, victim re-commits.
   never-block-on-the-human.

7. WAVE GATE: captain verifies epic checkboxes (suite+grep+fmt+install
   --force), closes epic with evidence, then next wave.

8. TEARDOWN: release reservations + slot, kill --force-working, br sync,
   commit .beads.
   - `toron reserve release --project <p> --agent <pin>`
   - `flywheel mail slot release <project> --slot cargo --as <pin>`
   - `flywheel kill <project> --force --force-working`

9. REPORT: each crew posts concise report on lane thread: status, commit,
   verify result, deviations.

{{VERSION}} {{GENERATED}}
