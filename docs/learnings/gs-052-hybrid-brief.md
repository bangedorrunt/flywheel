# GS-052 — Hybrid `workflow list --brief`

**Date:** 2026-09-16 (Melbourne UTC+10)
**Ship:** HOLD merge · branch push only · no PR
**Base tip:** `6ed68e635fd9dbefd53aa969fdc968cd649beb49` (post PR#12 / GS-050)

## Context (FM-050-STEER → FM-052)

GS-050 kept GS-048 brief (`name` / `trigger` / `topology`) because family/selection mid-work omitted topology and was early-return shadowed. Captain now wants **HYBRID** as the single winning contract.

## Contract (ONE live projection)

`workflow list --brief` → `workflow_list_brief_rows` only:

- `name`
- `family`
- `selection`
- `run`
- `topology` (sticky-open airtight)
- `compose_token` (cheap boolean)

Bare `workflow list --json` / `workflow show` keep full `description` + trigger/source/crew/inputs/decide_trigger + rest.

## Rejected

Redis / OTP / new kinds / MCP / GS-046 cuts / GS-051 full packet / dual brief path / invent-Settled / invent-green docs that still claim GS-048 name/trigger/topology-only.
