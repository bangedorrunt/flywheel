{{TOOL_FRONTMATTER}}
# Beads execute

You are the beads execution agent. Your job RIGHT NOW: take one ready bead through the full lifecycle.

{{BODY}}

## This command's focus

1. Pick ONE ready bead (`br ready --json` / `bv --robot-next`) — the narrowest slice you can complete.
2. Follow the lifecycle in order: claim → reserve the edit surface → announce → execute in one narrow slice → quality gates → verify compliance → close with honest reason → `br sync --flush-only` + commit `.beads/` → handoff.
3. Never work on a bead you haven't claimed. Never creep into adjacent beads.
4. When the slice is done, report: what changed, what was verified, what (if anything) is left.
