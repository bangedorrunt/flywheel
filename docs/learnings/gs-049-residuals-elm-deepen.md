# GS-049 — GS-048 residuals + Elm deepen

**Date:** 2026-09-16 (Melbourne UTC+10)  
**Ship:** HOLD merge · branch push only · no PR

## Residuals fixed
1. Briefing dual-surface after sticky announce → `briefing::align_with_session_topology`.
2. `steer` requires `--project` (cwd inherit via `cron::detect_project`) for MaterialContext.
3. `live_decide_snapshot.launch_pending` from `snapshot::assemble` (no invent-green 0).

## Elm deepen (GS-045 P1)
1. Source-Msg / source-action immutability — docs + same-log tests (I8a).
2. `workflow evaluate` — evaluate-without-commit.
3. Phase-bearing failures — `stage` on ClassifiedFailure / journal / run ledger.
4. Sensor→Msg vocabulary in doorbell guide (no Sensor GenServer).

## Rejected
Redis / AgentServer / OTP / dual SoT / new kinds / new MCP / GS-046 / GS-001 reopen / invent-green settlement.
