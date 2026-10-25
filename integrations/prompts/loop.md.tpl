{{TOOL_FRONTMATTER}}
# Orchestration: loop-mode

/loop-mode runs `loop-mode run` in open mode. It is not a third mode.
Read `flywheel-open`. Do not read `flywheel-captain`.

The supplied text is the objective, scope, and flags. ONE pane drives
ONE objective end-to-end, not a reusable definition. Beads optional:
`--work` selects bead-backed execution. The opposite of /swarm-mode: no
parallel lanes, no wave fan-out; a reviewer at checkpoints is an
exception, not a second crew. The solo agent captains reversible
decisions; the human objective stays authoritative.

## MATERIAL RECLASSIFY

On a material trigger (new AUTHORITATIVE objective, explicit topology
ask, new work-unit class, classifier miss, chain exhaustion):
re-run `decide --json` + `workflow list --brief` + `stage list`, announce
`vehicle: ...` or `none: raw`; never flip loop->swarm unasked.

## The loop

0. ARRIVE then BRIEF: `flywheel arrive <project> --as <pin>`
   then `flywheel briefing "<objective>" --project <p> --json`. Join
   topology -> do not start a second loop; loop -> continue. `decide`
   is the leaf inside briefing. Before each run, confirm profile + model
   routing: show default + registry alternatives; blank keeps it; record
   the choice.

1. SETUP: `flywheel loop-mode run <project> --work` after briefing
   commits loop. If the durable run fails: `flywheel loop-mode doctor
   --fix`, then restart -- a plain re-run starts FRESH; `--resume` only
   replays a killed run on purpose. Re-check the catalog on
   follow-ups; smallest vehicle wins. Write the objective as a brief
   with GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX /
   FORBIDDEN / REPORT. Reserve files before edit (toron reserve acquire,
   renew 4min); build slot for cargo. Wake is the doorbell: ack, then
   pull inbox/channel/job; no polling timers. Ring: `flywheel doorbell
   inject <project> <pin> <line>` (journaled, budgeted, digest-visible).

2. EXECUTE: TDD per unit (RED first, GREEN minimal, REFACTOR green). Stage ONLY reserved paths (never git add -A -- cross-sweep
   race); commit per unit with the unit id in the message. Each unit ends
   in a check (sequence-verifiable-units).

3. IN-FLIGHT PEER CHECKPOINTS (deploy, don't stall): call the reviewer
   after RED exists, before closing a judgment-laden unit, and on design
   forks: `flywheel collab <proj> --to <reviewer-pin> --subject ... --body
   ...`
   (blocks; reviewer = different model family). Forks with 2+ options -> `flywheel workflow run prototype --execute --mode
   pane` (n>=2 arms), oracle judges. Red-verify: `flywheel workflow compose "<objective>" --parts
   repair-until-verified --run`. NEVER block
   the captain for reversible calls; peers substitute
   (never-block-on-the-human).

4. VERIFY: run the brief's ## VERIFY command (or the acceptance
   command for non-bead objectives); save the receipt. On failure ->
   fix-root-causes loop: reproduce first, ask why until the root, never
   re-verify a broken head. Check the real artifact, not "it compiles"
   (prove-it-works).

5. CLOSE: bead-backed -> commit-before-close, bead id in the message and
   ## VERIFY tail in the close reason; release reservations on close
   (`toron reserve release --project <p> --as <pin>`) -- stale leases
   block the next dispatch. Non-bead: report the result honestly. On
   TIMEBOX expiry: report partial state + resume point, then stop
   (outcome-oriented-execution).

6. REPORT: on the task thread if one exists, mail the captain: status,
   commit, verification command + result, deviations, resume point if
   incomplete.

7. PROMOTE (optional): /swarm-mode only on operator choice; solo stays
   one harness + reviewer.

## NO HARNESS DELEGATION

The loop pane owns the dispatcher. No harness-native delegation tools
(agent/subagent/task/spawn/dispatch/worktree/delegate-shaped): they
bypass the registry (doorbell + dispatcher + guards see nothing). Fence:
`flywheel __subagent-guard <tool>` (exit 2 = deny); observe/stop and
session-local todo stay legal; mcp__* always allowed; crew panes exempt
(`--role crew` / `FLYWHEEL_ROLE=crew`). The fence is the guard.

{{VERSION}} {{GENERATED}}
