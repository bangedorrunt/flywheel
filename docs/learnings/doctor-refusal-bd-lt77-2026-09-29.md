# Doctor refuses the verdict-destroying rebuild (bd-lt77, 2026-09-29)

The incident: `br doctor --repair` rebuilt the DB from the issues JSONL, a
PARTIAL source of truth (issues only, never gate verdicts), and reported
`imported N, skipped 0` while emptying the verdict store for 15 of 17
closed beads.

The fix landed in the beads repo, not here: `gate_verdict_row_count`
counts the rows a rebuild would destroy, and `gate_verdict_rebuild_refusal`
refuses `--repair` naming the count (fail-closed, with an explicit override
flag). The code cites this bead; `cargo test --lib gate_verdict` is 7 green,
and the flywheel-side verify (`test workflow_catalog`) is 23 green.

No change in this repo. Recorded so the flywheel tracker reflects the
beads-side fix.
