{{TOOL_FRONTMATTER}}
# Workflow: captain-audit

Run the captain-audit workflow: read-only analysis of a topic within a scope — scout gathers evidence, the oracle synthesizes the report, the reviewer challenges it. ANALYSIS ONLY: no beads, no repo edits; the report is written to `report_path`.

## This command's focus

1. Gather `topic`, `scope`, and `report_path` from the user's message (or the `--inputs` JSON; `report_path` defaults to `runs/captain-audit-<ts>.md`).
2. Run: `flywheel workflow run captain-audit --execute --mode headless --inputs '{"topic": "<topic>", "scope": "<scope>", "report_path": "<path>"}'`
3. Report the outcome: where the report was written and the reviewer's verdict.

{{VERSION}} {{GENERATED}}
