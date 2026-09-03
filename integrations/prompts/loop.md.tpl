{{TOOL_FRONTMATTER}}
# Orchestration: loop-mode

Run the /loop-mode slash command: solo mode.

Arguments are supplied by the harness adapter above. Treat the supplied text as
the objective, scope, and any mode flags for this invocation. ONE current harness/pane
drives ONE objective end-to-end. It uses workflow runtime
machinery but is not a reusable workflow definition. Beads are optional:
`--work` selects bead-backed execution; without it, the loop may drive an
objective, queue, or workflow. It is the opposite of /swarm-mode: no parallel
crew lanes and no wave fan-out. A reviewer may be summoned at checkpoints,
but that reviewer is an exception, not a second execution crew.

The solo agent is its own captain for reversible decisions; the human
objective and scope stay authoritative, and peer-aware messages never replace
them.

## The loop

Before each managed run, ask the operator to choose the profile and model
routing. Present the profile default plus compatible registry alternatives
with known cost/capabilities. Blank keeps the default; an explicit
provider/model overrides it. Record the choice in the launch/report summary.

1. SETUP: `flywheel loop-mode run <project> --work`. Slash /loop-mode pins
   this. Do not re-run `flywheel decide`. One pane via `flywheel spawn <proj>
   --profile <role>` (or run in-place if already pane'd). The selected/current
   harness is the execution harness for the whole solo run. Write the objective
   as a brief with GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX /
   FORBIDDEN / REPORT. Keep it specific enough to execute and verify. Reserve
   files before edit (toron reserve acquire; renew 4min); build slot for cargo.
   Wake is the workspace doorbell: show you heard it, then pull the relevant
   inbox, channel, or job. Do not start a self-managed polling timer.

2. EXECUTE: TDD red-green per unit (RED test first, GREEN minimal code,
   REFACTOR while green). Stage ONLY reserved paths (never git add -A --
   cross-sweep race); commit per unit with the unit id in the message.
   Each unit ends in a check; verify before the next
   (sequence-verifiable-units).

3. IN-FLIGHT PEER CHECKPOINTS (deploy, don't stall): call the reviewer
   AFTER the RED test exists, BEFORE any commit that closes a
   judgment-laden unit, and on any design fork:
   `flywheel collab <proj> --to <reviewer-pin> --subject ... --body ...`
   (blocks for reply; reviewer profile = different model family). Forks
   with 2+ viable options -> `flywheel workflow run prototype --execute
   --mode pane` (n>=2 sketch arms), oracle judges. Red-verify:
   `flywheel workflow compose "<objective>" --parts repair-until-verified --run`.
   NEVER block on the
   captain for reversible calls: if a fact is observable by running
   something, run it; peers substitute (never-block-on-the-human).

4. VERIFY: run the ## VERIFY command from the brief when one exists; for
   non-bead objectives, run the explicit acceptance/verification command from
   the objective. Save the receipt. On failure -> fix-root-causes loop:
   reproduce first, ask why until the
   root, never re-verify a broken head (fix-root-causes). Check the real
   artifact, not "it compiles" (prove-it-works).

5. CLOSE: when bead-backed, commit-before-close with the bead id in the
   message and include the ## VERIFY output tail in the close reason. For a
   non-bead objective, report the commit/result without pretending a bead was
   closed.   Report the verification evidence and any important deviations. On TIMEBOX
   expiry: report the partial
   state + the exact resume point, then stop (outcome-oriented-execution).

6. REPORT: after completion, send the captain a concise report on the task
   thread when one exists. Include status, commit, verification command and
   result, deviations, and the exact resume point if incomplete.

7. PROMOTE (optional): switch to `/swarm-mode` only on operator choice.
   Solo stays one harness + reviewer.

{{VERSION}} {{GENERATED}}