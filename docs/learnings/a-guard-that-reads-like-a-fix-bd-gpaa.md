# A guard that reads like a fix but cannot fire

`bd-gpaa`, flywheel. Companion to
[order-blind-and-dead-guards](order-blind-and-dead-guards-flywheel-bd-9w95.md),
which swept for guards that *fire* on nothing. This one is the opposite shape: a
guard that is correct in intent, correct in code review, and **never executes
its own branch** — because it compares two paths that are never equal.

## The defect

`cleanup_test_data_dir` refuses to remove a test's temp dir when that dir is
where the process-global mailbox was built. Shipped as `bd-tg4n`:

```rust
if built_mail_root() == Some(dir) {
    return;
}
```

`built_mail_root()` reads `BUILT_ROOTS`, which `build_mailbox` fills with the
Mailbox **data** dir. `mailbox_roots()` computes that as
`mailbox_data_dir(&root)` = `{root}/projects`. Callers hand the helper the
**durable root** `{root}` — the value they set `TORON_DATA_DIR` to.

`{root}/projects` != `{root}`. The guard could not fire, ever.

The symptom it was meant to stop survived verbatim, ~2 runs in 12 under the
module filter:

```
mail error subscription rejected: replay failure: error returned from
database: (code: 1) no such table: bus_events
```

The SQLite pool is still open; the store file underneath it was removed by
whichever test won the race to reach `Mail::from_config` first, whose own
closing cleanup deleted its own directory.

## Why review missed it

Three separate layers all looked green:

1. The guard's *comment* described the right intent in detail.
2. The test next to it tested the **other** branch — "an unrelated temp dir is
   still removed" — and passed.
3. `bd-tg4n` closed LEGAL with a gate row and a measured flake-rate delta.

The load-bearing branch had no test, because it was **unreachable from a unit
test**: `BUILT_ROOTS` is a `OnceLock` that only a live mailbox build populates.
So the test could only ever pin the half that was already correct. The comment
even said so — "that branch is covered by the measured end-to-end behaviour
instead" — which is a rate measurement, not evidence.

**The tell was in the review surface all along.** If a guard needs a live
process to reach, and the only test near it covers its sibling branch, the
branch is not tested. That is the shape to look for.

## The fix, and the shape change that enabled it

```rust
fn dir_holds_live_store(built: Option<&std::path::Path>, dir: &std::path::Path) -> bool {
    matches!(built, Some(root) if root.starts_with(dir))
}
```

At-or-under, not equality, because the caller passes an *ancestor* of the store.
`Path::starts_with` is component-wise, so `fw-bind-stamp-XY` is not under
`fw-bind-stamp-XYZ` — a text-prefix guard would leak every sibling and look
right.

The real unlock was making the predicate **pure**: `(built, dir) -> bool`, no
`OnceLock`, no mailbox. Suddenly every branch is drivable from a unit test, and
each new case got a mutation proving it can fail (5/5 red,
`scripts/mutations/mail-cleanup-guard.spec.sh`, control green before and after).

## Instrument; do not infer

The filesystem probe (`ls` before/after, count survivors) gave **1-in-12 vs
0-in-12** — noise — and at one point suggested the fix did nothing, because
whichever test owns the store is *different every run*. Both were wrong signals.

Five lines of `eprintln!` in the helper, same method on both sides:

```
# before (equality guard)
dir=…/fw-fold-<uuid>   built=…/fw-fold-<uuid>/projects   protect=false
# after (ancestor guard)
dir=…/fw-s9-mail-<uuid> built=…/fw-s9-mail-<uuid>/projects protect=true
```

The same test owns the store in both runs; only the guard differs. That is the
whole claim, and it is deterministic.

**Rule:** when a flake's mechanism is a delete racing a reader, count the
surviving objects *at the decision point*. A survivor count is a proxy that
also catches unrelated deleters, and it moves with test ordering. Log the
predicate's own inputs and its own answer.

## The trade the fix makes explicit

The guard now skips cleanup for the store owner's directory, so **one temp dir
per test process survives by design**. That is not a leak to optimise away: you
cannot delete the directory a live pooled connection is using. Every other temp
dir is still removed (`an_unrelated_temp_dir_is_still_removed`, mutation G5 —
without it, all five protection tests pass on a helper that never touches the
filesystem).

## Generalisable

- A guard that compares two path spellings is a guard that may compare nothing.
  `==` on paths where one side is a root and the other a subdirectory is the
  common form.
- If a branch needs a live process to reach, **make the predicate pure** so a
  test can reach it. Otherwise you will ship the untested branch.
- Over-determination is not coverage. Two guards deciding the same thing means
  deleting one leaves the suite green.
- A flake rate is an outcome measure. It cannot tell you *why*, and at n=12 it
  cannot tell you whether you fixed anything.