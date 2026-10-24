# Live matrix: fresh binary with full parity code (2026-10-24)

Bead: bd-gf6l. Binary: flywheel 0.8.0 rebuilt from main (`install --path .
--force --features backend-superlogical`), no codesign needed.

## Matrix (installed binary, live services, approval-free)

| Case | Result |
|---|---|
| `auto backend list` | auto row present, active, ready; herdr + superlogical ready |
| `auto backend status` (live rex) | multiplexer healthy via rex |
| `auto` + `REX_BIN_DIR=/tmp/empty-rex-dir2` | multiplexer healthy via herdr fallback |
| pinned `superlogical` + missing rex | multiplexer unhealthy — fails loud, no silent fallback |
| fixture sessions after run | zero remaining (`fixture-proc` created and killed) |

## Doorbell triage (read-only)

- Daemon hosted, up, 0 dropped, 0 events_lost, watch alive, no restart loops.
- herdr `agent list` is EMPTY: no live fleet, so there is nothing to wake.
- 44 unanswered pins accumulated with zero live panes: lane debris, not
  breakage. Left untouched (live daemon state; reaping is its job).

## Not covered here (needs approvals or spend)

- Wake ring to a rex block (needs toron fixture pins — store approval).
- Real opencode crews (spend; cheap-model rule recorded).
- `__send` to rex via registry (needs fixture project plumbing).
