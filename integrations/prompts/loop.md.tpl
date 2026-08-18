{{TOOL_FRONTMATTER}}
# Orchestration: loop

Run the /loop slash command: ONE agent, ONE objective, fully automated, but
able to deploy a peer (@reviewer, different model family) mid-flight at
defined checkpoints instead of shipping unverified. The opposite of /swarm:
no crew, no waves -- a single pane drives a bead to a verified close. The
summoned reviewer substitutes for the captain (never-block-on-the-human).

## The 6-step loop

1. SETUP: one pane via `flywheel spawn <proj> --profile <role>` (or run
   in-place if already pane'd). Write the objective as a brief:
   GOAL / SCOPE / CONTEXT / ACCEPTANCE / VERIFY / TIMEBOX / FORBIDDEN /
   REPORT / PRINCIPLES / STANDING (ADR-0019 Spec 2). Reserve files before
   edit (toron reserve acquire; renew 4min); build slot for cargo; poll the
   captain thread + your own inbox every 5 min.

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
   --mode pane` (n>=2 sketch arms), oracle judges. NEVER block on the
   captain for reversible calls: if a fact is observable by running
   something, run it; peers substitute (never-block-on-the-human).

4. VERIFY: run the ## VERIFY command from the brief; save the receipt.
   On failure -> fix-root-causes loop: reproduce first, ask why until the
   root, never re-verify a broken head (fix-root-causes). Check the real
   artifact, not "it compiles" (prove-it-works).

5. CLOSE: commit-before-close with the bead id in the message; close
   reason MUST include the ## VERIFY output tail; cite PRINCIPLES by name   -- which decision each changed. On TIMEBOX expiry: report the partial
   state + the exact resume point, then stop (outcome-oriented-execution).

6. DONE-MAIL: after closing, mail the captain on `thread_id=<bead-id>`;
   subject `[<bead-id>] done`, BODY with the report fields (status, commit
   sha, verdict kind, the VERIFY command + result tail, deviations,
   PRINCIPLES cited). One report, two channels: the close reason is the
   durable copy, the mail is the captain's copy. The captain has no push.

7. PROMOTE (optional): the loop MAY promote itself to /swarm -- spawn
   peers -- only when the bead graph shows parallelizable disjoint lanes.
   Otherwise stay one agent + the summoned reviewer.

Never ship unverified work; never stall waiting for the human.

{{VERSION}} {{GENERATED}}