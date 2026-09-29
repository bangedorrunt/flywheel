# Pane settle fix verified (bd-ietv, 2026-09-29)

The defect: `steer` goal-workflow runs bailed `no crew pane for @oracle`
even though the crew had spawned the pane — a spawn/list race at dispatch
time. The fix landed as 3616cfd under this bead's id: dispatch settles
for a just-spawned pane instead of bailing, with
`settle_role_waits_out_a_pane_herdr_has_not_listed_yet` pinning it.
`test -p flywheel pane_dispatch` is green (2 passed).

No change in this commit beyond the record. Note: src/pane.rs carries an
unrelated uncommitted morning diff (model-routing cleanup in
spawn_crew_into) — left untouched per the xaw8 lesson.
