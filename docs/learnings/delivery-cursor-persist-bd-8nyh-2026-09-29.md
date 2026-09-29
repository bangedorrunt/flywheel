# DeliveryCursor persistence (bd-8nyh, implemented as toron bd-aqvg)

Flywheel bead bd-8nyh asked for restart-safe delivery cursors. The store lives
in toron, so the build landed there as toron bead bd-aqvg, commit d4c5073.

## What changed (toron repo)

- Migration 0016 `delivery_cursors` (session PK, delivered JSON, cursor,
  updated_us) plus save/load/prune on WorkflowEffectStore.
- `DeliveryCursorStore`: snapshot (drops in_flight), merge restore (union +
  max, never shrinks), dirty flag on new marks, boot-load claim.
- `WorkflowHost`: lazy boot load on first flush tick, dirty-gated persist at
  tick end, 90d idle prune. Journal-less hosts skip silently.

## Honest semantics

Loss was already impossible (mark-after-success, pre-delivery journal).
Restart duplicates are now bounded to marks after the last saved tick, not the
full history. Pruned sessions (>90d idle) re-deliver on next sighting.

## Proof

toron-workflow 314, toron-core 794, toron-mcp 113 tests green, clippy clean,
catalog_no_internal_ids clean. Round-trip test covers save, boot load,
corrupt-row skip, journal-less skip, and the tick hook itself.
