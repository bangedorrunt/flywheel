{{TOOL_FRONTMATTER}}
# Orchestration: swarm-mode

Run the /swarm-mode slash command: the multi-crew, captain-led execution
mode.

Arguments are supplied by the harness adapter above. Treat the supplied text as
the objective, scope, crew shape, and mode flags for this invocation.
The captain steers multiple crews, normally using the current harness/profile
for each role. This is the parallel counterpart to `/loop`, which is solo
execution in one current harness. Swarm is for independent lanes and waves;
loop is for one objective driven end-to-end by one pane.

One captain orchestrates up to 5 crews across a work graph; independent lanes
run in parallel when their file ownership does not overlap. Keep the captain's objective authoritative and make each lane independently
verifiable.

## The 8-step playbook

Before spawning or dispatching, ask the operator to choose the model routing for
each profile (for example planner, coder, reviewer, scout). Present each
profile's default model and compatible registry alternatives. Blank input keeps
the profile default; an explicit provider/model selection is an override.
Use the selected profile/model consistently for that lane.

1. CLASSIFY: `flywheel decide "<target>" --json --ready-beads N`. Follow the
   returned primitive rather than inventing a second execution path.

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit the
   tracker. Map the bead graph (deps, wave epics). Waves run ONE AT A TIME;
   epics are gates, not tasks (sequence-verifiable-units).

3. LANES: derive single-writer file ownership from bead SCOPE. builtins /
   main.rs / one-file-at-a-time = reservation handoff chain; disjoint
   files / disjoint repos = parallel. Max 5 crews: 3 coder lanes + 1 scout
   (spikes/baseline, read-only) + 1 verifier (receipt-runner: re-runs every
   bead's ## VERIFY at clean HEAD, posts VERIFIED/FAILED; br sync
   housekeeping).

4. SELECT AND SPAWN: before dispatch, ask the operator to choose the
   profile and model for each role. Show the profile default and available
   registry alternatives, including harness compatibility and cost when known.
   Then use `flywheel spawn <proj> --profile <role> --count N`; use an explicit
   provider/model only when the operator selects an override. Keep wake
   coordination separate from work-graph supervision.

5. BRIEF each lane: the captain's lane dispatch is authoritative for that
   crew. Peer-aware wake messages are coordination signals, not replacement
   tasks. Bead ids in order, files owned, pin, claim
   (br update --status=in_progress), reserve before edit
   (toron reserve acquire, renew 4min), build slot before cargo, TDD
   (RED test first), commit-before-close with bead id in the message, close
   reason MUST include the ## VERIFY output tail, stage ONLY reserved paths
   (never git add -A - cross-sweep race), on [doorbell] pull then act
   (no 5-min inbox timer), post
   claim/close/blocked to the steer channel, blocked >10min -> mail the
   captain, TIMEBOX 90min then partial report with exact resume point.
   Use the resolved public mail identity for coordination and repository hooks.

6. CONFLICT / COLLAB LADDER: soft conflict (TTL overlap / want a held
   path) -> `toron reserve request-release --target <holder-pin> --ask
   release|defer:<secs>` (plane P) -> holder answers `toron reserve respond
   --decision agreed|denied|deferred:<secs>` -> escalate after timeout on
   the steer channel (kind 9), captain LAST. Hard reservation conflict ->
   serialize + post the channel. Shared design question -> collab the peer
   (`flywheel collab <proj> --to <peer-pin>`) or interrogate/debate for
   forks. Cross-sweep detected -> flag on the channel, victim re-commits.
   Grant request for out-of-reservation files -> flag FIRST, captain
   approves narrowly.

7. WAVE GATE: the captain runs the epic's verification checkboxes itself -
   suite + grep + fmt canonical + binary reinstall (cargo install --path .
   --force; a stale binary nearly failed a gate). Close the epic with
   evidence, THEN dispatch the next wave (prove-it-works).

8. TEARDOWN: release reservations + build slot, kill --force-working, br sync,
   commit .beads.
   - reservations: `toron reserve release --project <p> --agent <pin>` (empty
     --paths = all of the agent's leases; idempotent).
   - build slot: `flywheel mail slot release <project> --slot cargo --as <pin>`.
   - kill: `flywheel kill <project> --force --force-working`.

9. REPORT (every crew): when your lane is done, send the captain a concise
   report on the lane thread when one exists. Include status, commit,
   verification command and result, and deviations.

Never block on the captain for reversible calls: if a fact is observable by
running something, run it (never-block-on-the-human).

{{VERSION}} {{GENERATED}}
