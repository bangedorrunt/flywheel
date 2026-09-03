{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

Run /swarm-mode: captain-led multi-crew execution.

Args: objective, scope, crew shape, flags. The captain steers crews via
current harness/profile per role. Parallel counterpart to /loop-mode (solo).
Swarm for lanes and waves; loop-mode for one objective in one pane.

One captain, up to 5 crews; lanes run parallel when files don't overlap.
Keep objective authoritative, each lane verifiable.

## Playbook

Before dispatch, CLASSIFY once if the primitive is unclear:
`flywheel decide "<target>" --json --ready-beads N` (sequence-verifiable-units).
/swarm-mode itself pins wave-swarm, never re-run decide after compose.

Ask operator for model routing per profile (planner/coder/reviewer/scout)
before spawn/dispatch. Show default + registry alternatives; blank keeps
default, explicit provider/model overrides. Use consistently.

1. RUN: `flywheel workflow compose "<objective>" --parts wave-swarm --run --inputs
   '{"objective": "<target>"}'`. Slash /swarm-mode pins this recipe.
   GATE->DISPATCH->EXECUTE->OBSERVE->VERIFY->SIGNOFF. The run parks at
   SIGNOFF: resolve via `flywheel workflow approve`. DISPATCH briefs
   `--stage systematic-execute` (p6) onto coder panes, never p1.

3. GATE PREP: `br sync --flush-only`; clear stale assignees; commit tracker.
   Map bead graph (deps, waves). Waves ONE AT A TIME; epics are gates.

4. LANES: single-writer file ownership from bead SCOPE. builtins/main.rs/
   one-file-at-a-time = handoff chain; disjoint files = parallel. Max 5
   crews: 3 coder + scout (read-only) + verifier (re-runs VERIFY,
   posts VERIFIED/FAILED). guard-the-context-window: bulk to subagents,
   summaries in main thread.

5. SPAWN (escape; un-waved ad-hoc lanes only): `flywheel spawn <proj>
   --profile <role> --count N --escape`; kind/count + --model/--provider
   without --escape warns. `/swarm-mode peer-aware=off` maps to spawn
   `--no-peer-aware`. Keep wake separate from supervision.

6. BRIEF: lane dispatch authoritative. Peer wakes are signals, not tasks.
   Bead ids, files, pin, claim (br update in_progress), reserve (toron
   reserve acquire 4min), build slot, TDD (RED first), commit-before-close
   with VERIFY tail, stage ONLY reserved paths (never git add -A),
   on [doorbell] pull then act, post to steer channel, blocked >10m mail
   captain, TIMEBOX 90m partial report. Use public mail identity.
   prove-it-works. Turn-based crews stall silently: when a claimed bead
   shows no commits for ~2 polls, send a status+continue wake.

7. CONFLICT: soft -> `toron reserve request-release` -> respond
   agreed|denied|deferred -> escalate on steer channel, captain LAST.
   Hard conflict -> serialize. Design question -> collab peer or
   interrogate/debate. Cross-sweep -> flag, victim re-commits.
   never-block-on-the-human.

8. WAVE GATE: captain verifies with fresh eyes -- suite+fmt+clippy AND the
   actual wiring. Live doorbell and swarm_loop must call workspace::tick
   (slice reducers stay private). Read the call sites; verify dropped-behavior
   lists (grep `let _ =` on action vecs). Close the epic with evidence, then
   next wave.

9. TEARDOWN: release reservations + slot, kill --force-working, br sync,
   commit .beads.
   - `toron reserve release --project <p> --agent <pin>`
   - `flywheel mail slot release <project> --slot cargo --as <pin>`
   - `flywheel kill <project> --force --force-working`

10. REPORT: each crew posts concise report on lane thread: status, commit,
   verify result, deviations.

{{VERSION}} {{GENERATED}}
