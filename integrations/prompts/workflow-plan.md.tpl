{{TOOL_FRONTMATTER}}
# Workflow: plan

Run the plan workflow: goal → bead-ready plan via the planner.

## This command's focus

1. Gather the goal from the user's message (or the `--inputs` JSON).
2. Run: `flywheel workflow run plan --execute --mode headless --inputs '{"goal": "<goal>"}'`
3. Report the plan outcome: the decomposition the planner produced (with the approval gate noted if it parks for review).

{{VERSION}} {{GENERATED}}
