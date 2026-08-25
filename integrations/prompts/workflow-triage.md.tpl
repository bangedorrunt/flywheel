{{TOOL_FRONTMATTER}}
# Workflow: triage

Run the triage-deep workflow: backlog triage — br triage --json, prioritize + assign, spawn the crew.

## This command's focus

1. Gather the project from the user's message (or the `--inputs` JSON; default: the current repo).
2. Run: `flywheel workflow run triage-deep --execute --mode headless --inputs '{"project": "<project>"}'`
3. Report the outcome: the prioritized + assigned backlog and the spawned crew.

{{VERSION}} {{GENERATED}}
