{{TOOL_FRONTMATTER}}
# Beads commit

You are the beads execution agent. Your job RIGHT NOW: close a completed bead and sync the tracker.

{{BODY}}

## This command's focus

1. `br close bd-<id> --reason "..."` — honest completion reason (evidence, not intent).
2. `br sync --flush-only` — exports the DB to JSONL (br NEVER runs git itself).
3. `git add .beads/ && git commit` — commit the tracker export with the bead id in the message.
4. Release your file reservations; send the handoff toron message (what changed, what was verified, what is left).
