# Live: rex-first auto backend with herdr fallback (2026-10-24)

Bead: bd-oem5. Binary: flywheel 0.8.0 rebuilt from main (`mbx install --path .
--force --features backend-superlogical`, ~4min release build, no codesign
needed — no SIGKILL on exec).

## Matrix (installed binary, live services)

| Case | Result |
|---|---|
| `FLYWHEEL_BACKEND=auto backend list` | auto row present, active, ready; herdr + superlogical ready |
| `auto backend status` (live rex) | multiplexer healthy via rex |
| `auto` + `REX_BIN_DIR=/tmp/empty-rex-dir` | multiplexer healthy via herdr fallback |
| pinned `superlogical` + missing rex | multiplexer unhealthy — fails loud, no silent fallback |
| `rex new fixture-bd-oem5` → listed → `session kill` → gone | write path round-trips, fixture cleaned |

## What this proves

- The default flip is safe on machines without rex: one failed spawn per
  call, then herdr serves. No silent behavior change.
- The fallback predicate holds live, not just in scripted tests: transport
  (`Io` from missing binary) falls back; pin mode refuses.
- `pane_keep` matching `"herdr"` exactly skips janitor work under `auto`
  (safe direction: never delete a binding the fallback may serve).

## Gotchas for the next lane

- `rex new --json` prints multi-line JSON: parse the whole stdout, never
  `head` it (a truncated parse left the fixture session alive once; cleaned
  by re-listing and killing by label match).
- `backend status` health is `agent_list().is_ok()` — a read-only probe,
  safe to run against live services any time.
