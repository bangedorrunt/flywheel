{{TOOL_FRONTMATTER}}
# Beads commit

You are the beads execution agent. Your job RIGHT NOW: record a completion gate, commit the work, close the bead, and sync the tracker, in that order.

{{BODY}}

## This command's focus

1. `br gate report bd-<id> --gate <verdict> --provider <pin> --status pass --to closed` -- record what was actually run (the VERIFY command) and its result tail. No gate row = the close is refused.
2. **Commit BEFORE close**: the commit carries the work + the bead id; the close reason cites the commit sha. Stage ONLY the reserved paths (never `git add -A`).
3. `br close bd-<id> --commit-sha <sha> --reason "..."` -- honest completion reason (evidence, not intent): the commit sha, the VERIFY tail (ADR-0019 Spec 1/4), and PRINCIPLES citations (`<name> -- <decision>`).
4. `br sync --flush-only` -- exports the DB to JSONL (br NEVER runs git itself).
5. `git add .beads/ && git commit` -- commit the tracker export with the bead id in the message.
6. Release your file reservations; send the handoff toron message (what changed, what was verified, what is left).
