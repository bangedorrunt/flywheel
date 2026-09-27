# Retention must cover the highest-volume file, and a counter feeding an alarm is a false alarm forever

Recorded 2026-09-27. Incident, mechanism, blast radius, and what is not fixed.

## The shape of the mistake

The doorbell had **two** append-only logs per workspace. One had a complete
retention policy; the other had none. The one with a policy was the low-volume
one.

`store.rs` gave the journal everything: a 512 KiB rotate gate,
`RETENTION_LINES` 5000, `RETENTION_GENS` 4, `RETENTION_BYTES` 5 MiB, spill into
generations, and an audit row for every dropped line.
`file_host::append_audit_row` was `OpenOptions::append` + `writeln!`. Nothing
read its size.

That is backwards, and the reason is worth stating plainly: **the audit log is
the high-volume file, because it records every per-attempt failure.** It is
precisely the log a repeating fault writes to. The journal that *was* bounded
held 4,705 rows while the audit log next to it held 65,512,446.

## What it cost

A stale Unix socket with no listener made every watch connect-fail. The doorbell
called it "sidecar absent", restarted, and repeated every ~16 seconds. Nothing
named the cause, because the two silent death paths in `run_wake_loop` wrote no
audit row at all.

| file | size |
|---|---|
| `flywheel-toron.audit.jsonl` | 14 GB |
| `flywheel-flywheel.audit.jsonl` | 11 GB |
| `flywheel-beads.audit.jsonl` | 1.4 GB |
| `flywheel-chiebukuro.audit.jsonl` | 366 MB |
| **total doorbell state** | **26 GB** |

65.5 million rows, one of them distinct.

## The second, quieter one

`known_journal_workspaces()` counts files in the journals directory. That count
*is* `known_workspaces`, and it feeds the standby wake-blind signal. So 913
journal files for workspaces the plane had served months ago were not merely
20 MB of stale bytes — they were a **permanent false alarm** in `/health`
("a fleet expects rings I will not deliver") for a fleet that no longer
existed. A counter feeding an alarm must be bounded, or the alarm is a
permanently lit fixture and stops meaning anything.

## The two policies, and why they differ

- **Audit log: bounded by SIZE.** An audit row for a live workspace is worth
  keeping at any age. Cap 5 MiB, keep the newest 4,000 rows.
- **Journals: pruned by SCOPE and AGE.** A journal for a dead workspace is
  worthless at any size. Delete only when the workspace is not hosted *and* the
  file is older than 7 days.

Neither policy is the other one. Reaching for "rotate everything" would have
deleted wake records; reaching for "never delete" is what produced 26 GB.

## Three things that only worked because they were checked

1. **Read the tail, never the whole file.** Trimming a 14 GB log by reading it
   into memory would OOM the daemon that exists to stay up. Seek to
   `len - MAX`, drop the partial first line, write to a temp, rename.
2. **A partial tail row is corruption.** The next `append` concatenates onto
   it. Every retained line is newline-terminated.
3. **Rotation must keep the NEWEST rows.** An audit log whose rotation keeps the
   beginning cannot diagnose a live fault. This direction is fenced, not
   assumed — a probe that kept the oldest rows was caught red.

## Deleting live state needs two independent guards

The journal prune deletes files. Both of these had to be falsifiable
separately, and both were:

- **not currently hosted**, and
- **older than the age threshold**.

The age gate is what makes the prune safe: a just-spawned crew whose host loop
has not written yet is never a candidate, so a live workspace's wake record
cannot be removed. Removing either guard is caught by the same test. An
unfalsifiable guard is not a guard.

## A fixture that nearly became a false finding

The first prune test passed a logical `now` in 2033. A file written seconds
earlier therefore looked decades old, and the prune removed two files instead of
one. The assertion failure was **correct**; the fixture was wrong. Time-based
retention tests must derive their timestamps from the real clock, or they assert
a relationship that does not exist.

## Not fixed

- A stale socket with no listener is still indistinguishable from an absent
  one, and the daemon does not rebind on start. The loop was cleared by a
  restart, not by a fix.
- Silence is still treated as death. Debatable; not changed in a drive-by.
- 9 stale heartbeat stamps are still tracked as if live.
