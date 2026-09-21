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

## Live e2e outcome (bd-46qy, 2026-09-21)

Proven live: G8 subscribe refusal, G1 why-shows-daemon-rows, XDG cutover
(pid 80758 via launchd respawn, hosted, seq advancing), urgent ring now
reaches a real digest (bd-v8vi fix, digest line 04:59:44Z). New bugs filed
from probing: bd-v8vi (urgent audit-blackhole, fixed 63d311c), toron
channel-plane incident bd-pwja (open). Process scars: reinstall the binary
after landing (stale CLI lies); old daemon ignored SIGTERM, kill -9 +
launchd respawn recovered; `doorbell restart` adopts instead of restarting,
use stop --force or kill + start.

## Cross-repo closeout record (bd-pwja)

Toron channel-plane incident, fixed toron-side, recorded here so the
flywheel ledger binds to a real local commit. Root cause: bus cap-prune
evicted 39000/39002 room-state under kind-9 flood. Fix: toron@a54f953
(bd-fxzv closed LEGAL toron-side, 22/22 event_store green, clippy clean).
Live verified: channel list + read show steer-bd-cr8h with 8 posts.
Ceremony repair: prior gate rows cited a phantom sha (b702b8b, in neither
repo); superseded by the gate bound to THIS commit. ack_overdue_only was
already shipped (toron@e708aa8, branch at messaging.rs:258); my duplicate
bd-pt4a was a bad grep, closed as invalid.

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
