# Learnings: doorbell plane verify + fix (bd-cr8h, 2026-09-21)

Verify operator doorbell model + GS-056 gaps at tip 3c977179, debate to
agreement (1 pi), implement via 2 pi lanes. Beads bd-cr8h / bd-2i96 / bd-zawk.

## What worked

- Two independent explore verifiers with file:line evidence before debate.
  Architecture all CONFIRMED; 11 gaps triaged to 4 live, 7 fixed/by-design.
- Debate locked scope in 2 rounds: (A) why-fold + health GC, (B) one inject
  gate + subscribe footgun. Digest-delay + subscribe-digest stayed documented.
- Lane A (0011730): why honors journal `pins` for daemon InboxFetched/Digest
  rows, keeps [journaled] label, no invented fold. Health counts sweep set.
- Lane B (6ea530c): one typed InjectGate both paths; subscribe without
  --project refuses silent store. Both closed LEGAL, doorbell fence green.

## Gotchas (do not repeat)

1. **`send --stage` dispatched:true landed:false is UNVERIFIED delivery.**
   Never fire-and-forget a load-bearing brief. Confirm ACK or re-deliver as
   pullable channel file-thread + direct herdr prompt (worked both lanes).
2. **Pin-respawn retires the pane.** A send in flight during retire misses.
   `mail whois` + retry, channel thread as fallback.
3. **Ring without inbox is normal.** Crews answered every ring by manual pull
   (channel read / job list); inbox --since last stayed empty. Healthy ring +
   empty inbox means pulls are manual, not that mail flows.
4. **Idle pane + in_progress bead + no commit = ask, don't wait.** The
   three-line check (draft? stuck? narrow scope?) unblocked Lane B in one poll.
5. **Newcomer channel post fails LOUD non-member until join.** Join first,
   then post. Presence publish needs --project.
