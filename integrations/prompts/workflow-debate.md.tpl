{{TOOL_FRONTMATTER}}
# Workflow: debate

Run the debate workflow: adversarial synthesis -- reviewers brief, an oracle decides.

## This command's focus

1. Gather the proposal from the user's message (or the `--inputs` JSON).
2. Run: `flywheel workflow run debate --execute --mode headless --inputs '{"proposal": "<proposal>"}'`
3. Report the outcome: the reviewer briefs (parallel fan-out) + the oracle's decision.

{{VERSION}} {{GENERATED}}
