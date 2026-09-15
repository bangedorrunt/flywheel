{{TOOL_FRONTMATTER}}
# Orchestration: loop-mode

Run /loop-mode: solo mode in the current harness.

Treat the supplied text as the objective, scope, and flags. ONE
harness/pane drives ONE objective end-to-end via workflow runtime
machinery; not a reusable definition. Beads optional:
`--work` selects bead-backed execution. The opposite of /swarm-mode: no
parallel lanes, no wave fan-out; a reviewer at checkpoints is an exception,
not a second crew. The solo agent is its own captain for reversible
decisions; the human objective and scope stay authoritative.

## MATERIAL RECLASSIFY (GS-048)

On material mid-session triggers (new AUTHORITATIVE objective, explicit topology ask, new work-unit class, classifier miss, stage-chain exhaustion): re-run `decide --json` + `workflow list --brief` + `stage list` and announce `vehicle: …` or `none: raw`. Do not flip loop→swarm without an explicit request.

## The loop

0. ARRIVE then BRIEF (ADR-0011): `flywheel arrive <project> --as <pin>`
   then `flywheel briefing "<objective>" --project <p> --json`. Join
   topology -> do not start a second loop; loop -> continue. `decide`
   is the leaf inside briefing. Before each managed run, ask the
   operator for profile + model routing: present the profile default
   plus compatible registry alternatives with cost/caps; blank keeps
   the default; record the choice in the launch/report summary.

1. SETUP: `flywheel loop-mode run <project> --work` after briefing
   commits loop. Re-check the catalog on follow-up/context changes; choose
   the smallest vehicle. Write the objective as a brief
   with GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX /
   FORBIDDEN / REPORT. Reserve files before edit (toron reserve acquire;
   renew 4min); build slot for cargo. Wake is the workspace doorbell:
   show you heard it, then pull the relevant inbox, channel, or job. No
   self-managed polling timer. Ring from the console:
   `flywheel doorbell inject <project> <pin> <line>` -- journaled,
   budgeted, digest-visible.

2. EXECUTE: TDD red-green per unit (RED test first, GREEN minimal code,
   REFACTOR while green). Stage ONLY reserved paths (never git add -A --
   cross-sweep race); commit per unit with the unit id in the message.
   Each unit ends in a check before the next
   (sequence-verifiable-units).

3. IN-FLIGHT PEER CHECKPOINTS (deploy, don't stall): call the reviewer
   AFTER the RED test exists, BEFORE any commit closing a judgment-laden
   unit, and on any design fork:
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
   message and the ## VERIFY output tail in the close reason. Non-bead:
   report the commit/result without pretending a bead was closed. On
   TIMEBOX expiry: report partial state + exact resume point, then stop
   (outcome-oriented-execution).

6. REPORT: send the captain a concise report on the task thread when
   one exists: status, commit, verification command + result,
   deviations, exact resume point if incomplete.

7. PROMOTE (optional): /swarm-mode only on operator choice; solo stays
   one harness + reviewer.

## NO HARNESS-NATIVE DELEGATION (ADR-0010 D13)

The loop pane is a dispatcher-owning primary: no harness-native delegation
tools (agent/subagent/task/workflow/spawn/dispatch/worktree/delegate-shaped).
They bypass the spawn registry (doorbell + dispatcher + guards see nothing).
Fence: `flywheel __subagent-guard <tool>` (exit 2 = deny); observe/stop and
session-local todo stay legal; mcp__* always allowed; crew panes exempt
(`--role crew` / `FLYWHEEL_ROLE=crew`). Defense-in-depth: the fence is the
guard, not this text.

{{VERSION}} {{GENERATED}}
