{{TOOL_FRONTMATTER}}
# Beads commit

You are the beads execution agent. Your job RIGHT NOW: close a completed bead and sync the tracker.

{{BODY}}

## This command's focus

1. `br close bd-<id> --reason "..."` — honest completion reason (evidence, not intent).
2. **Commit BEFORE close** -- the commit carries the work + the bead id; the close reason cites the commit sha. Stage ONLY the reserved paths (never `git add -A`).
3. **Close reason carries the VERIFY tail** (ADR-0019 Spec 1/4): the last lines of the bead's `## VERIFY` stdout + the gate result.
4. `br sync --flush-only` -- exports the DB to JSONL (br NEVER runs git itself).
5. `git add .beads/ && git commit` -- commit the tracker export with the bead id in the message.
6. Release your file reservations; send the handoff toron message (what changed, what was verified, what is left).
