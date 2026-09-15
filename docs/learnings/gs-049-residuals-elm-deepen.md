# GS-049 — GS-048 residuals + Elm deepen

**Date:** 2026-09-16 (Melbourne UTC+10)  
**Ship:** HOLD merge · branch push only · no PR

## Residuals fixed
1. Briefing dual-surface after sticky announce → `briefing::align_with_session_topology`.
2. `steer` requires `--project` (cwd inherit via `cron::detect_project`) for MaterialContext.
3. `live_decide_snapshot.launch_pending` from `snapshot::assemble` (no invent-green 0).

## Elm deepen (GS-045 P1)
1. Source-Msg / source-action immutability — docs + same-log / post-effect retry tests (I8a). Tip `Cmd::Reclassify` has no `effect_key` yet; assert goal/reason identity.
2. `workflow evaluate` — evaluate-without-commit.
3. Phase-bearing failures — `stage` on ClassifiedFailure / journal / run ledger (`classify_with_stage`; Blocked→park).
4. Sensor→Msg vocabulary in doorbell guide — Presence via Herdr+roles (not a Msg-arm Sensor GenServer).

## Auto-fix fold (clear_for_pr:false)
1. Sticky commit dual-SoT: `align_with_session_topology` rewrites argv for **all** sticky-held invent flips (open×loop|swarm|join|none), not only swarm|join.
2. Held flag from `sticky_open_topology` (single SoT) including `parse_explicit_topology` on the briefing CLI path.
3. I8a theater removed — assert distinct payload ≠ same Cmds; post-effect DecisionReady retry keeps Reclassify identity.
4. ActionStage beyond hardwired execute when context known.
5. Presence docs clarified (Herdr+roles).

## Rejected
Redis / AgentServer / OTP / dual SoT / new kinds / new MCP / GS-046 / GS-001 reopen / invent-green settlement.
