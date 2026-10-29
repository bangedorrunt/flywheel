# bd-context-handoff-064d: coverage-ranged handoff (LANDED primitive + wiring follow-up)

## Landed (lane 3, this change — `src/workspace.rs` only)

Pure primitive, Tick-Law clean (no I/O, clock, or env in the reducer):

- `HandoffRow` — auditable row keyed `(project, pin, bead)` in
  `WorkspaceModel::handoffs`: `from_run` (last seen, inclusive) / `to_run`
  coverage, `strategy`, `artifact` path, `consuming_run`.
- `handoff_delta(row, runs)` — default return is `DeltaSinceLastSeen` (only
  runs after `from_run`); missing or stale row (`to_run`/`from_run` unknown to
  the current run list) falls back to `FullBrief` and names the reason.
  A current pane yields an EMPTY delta — the resume sends nothing.
- `resume_brief(row, runs)` — ONE primitive for both the respawn resume and
  the session-pickup resume brief. Renders the strategy + exactly the runs
  that travel; the fallback names itself as a full brief.
- `Msg::HandoffRecorded { row }` — sole writer of the map. Last writer wins,
  emits nothing.
- `resume_runs_for(model, project, pin, bead, runs)` — effect-layer entry
  point: look up the stored row, return the delta.
- `record_handoff(...)` — stamps a row with the delta strategy.

Tests (in `workspace.rs`, run with the bin fence): `handoff_delta_covers_only_unseen_runs`
(the bead VERIFY name), missing-row fallback, stale-row fallback.

## Wiring follow-up (NOT lane 3 — needs `durable.rs` / `main.rs`, other lanes)

1. `respawn_and_requeue` (`src/durable.rs`, context_limit path): after the fresh
   pane boots, call `resume_runs_for` with the bead's run list and requeue the
   DELTA brief instead of the full original text. Record the row via
   `Msg::HandoffRecorded` (or the journal equivalent) with `from_run` = last run
   the dead pane saw, `to_run` = newest at record time, `artifact` = brief path.
2. Session-pickup (`pickup` workflow, `resume-pickup` stage): build its resume
   briefs through `resume_brief` instead of the ad-hoc brief builders — this is
   the "reuse one primitive" half of the bead.
3. Run source: callers supply `&[HandoffRun]` (id + seq, oldest first) projected
   from the run ledger (`workflow history` / `runs list` surface). The primitive
   deliberately does not pick the source — one projection, reused by both readers.

Until (1) lands, respawn still requeues the full text: the primitive is proven,
the effect wiring is the remaining half. Do NOT close the bead on the
primitive alone — VERIFY passes, acceptance needs the respawn read.
