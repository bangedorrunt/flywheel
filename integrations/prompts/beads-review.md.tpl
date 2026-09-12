{{TOOL_FRONTMATTER}}
# Beads review

You are the beads execution agent. Your job RIGHT NOW: verify a bead's work satisfies its acceptance criteria.

{{BODY}}

## This command's focus

1. Load `beads-compliance-and-completion-verification` -- verify the change matches the bead's acceptance criteria, not just "it compiles".
2. Check: tests/lint/typecheck run for the slice; edge cases handled; no adjacent-bead creep.
3. Check the close reason would be honest: evidence (commands run + results), not intent.
4. **Review at CLEAN HEAD (ADR-0019):** verify the committed state, not the working tree -- `git stash` or review the pushed commit / a `/tmp` clone. Uncommitted edits are not evidence.
5. **Reject PRINCIPLES-without-decision:** a brief `PRINCIPLES` line that cites a principle name but no decision is cargo-cult (ADR-0019 Spec 5) -- flag it.
6. **Filename check:** no new `*V2*` / `*_improved*` / `*_enhanced*` files (subtract-before-you-add; ADR-0019 Spec 6 preflight).
7. Report VERIFIED (ready to close) or the specific gaps -- do NOT close a bead whose work is not actually done.
