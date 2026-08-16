{{TOOL_FRONTMATTER}}
# Orchestration: swarm

Run the /swarm slash command: the wave-gated bead-graph swarm playbook. One
captain orchestrates up to 5 crews across a bead graph; waves run one at a
time; epics are gates, not tasks. Distilled from the ADR-0019 program
(2026-08-15, 34/34 beads).

## The 8-step playbook

1. CLASSIFY: `flywheel decide "<target>" --json --ready-beads N`. If beads
   already exist for the objective -> THIS playbook, not steer. Record the
   deviation if you override decide.

2. GATE PREP: `br sync --flush-only`; clear stale assignees; commit the
   tracker. Map the bead graph (deps, wave epics). Waves run ONE AT A TIME;
   epics are gates, not tasks (sequence-verifiable-units).

3. LANES: derive single-writer file ownership from bead SCOPE. builtins /
   main.rs / one-file-at-a-time = reservation handoff chain; disjoint
   files / disjoint repos = parallel. Max 5 crews: 3 coder lanes + 1 scout
   (spikes/baseline, read-only) + 1 verifier (receipt-runner: re-runs every
   bead's ## VERIFY at clean HEAD, posts VERIFIED/FAILED; br sync
   housekeeping).

4. SPAWN: `flywheel spawn <proj> --profile <role> --count N` (peer-aware
   self-poll ON; --no-peer-aware only for single-shot panes). Poll-channel:
   `toron channel create <slug>-steer`; captain mail thread <slug>-captain.
   Captain posts the lane map + contract. `flywheel loop run <proj>` from
   PARENT cwd, project as slug.

5. BRIEF each lane: bead ids in order, files owned, pin, claim
   (br update --status=in_progress), reserve before edit
   (toron reserve acquire, renew 4min), build slot before cargo, TDD
   (RED test first), commit-before-close with bead id in the message, close
   reason MUST include the ## VERIFY output tail, stage ONLY reserved paths
   (never git add -A - cross-sweep race), poll inbox 5min, post
   claim/close/blocked to the steer channel, blocked >10min -> mail the
   captain, TIMEBOX 90min then partial report with exact resume point.
   AGENT_NAME=<pin> env for repo hooks (guard-the-context-window).

6. CONFLICT / COLLAB LADDER: hard reservation conflict -> serialize + post
   the channel. Shared design question -> collab the peer
   (`flywheel collab <proj> --to <peer-pin>`) or interrogate/debate for
   forks. Cross-sweep detected -> flag on the channel, victim re-commits.
   Grant request for out-of-reservation files -> flag FIRST, captain
   approves narrowly.

7. WAVE GATE: the captain runs the epic's verification checkboxes itself -
   suite + grep + fmt canonical + binary reinstall to ~/.local/bin (a stale
   binary nearly failed a gate). Close the epic with evidence, THEN
   dispatch the next wave (prove-it-works).

8. TEARDOWN: release slots/reservations, kill --force-working, br sync,
   commit .beads.

Never block on the captain for reversible calls: if a fact is observable by
running something, run it (never-block-on-the-human).

{{VERSION}} {{GENERATED}}
