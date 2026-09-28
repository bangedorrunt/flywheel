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

## Correction: the rate was NOT the shed loop

An earlier version of this file named the unthrottled `FrameAction::Shed` arm as
**the** source of the rate, "by elimination". That was wrong, and the reason is
simple: **the shed arm does not emit `Died` at all.** It keeps the cursor and
reconnects; it reports nothing, so it cannot produce the rows the storm is made
of. Elimination reasoning is only as good as the candidate list, and mine had
the wrong candidate on it.

The shed arm is still a real hole and is now guarded (audited per resync,
throttled by `RESYNC_FLOOR`), because a loop that reconnects with no delay and
no row is unbounded and invisible whether or not it caused this storm.

What the rows say instead: the storm's unit is a **failed enqueue that reports
itself as a death** — `deliver_died_blocking` failing because the brain channel
was full, and the failure re-reporting a death, which `7cdb927` fixed. That is
how 620 deaths/second coexist with a daemon at 3% CPU and no socket audit: the
loop never touches the socket.

## What is fixed

1. **The amplifier: cut.** The brain wrote a journal row per `Died` **message**,
   including the 2,112 in that minute that produced no action and
   `executed.restarted: false` — 2,120 rows for 8 real restarts, 265 rows of
   noise per restart of truth. An inert death is now a COUNT, journaled at
   1, 2, 4, 8 ... (`coalesced_deaths` names the deaths each row stands for), so
   a storm costs `log2(deaths)` rows and still accounts for every death.
   Red first: 4,096 inert deaths wrote 4,096 rows; now ≤ 16.
2. **The silent path: guarded.** The `Shed` arm audits one row per resync
   (`wake_sock_shed`, with `resync_from` and a running `resync_index` so a loop
   reads as a rate) and waits `RESYNC_FLOOR` (500ms) before reconnecting, which
   bounds it at 2 resyncs/second instead of socket speed. Proven by neutering
   each half in turn: without the sleep, 3 sheds took 1.4ms; without the audit,
   0 rows.
3. **Line 1710 is still open.** `WakeOutcome::Died => deliver_died_blocking(&tx)`
   in `crates/doorbell-rt/src/file_host.rs` is unconditional: no counter, no
   single-flight, unlike the sibling arm at 1686 that `7cdb927` guarded. It is
   the only other `Died` producer in the process besides 1686.

## Lost rows: a seq is allocated before the write

The storm cost data, and this is how. `doorbell-journal-flywheel-opencode.jsonl`
is contiguous through 4,633 sequenced rows except for one hole: line 3256 is
seq 153410 at `07:32:24Z`, line 3257 is seq **153427** at `07:32:41Z`. Sixteen
seq values exist in no file on disk (checked across every journal in the
directory), and nothing in the corpus is missing a `seq` field.

Both sides of the hole are the **`Sweep` cadence at one row per second**. So the
sweeps kept ticking through a 17-second window and each tick consumed a seq and
left nothing behind. The mechanism is in `brain.rs`:

```rust
let seq = self.next_seq;
self.next_seq += 1;
let _ = self.store.append_journal_seq(...);   // error discarded
```

The seq advances before the write, and the write's error is thrown away. Any
failing append therefore burns a seq permanently and reports nothing in the
moment; the gap is the only trace, it is unexplainable from the journal, and
`/health` reduces it to a single boolean (`journal_torn_tail`) that also stops
the fold at the tear, hiding everything after it from `doorbell why`.

The unwritable window is most plausibly rotation: `rotate()` rewrites the active
file with `std::fs::write(&active, tail.join("\n") + "\n")`, which is a
truncate-and-rewrite from a snapshot, not an append.

**Correction to an earlier version of this section.** It said the journals
contain no rotation row despite `rotate()` claiming "Every drop is an audit
row". That was wrong, and wrong the same way as the shed claim above: I grepped
the **journal** and concluded the audit did not exist. Rotation records live in
the **audit sidecar**, `doorbell/<workspace>.audit.jsonl`. They are there:
530 rows for flywheel-flywheel, 370 for flywheel-toron, 10 for opencode.

And they settle the opencode gap by exclusion: every opencode rotation record is
dated **2026-09-28 05:33 and 05:47** — hours AFTER the 07:32 gap it would have
to explain. No record covers that range, so those 16 seqs were not retention.
They were a lost write.

chiebukuro is the opposite case and the worse one: **89 gaps, one of them 5,175
seqs**, four generation files on disk (so rotation ran at least four times), and
the sum of its recorded `dropped_lines` is **zero**. Dropped rows exist there
with no record at all.

So the two halves have different causes: a designed drop that the record cannot
be tied to (no seq range in it, so the fold cannot tell it from corruption), and
a lost write whose only trace is the gap.

## Re-deriving it

Journals are `${XDG_STATE_HOME:-~/.local/state}/flywheel/journals/doorbell-journal-<workspace>.jsonl`,
one JSON object per line, `msg.rt_kind` naming the dispatched message and
`actions` the effects it produced. Rotation is a 512 KiB gate with numbered
archives, so the 2026-09-27 windows survive only as `.N` files; re-derive soon
or not at all.

Count `Died` rows per minute and per second. A `Died` row with an empty
`actions` list is inert: it changed nothing and still wrote a row.
