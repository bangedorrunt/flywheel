# Rex ack + deaf e2e (2026-10-24, bd-349h)

## Ack half (proven live)
- TealOtter + JadeFox on rex, isolated cwds, read own inboxes only.
- Answered correctly (factory require_herdr_for_spawn names; fallback is_transport codes).
- Ran clean `toron ack accept`, mailed captain; both replies received, cursors advanced, 0 overdue. $0.01 each.

## Deaf half (proven live after bd-fd09 fix)
- AshBadger bare-shell block, instructed to do nothing.
- Host (fresh binary 103642e1, `auto` backend) rung 6 wake lines (wm 45), zero reads.
- Fact `deaf:<npub>:69:no_answer` recorded; CAPTAIN line logged.
- Root cause of the earlier zero-ring run: observe read `pane_list` (status Unknown by
  construction on rex) instead of classified `agent_list` rows. Fix projects agent rows
  onto the pane shape; bindings verify via rex label + session id.

## Evidence
- `/tmp/e2e-doorbell8.log` (CAPTAIN line), sandbox facts at `/tmp/e2e-rex/flywheel/facts/flywheel-inbox.jsonl`
- Markers: `/tmp/bd-ack-e2e-done.txt`, `/tmp/bd-rlvb-test-done.txt`
