{{TOOL_FRONTMATTER}}
# Workflow: onboard

Run the onboard workflow: cold-start a newcomer — map the repo, seed context, propose the first task.

## This command's focus

1. Gather the project from the user's message (or the `--inputs` JSON; default: the current repo).
2. Run: `flywheel workflow run onboard --execute --mode headless --inputs '{"project": "<project>"}'`
3. Report the outcome: the repo map + the seeded first task.

{{VERSION}} {{GENERATED}}
