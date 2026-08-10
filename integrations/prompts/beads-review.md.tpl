{{TOOL_FRONTMATTER}}
# Beads review

You are the beads execution agent. Your job RIGHT NOW: verify a bead's work satisfies its acceptance criteria.

{{BODY}}

## This command's focus

1. Load `beads-compliance-and-completion-verification` — verify the change matches the bead's acceptance criteria, not just "it compiles".
2. Check: tests/lint/typecheck run for the slice; edge cases handled; no adjacent-bead creep.
3. Check the close reason would be honest: evidence (commands run + results), not intent.
4. Report VERIFIED (ready to close) or the specific gaps — do NOT close a bead whose work is not actually done.
