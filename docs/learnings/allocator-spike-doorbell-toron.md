# Allocator spike: measurement + decision (bd-pk81)

## Status API (2026-09-29)

Rust Allocator API is bare-minimum landing (PR 156882, Nia driving):
`Allocator` trait (two required methods) + `Box::new_in` / `Vec::new_in` +
`System`/`Global`, `dyn Allocator` planned. HashMap/pin/static story
incomplete, nightly-only surface, collection coverage partial. Prototype below
needed `#![feature(allocator_api)]` on nightly-2026-09-25.

## Measurement (throwaway probe, /tmp/alloc-spike, NOT committed)

Shape: 64 small-box churn per round (mimics serde_json Value trees — the
hottest alloc shape on journal + mailbox paths), 100k rounds = 6.4M allocs,
-O, Apple silicon, 3 runs:

- System allocator: 49–53ms
- Best-case bump arena (no frees, reset per round): 20ms
- Ceiling gain: ~30ms / 6.4M allocs ≈ **5ns per alloc**

## Decision: DO NOT ADOPT

1. Our rates are hundreds of allocs/sec per hot path, not millions. Saving
   5ns/alloc buys single-digit microseconds/sec — unmeasurable next to one
   `write(2)`, one herdr CLI spawn (ms), one SQLite insert.
2. Our real incidents were unbounded-growth and design bugs (14GB respawn
   loop, EMFILE storm, silent drops). Faster malloc fixes none of them;
   bounding and counting did.
3. Cost is real: nightly-only API, partial collection support, a third set of
   tuning knobs, and the doorbell daemon deliberately avoids toron-core's
   build closure — a custom allocator could not even cover the process
   uniformly.

## Revisit when

Profiling shows allocator cost above 5% of tick time on a real workload, or
the API stabilizes with full collection support AND a measured hot path
demands it. Re-run the /tmp probe shape against that path first.
