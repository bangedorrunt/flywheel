# GS-050 — Reclassify journal `effect_key` honesty

**Date:** 2026-09-16 (Melbourne UTC+10)
**Ship:** HOLD merge · branch push only · no PR
**Base:** flywheel main `ed1fca3f9af02e381a4841ba02e74cba9f33f73a`

## Residual closed (FM-049-BAR)

GS-049 documented that tip `Cmd::Reclassify` had **no** `effect_key` — journaled keys lived only on rebind / workflow effect paths. That was an honesty gap vs ADR-0032 I8a (retries keep source-action identity).

## Fix

1. `Cmd::Reclassify` gains `effect_key: String`.
2. `reclassify_effect_key(goal, reason, explicit, pin)` → `reclassify:<sha256>` (domain `gs050-reclassify-v1`). Attempt number is excluded.
3. `Msg::MaterialContext` tick mints the key once; same-log and post-effect retries emit the same key.
4. Effect layer logs `effect_key` on apply; `reclassify_requested` Audit detail carries it.
5. Tests: stable across retries, distinct goals diverge, post-effect DecisionReady retry keeps key.

## H1 — ONE `workflow list --brief` contract

Live path (GS-048 early-return): **name / trigger / topology**.
Deleted: unreachable family/selection compact `--brief` branch (never ran after the early return).
Fixed: dual `--robot-capabilities` `"workflows"` key (JSON last-wins invent-green of the dead contract) + docs/CHANGELOG invent-green of family/selection-as-brief.

## Docs sync (GS-048 / GS-049 tip)

- MaterialContext → Reclassify → DecisionReady
- Sticky-open airtight
- `workflow list --brief` = name/trigger/topology (ONE contract)
- Elm deepen: I8a, dry evaluate (`workflow evaluate`), ActionStage, sensor vocab (docs only)

## Rejected

Redis / OTP / new kinds / MCP / GS-046 / invent-green.
