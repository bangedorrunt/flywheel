# A backoff is not a rate limit without single-flight

The 2026-09-27 doorbell incident, and the general shape it belongs to.

## What happened

`doorbell serve` ran at 184% CPU and 679 MB RSS. `flywheel-flywheel.audit.jsonl`
grew at 169 KB/s — 1,408 identical `wake_sock_absent` rows per second, counted by
timestamp bucket. Each burst crossed the 5 MiB cap, so the rotation rewrote the
whole file continuously. The toron daemon ended up at `Too many open files` with
234 fds on `wake.sock`, its accept loop dead, and the socket file present but
refusing connections.

The loop was not the backoff, and it was not the retention cap. Both were correct
in isolation. The defect was between them.

## The rule

**A backoff spaces ONE retry. Without a single-flight guard, nothing stops a burst
of failures from each scheduling its own retry — so N failures become N timers
that all fire together after their sleeps.**

`restart_wake` had a correct escalating backoff (1, 2, 4, 8, 16s, capped) and no
"is one already pending" flag. Every `Died` scheduled another task. That is why
1,408 attempts/second coexisted with a 16-second backoff: the backoff was never in
the path, because 1,408 tasks each slept their own 16 seconds and then all
connected anyway.

Two consequences worth stating separately:

- **A bounded writer is necessary and not sufficient.** A size cap converts
  unbounded *disk* growth into unbounded *CPU*. The write *rate* needs its own
  bound. The retention fix I added earlier became the amplifier of the bug it was
  meant to prevent.
- **Two services took each other down.** The doorbell's retry storm exhausted the
  toron daemon's descriptors, killing the socket the doorbell was polling. Then
  `wake_sock_absent` correctly reported the socket absent, forever. Neither
  service was wrong on its own; the *interface* had no rate limit on the client
  side, and a server cannot distinguish a retrying client from an attack.

## The fence, and the shape of a good one

The witness is `WatchRetry::fails()`, because `record_fail` is only reached when a
restart is actually *scheduled* — so the counter reads how many got past the guard.

- guard removed → 51 deaths scheduled 51 restarts (asserted 1)
- flag never reopens → a death after the first task scheduled nothing (asserted ≥2)

The second one matters as much as the first. Single-flight without reopening is a
one-shot, which trades a storm for permanent silence. Silence is the worse failure:
never-deaf beats wake-blind, so the retry must go on forever *and* come back fast.

## The generalisable form

- A backoff spaces one retry. Add single-flight, or it is not a rate limit.
- Bound the write *rate*, not only the size. Otherwise retention becomes the
  amplifier.
- A client that can out-connect a server can exhaust that server's descriptors.
  The rate limit belongs on the client side of a shared resource.
- A guard that cannot reopen is a one-shot, and a one-shot is a different bug with
  the same symptom.

## Ledger note

This work was committed citing `bd-lorot`, **which was never a real bead**. The
commit guard checks that a `bd-*` token appears in the message; it does not check
that the bead resolves, so the gap was invisible to it. The real bead is
**`bd-6rtl`**, and the six commits that say `bd-lorot` are the ones that carry the
implementation. A citation that does not resolve is worse than no citation,
because the ledger reads as authoritative and is not — so the id mismatch is
recorded here rather than quietly tidied away.
