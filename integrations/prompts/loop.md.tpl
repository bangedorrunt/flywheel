{{TOOL_FRONTMATTER}}
# Orchestration: loop-mode

Run /loop-mode: solo mode in the current harness.

The supplied text is the objective, scope, and flags. ONE
harness/pane drives ONE objective end-to-end; not a reusable definition. Beads optional:
`--work` selects bead-backed execution. The opposite of /swarm-mode: no
parallel lanes, no wave fan-out; a reviewer at checkpoints is an exception,
not a second crew. The solo agent captains reversible decisions; the
human objective stays authoritative.

## MATERIAL RECLASSIFY (GS-048)

On material mid-session triggers (new AUTHORITATIVE objective, explicit topology ask, new work-unit class, classifier miss, stage-chain exhaustion): re-run `decide --json` + `workflow list --brief` + `stage list` and announce `vehicle: ...` or `none: raw`. Never flip loop->swarm unasked.

## The loop

0. ARRIVE then BRIEF: `flywheel arrive <project> --as <pin>`
   then `flywheel briefing "<objective>" --project <p> --json`. Join
   topology -> do not start a second loop; loop -> continue. `decide`
   is the leaf inside briefing. Before each managed run, confirm
   profile + model routing: show the default plus registry alternatives;
   blank keeps the default; record the choice.

1. SETUP: `flywheel loop-mode run <project> --work` after briefing
   commits loop. If the durable run fails (nested-runtime panic, or a
   replay storm of serialization errors), do not resume it bare: kill it,
   `flywheel loop-mode doctor --fix`, then run supervised passes
   (`flywheel loop-mode run <project> --work --force --once` on an interval)
   until a clean durable run takes over. Re-check the catalog on follow-ups; smallest vehicle
   wins. Write the objective as a brief
   with GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX /
   FORBIDDEN / REPORT. Reserve files before edit (toron reserve acquire;
   renew 4min); build slot for cargo. Wake is the doorbell: ack it,
   then pull inbox/channel/job. No polling timers. Ring:
   `flywheel doorbell inject <project> <pin> <line>` -- journaled,
   budgeted, digest-visible.

2. EXECUTE: TDD per unit (RED test first, GREEN minimal code,
   REFACTOR while green). Stage ONLY reserved paths (never git add -A --
   cross-sweep race); commit per unit with the unit id in the message.
   Each unit ends in a check before the next
   (sequence-verifiable-units).

3. IN-FLIGHT PEER CHECKPOINTS (deploy, don't stall): call the reviewer
   after RED exists, before closing a judgment-laden unit, and on
   design forks:
   `flywheel collab <proj> --to <reviewer-pin> --subject ... --body ...`
   (blocks for reply; reviewer profile = different model family). Forks
   with 2+ viable options -> `flywheel workflow run prototype --execute
   --mode pane` (n>=2 arms), oracle judges. Red-verify: `flywheel
   workflow compose "<objective>" --parts repair-until-verified --run`.
   NEVER block on the captain for reversible calls; peers substitute
   (never-block-on-the-human).

4. VERIFY: run the ## VERIFY command from the brief when one exists;
   non-bead objectives run the explicit acceptance command. Save the
   receipt. On failure -> fix-root-causes loop: reproduce first, ask
   why until the root, never re-verify a broken head. Check the real
   artifact, not "it compiles" (prove-it-works).

5. CLOSE: bead-backed -> commit-before-close with the bead id in the
   message and the ## VERIFY output tail in the close reason. Release your
   reservations once the close lands (`toron reserve release --project <p>
   --as <pin>`); stale leases block the next dispatch. Non-bead:
   report the commit/result honestly. On
   TIMEBOX expiry: report partial state + exact resume point, then stop
   (outcome-oriented-execution).

6. REPORT: send the captain a concise report on the task thread when
   one exists: status, commit, verification command + result,
   deviations, exact resume point if incomplete.

7. PROMOTE (optional): /swarm-mode only on operator choice; solo stays
   one harness + reviewer.

## NO HARNESS-NATIVE DELEGATION 

The loop pane owns the dispatcher: no harness-native delegation
tools (agent/subagent/task/spawn/dispatch/worktree/delegate-shaped).
They bypass the registry (doorbell + dispatcher + guards see nothing).
Fence: `flywheel __subagent-guard <tool>` (exit 2 = deny); observe/stop and
session-local todo stay legal; mcp__* always allowed; crew panes exempt
(`--role crew` / `FLYWHEEL_ROLE=crew`). Defense-in-depth: the fence is the
guard, not this text.

{{VERSION}} {{GENERATED}}
