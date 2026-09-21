# Learnings: ADR-0014 Jev steer (bd-h3nj, 2026-09-19)

Debate → implement → fresh-eyes → close. Bead bd-h3nj, commit ac750fc.

## What worked

- 1-pi adversarial debate produced real contract fixes (G1–G7, L1–L8, I1–I7).
  Sharpest: sticky-open bypass by proxy (topology derives from primitive).
- Landing I1–I7 as normative ADR text before code made implementation mechanical.
- Fresh-eyes review caught 5 real blockers, including a NaN→max-confidence hole.
- Flag default-off, byte-identical passthrough: 131 workspace tests green untouched.
- Doorbell daemon alive and attached the whole session; no missed wakes.
  Channel post/read solid, long posts auto-chunk.

## Gotchas (do not repeat)

1. **Glob semantics:** `src/**/*.rs` does NOT match top-level `src/*.rs` in
   toron's fnmatch (star needs a literal `/`). A guard `Unreserved` block is
   probably your glob, not the guard. Diagnose with `reserve conflicts` +
   `reserve list --path` before forensics. Always acquire both levels.
2. **`send --to` misses fresh pins** (registration race) and the pane-id form;
   fall back to `herdr pane` primitives, or retry after settle.
3. **`add --kind <x>=1` can birth an undriven shell**; `herdr agent start` may
   time out there. Close the dead pane and pivot to a loaded crew member
   (code-unseen eyes still count as fresh on a new artifact).
4. **Briefing vs explicit topology:** briefing said open, operator said steer
   1-pi. Announce `vehicle: steer` on operator authority; sticky-open holds
   against invention, not against explicit orders.
5. **No jev/typesafe skill installed.** Grounded via TypeSafe docs webfetch.
   Consider a `typesafe` skill (API shapes, jaggedness, pack authoring).
6. **Pristine-tree comparisons:** HEAD `Cargo.lock` is stale in this env and
   mbx refuses `/tmp` worktrees — use the stash dance, then pop immediately.
7. **Test/toolchain drift is real:** installed `br` fail-closed rejects old
   helpers (filed bd-xdr0); retired `--confirm` breaks cron test; clippy 8/7
   pre-existing (bd-m0go). Scope loud, file follow-ups, never invent-green.

## Enable-readiness (bd-n86n)

W2 stays behind `FLYWHEEL_JEV` (off) until: POST moves outside the commit tx,
latency metric lands in the audit row, key custody + spend ceiling locked,
pack re-validation CI exists.
