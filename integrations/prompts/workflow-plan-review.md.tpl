{{TOOL_FRONTMATTER}}
# Workflow: plan-review

Run the plan-review workflow: review a plan bead-by-bead before implementation.

## This command's focus

1. Gather the plan from the user's message (or the `--inputs` JSON).
2. Run: `flywheel workflow run plan-review --execute --mode headless --inputs '{"plan": "<plan>"}'`
3. Report the review outcome: every bead checked for sense, optimality, and user value — revisions proposed in "plan space" before any implementation.

{{VERSION}} {{GENERATED}}
