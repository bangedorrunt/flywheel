# A Died storm is reproducible: 620 per second, three workspaces, two days

**Status:** reproduced and measured 2026-09-28. Partly fixed, and the part that
is not fixed is named at the bottom rather than guessed at.

## What was seen

The doorbell journal records one row per `Died` message the brain dispatches.
Three independent storms are in the retained journals:

| journal | when (UTC) | `Died` rows | inert (no action) | worst minute | worst second |
|---|---|---|---|---|---|
| `doorbell-journal-flywheel-chiebukuro` | 2026-09-27 07:32–07:38 | 4,877 | 0 (0%) | 4,226 | **620/s** |
| `doorbell-journal-flywheel` | 2026-09-27 07:29–07:53 | 3,383 | 0 (0%) | 1,241 | 179/s |
| `doorbell-journal-flywheel-flywheel` | 2026-09-28 04:25–05:15 | 2,169 | 2,155 (99%) | 1,717 | 127/s |

Two days apart, three different workspaces, no single event. The worst is
chiebukuro, where `Died` ran at 620 per second and 4,226 rows landed inside one
minute.

## Why it was invisible

The socket's own audits are deduped to one row per outage on purpose: the
pre-dedupe storm wrote 1,613 identical `wake_sock_absent` rows in one second.
So a `Died` storm is *not* accompanied by socket audits, and in the 2026-09-28
burst window the only audit kind present was `registry_ambiguous` (95 rows).
`doorbell why` read calm while the journal filled.

That is the shape worth remembering: the loudest thing in the journal had the
quietest audit trail.

## What is already fixed

- **The metric.** `/health` counted the string `RestartWatch`, so a storm of
  deaths read as a storm of restarts. It now carries the restarts the gate
  actually took (`7096fd9`), and the 2026-09-28 storm shows the gate working:
  of 2,169 `Died` rows only 14 produced a restart, and 2,155 produced no action
  at all.
- **The full-channel cascade.** A watch line that could not be enqueued used to
  report a death per line. It now reports one death per consumer and counts the
  rest (`7cdb927`).
- **The 2026-09-28 storm stopped at the deploy that carried `7cdb927`:**
  2,168 `Died` rows before 04:33:34Z, one in the 38 minutes after.

## What is NOT fixed

1. **The rate: found by elimination, and it is the silent path.** A row per
   `Died` message means 620 rows/s is 620 `Died` messages a second. `run_wake_loop`
   has exactly one exit that loops with **no delay and no audit row**:
   `FrameAction::Shed` sets the cursor to `resync_from` and reconnects
   immediately. Connect failure, handshake failure, EOF, read error and
   deaf-silence are each audited first; the shed arm is not. The evidence picks
   it out:

   - Across all three storms: **9,741 `Died` rows and not one socket audit row**.
     Every audited way the reader ends accounts for those rows and none fired.
   - Delivered `Watch` lines were 2, 3 and 4 against 4,877, 3,383 and 2,169
     deaths. Lines kept arriving and almost none landed: a full channel being
     re-fed, not a busy bus.
   - Nothing in the replay path sleeps, so it can run at socket speed. That is
     what 127/s, 179/s and 620/s look like.

   So the shape is a resync loop re-feeding the consumer faster than the brain
   drains, where each line that cannot be enqueued costs a death. It is not
   proved by a replay trace, because failed lines are never journaled and the
   arm that loops writes nothing. Closing that gap is the work: audit the shed,
   and throttle the resync. Then a recurrence is a measurement, not an
   inference.
2. **The amplifier.** The brain writes a journal row per `Died` **message**,
   including the 2,112 in that minute that produced no action and
   `executed.restarted: false`. A `Died` that changes nothing still costs a row,
   so the journal amplifies noise 265:1 against truth. Contrast the metric,
   where counting only real restarts was exactly right.
3. **Line 1710.** `WakeOutcome::Died => deliver_died_blocking(&tx)` in
   `crates/doorbell-rt/src/file_host.rs` is unconditional: no counter, no
   single-flight, unlike the sibling arm at 1686 that `7cdb927` guarded. It is
   the only other `Died` producer in the process besides 1686.

## Re-deriving it

Journals are `${XDG_STATE_HOME:-~/.local/state}/flywheel/journals/doorbell-journal-<workspace>.jsonl`,
one JSON object per line, `msg.rt_kind` naming the dispatched message and
`actions` the effects it produced. Rotation is a 512 KiB gate with numbered
archives, so the 2026-09-27 windows survive only as `.N` files; re-derive soon
or not at all.

Count `Died` rows per minute and per second. A `Died` row with an empty
`actions` list is inert: it changed nothing and still wrote a row.
