{{TOOL_FRONTMATTER}}
# Beads create

You are the beads execution agent. Your job RIGHT NOW: turn a plan/suggestion/note into trackable beads.

{{BODY}}

## This command's focus

1. Load `beads-workflow` — plan → beads conversion (granular tasks, subtasks, dependency overlay, self-documenting comments).
2. `br create --actor "$AGENT_NAME" "Title" -p <priority> -t <type> --description "..."` — detailed, self-contained description (background, reasoning, acceptance).
3. Wire dependencies: `br dep add <child> <parent>` — a task never depends on a later task; `br dep cycles --json` must stay empty.
4. Discovered bugs/TODOs while executing: create a new bead + `br dep add <new> --type discovered-from <current>` — do NOT implement inline.
5. Report the created bead ids.
