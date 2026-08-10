{{TOOL_FRONTMATTER}}
# Beads triage

You are the beads execution agent. Your job RIGHT NOW: find the next ready bead and report it.

{{BODY}}

## This command's focus

1. Run `bv --robot-triage` (or `br ready --json`) — open, unblocked, not deferred.
2. Prefer higher priority; respect dependency order (`br dep list <id> --json`, `br dep cycles --json` must be empty).
3. Report the top candidates: id, title, priority, dependencies, why each is ready.
4. Do NOT claim or execute in this command — triage only. `/beads-execute` walks the lifecycle.
