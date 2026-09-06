# ADR-0010 bead graph (grill → create)

Vehicle: `workflow run beads` / beads-lifecycle until=integrate, executed in-pane
(open-mode planner). Not implement. Not wave-swarm.

## Waves

| Wave | Dns | Loop label |
|------|-----|------------|
| 0 | D0 | `wave-0` |
| 1 | D1, D3 (parallel); D2←D1; D4←D3; D14←D3 | `wave-1` (held while wave-0 open) |
| 2 | D5, D6, D7, D8, D9, D12, D13; D10←D6+D3; D15←D6 | `wave-2` (held while wave-0/1 open) |
| — | D11 beads successor ADR (report) | none — independent, other repo |

Wave labels plus `--wave N` so both loop `--wave-label wave-N` and typed field work.
Explicit `blocks` deps document intra-wave order; D0 is also an explicit blocker
for every wave-1 bead so `br ready` stays honest without the loop.

## Non-goals (do not file)

Worktrees, filesystem inbox, second watcher, flywheel-only beads schema,
invented principles, subject-as-close-signal shims.

## Review gates after create

`br dep cycles` empty. Each P≤1/2 has `--verify` + `--principle`.
`br lint --json` clean. `br plan --json` shows two tracks: D0 then D11.

## Filed ids (2026-09-07)

| Dn | id | wave | ready now |
|----|----|------|-----------|
| D0 | `bd-aucr` | 0 | yes |
| D1 | `bd-w98c` | 1 | after D0 |
| D2 | `bd-8jlv` | 1 | after D0+D1 |
| D3 | `bd-s8bo` | 1 | after D0 |
| D4 | `bd-8j7m` | 1 | after D0+D3 |
| D14 | `bd-qpub` | 1 | after D0+D3 |
| D5 | `bd-65he` | 2 | after D0+D3 |
| D6 | `bd-f14d` | 2 | after D0 |
| D15 | `bd-tl65` | 2 | after D6 |
| D7 | `bd-w5fn` | 2 | after D0 |
| D8 | `bd-d8e4` | 2 | after D0 |
| D9 | `bd-l0fz` | 2 | after D0 |
| D13 | `bd-eo39` | 2 | after D0 |
| D10 | `bd-67qg` | 2 | after D3+D6 |
| D12 | `bd-fmya` | 2 | after D0 |
| D11 | `bd-jtnn` | — | yes (docs, beads repo ADR) |

Ready set: `bd-aucr` (D0) and `bd-jtnn` (D11). Cycles: 0. Lint: 0.

## Live-use beads (HarborPilot retrospective 2026-09-07)

No new Dns. No wave (parallel with D0). Label `live-use`.

| Finding | id | p | Notes |
|---------|----|---|-------|
| F1 captain auto-bind | `bd-de9c` | 1 | toron |
| F2 roster recency/prune | `bd-c29i` | 2 | toron |
| F5/F6 + corr re-use honesty | `bd-fhf0` | 1 | toron mail-surface |
| F3/F4 bootstrap templates | `bd-jrlr` | 2 | pin; contract already in tpls |
| verify-0005 drift | `bd-p0ed` | 2 | ADR-0005 D5 heading + swarm.tpl |
