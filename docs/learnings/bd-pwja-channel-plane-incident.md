# bd-pwja: toron channel-plane incident (2026-09-21)

## Symptom
`toron channel list` empty on flywheel+beads, reads fail not-found
(incl. steer-bd-cr8h, 10+ posts). Agent registry rows flapped
(restored same-key via bootstrap, npubs match). Mail plane + bus
watch intact, daemon up.

## Root cause
Bus store capped at 100_000 rows (`MAX_STORED_EVENTS`), prune tick
every 60s evicting FIFO by arrival (rowid). Kind-9 flood (~99,968
rows) pins the store at the cap, so the oldest arrivals — channel
room state (39000: zero rows left; 39002: one surviving member row),
manifests, profiles, reservations — were eaten first. Zero 39000
rows means deletion, not the close lifecycle (close leaves a
tombstone) and not an operator purge.

## Fix (prevents recurrence)
toron commit a54f953edc381947e978cf21aabddd102064995a (closes
toron-side bd-fxzv): cap eviction now exempts durable kinds
39000/39002/30010/30013/30031. Expiry GC still applies to all
kinds. RED-then-GREEN test
`test_cap_prune_exempts_durable_coordination_state`; event_store
22/22 green, clippy clean.

## Restore
steer-bd-cr8h room re-created; 8 surviving kind-9 posts readable
again. Verify: `toron channel list --project flywheel |
grep steer-bd-cr8h` passes.

## Deviations / follow-ups
- Restored room is PUBLIC with me (MistyCreek) as creator; original
  privacy/creator rows were evicted and unknowable. Chosen to
  satisfy the bead verify as written (no --as).
- Other evicted rooms (debate-*, hex ids) still roomless; their
  posts survive in the bus. Full restore needs archive
  reconstruct — requires captain approval, not run.
- ack_overdue_only item: already shipped at toron e708aa8
  (verified 2/2 tests green); toron-side bd-4f2e left open for
  captain to close as duplicate or keep as tracker.
