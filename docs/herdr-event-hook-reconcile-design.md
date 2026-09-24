# Herdr event-hook reconcile design (bd-8pis, part 3 — DESIGN ONLY, not built)

## Problem

Pane↔pin bindings go stale silently: a pane dies (`pane.closed`,
`exited`, agent crash) while toron rows (presence, `mail whois`
`pane_id`, captain alias) still name the dead pane. Sends to the dead
pin deliver into a black hole; the captain fold looks held while nobody
is home. Parts 1–2 of bd-8pis warn loudly at send time and in health
output; this doc designs the reconcile that would remove the staleness
at the source.

## Hook points (herdr side)

Subscribe to three herdr events — nothing else:

- `pane.closed` — pane removed (clean exit or kill).
- `exited` — agent process exited inside a live pane.
- `agent_status_changed` — agent flipped to `dead`/`idle` (covers the
  crash-without-close case).

All three already exist as herdr concepts (status renders `dead`;
`bd-fw-interrupt-kills-agent` distinguishes kill-styles). No new herdr
API needed — a subscriber only.

## Reconcile actions (flywheel/toron side, on each event)

1. Look up pins bound to the dead pane (arrival bindings +
   `mail whois` `pane_id`).
2. For each bound pin: clear the pane binding (set `pane_id` null —
   never delete the identity row), mark presence row stale.
3. If the dead pin holds the captain alias: do NOT auto-rebind (a quiet
   human looks identical — settled rule from the arrive path). Instead
   emit one `ack-sla`-style wake so the next doorbell rings the vacancy
   loudly.
4. Journal every reconcile under the pane's death event id (idempotent
   replay: same death event never reconciles twice).

## Open questions (need operator answers before build)

1. Where does the subscriber live — toron daemon (owns the rows) or
   flywheel doorbell (owns the fleet view)?
2. Should `exited` with a nonzero code page the captain immediately, or
   is the wake-ladder enough?
3. Retention: how long do cleared bindings stay queryable for forensics?

## Non-goals

- No auto-rebind of the captain alias (explicit operator verb only).
- No presence-heartbeat synthesis — the hook clears stale rows, it never
  fabricates live ones.
- No change to the send path: parts 1–2 warns stay as the last-mile net.
