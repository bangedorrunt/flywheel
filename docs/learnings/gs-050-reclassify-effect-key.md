# GS-050 — Reclassify journal `effect_key` honesty

**Date:** 2026-09-16 (Melbourne UTC+10)
**Ship:** HOLD merge · branch push only · no PR
**Base:** flywheel main `ed1fca3f9af02e381a4841ba02e74cba9f33f73a`

## Residual closed (FM-049-BAR + adversarial HIGH)

GS-049 documented that tip `Cmd::Reclassify` had **no** `effect_key` — journaled keys lived only on rebind / workflow effect paths. Tip `b4628d3` added a Cmd field but expand matched `_` (Cmd/stderr theater). Autofix makes `effect_key` a durable I8a journal identity.

## Fix

1. `Cmd::Reclassify` carries `effect_key: String`.
2. `reclassify_effect_key` → `ReclassifyIntent::derive_effect_key` — length-prefixed / tagged encode (`gs050-reclassify-v2`); `None` ≠ `Some("")`; no raw `\0` concat.
3. Host `expand_reclassify_cmds(project, …)` **uses** the key: `insert_or_load_reclassify` → `commit_reclassify` journals Prepared → InFlight → Settled|Failed under that primary key (agent_ref, flywheel event log — not mailbox). Retries bump attempt, same `effect_key`.
4. Decide stays outside pure `tick` (Tick Law).
5. Tests: journal PK / idempotent resume; collision; same-log + post-effect retries.

## H1 verdict — keep GS-048 brief

> **Superseded by GS-052 (hybrid brief):** captain later chose name+family+selection+run+topology(+compose_token) as the single live `--brief` contract; topology retained for sticky-open. Historical H1 text below kept for audit.


Investigate family/selection/run compact (catalog-review mid-work) vs name/trigger/topology:

- **Keep GS-048** (`name` / `trigger` / `topology`) as the SINGLE live `--brief` contract.
- Family/selection compact omitted **topology** (needed for sticky-open mid-session reclassify) and was early-return shadowed (never live). Finishing it as brief winner would regress the GS-048/049 session-routing bar or bloat brief into near-full-list.
- Skill-catalog pickup (family/selection/run) remains on bare `workflow list --json` / `workflow show`.
- Live projection: `workflow_list_brief_rows` (what `cmd_workflow` prints). Dual `--robot-capabilities` `workflows` key fixed. `.agents/skills/**` unchanged this tip.

## Rejected

Redis / OTP / new kinds / MCP / invent-green skills sync / invent-green brief dual path.
