# Coordination awareness between crews (2026-11-07)

Question: while lanes run, do they know about each other's work, can they detect a
file conflict, and do they resolve it peer-first or escalate? Evidence from the
final three-lane wave (A toron envelope, B hc registry, C supervisor/collab).

## What exists (native, not aspirational)

`toron reserve` is a full conflict surface, not just a lease store:

- `acquire` (shared/exclusive) / `list --path` / `conflicts` (symmetric-glob
  report for a path set vs every other holder, JSON `conflict_free`)
- `release` / `release-by-reason <bead>` / `sweep` (closed-bead leases) /
  `force-release`
- `yield` + `yields` (structured stand-down for a bead/holder),
  `request-release` + `respond`, `transfer`
- `with-lease` (acquire, run one command, ALWAYS release, exit with its code)
- Build slots (`slots`) serialize cargo; worktrees isolate a lane by checkout;
  channels/jobs are the crew-to-crew work plane; `flywheel collab` is the direct
  ask-and-wait; captain mail + `captain watch` is the escalation door.

## Observed

- B (SwiftBay) and C (WindyCreek) each acquired reservations for their exact
  files with `reason=<bead>`: `src/state.rs`, `src/hc.rs`, `src/main.rs`,
  `src/backend/*`, `src/cmd/recover.rs`, `src/cmd/spawn.rs` (B) and
  `src/cmd/doorbell.rs`, `src/doorbell_supervise.rs`, `src/cmd/mail.rs`,
  `src/workspace.rs`, `crates/doorbell-core/src/**` (C). A live
  `reserve conflicts --as WindyCreek` over C's set returns `conflict_free: true`.
- The captain had to fence the overlap by hand: "do not edit `src/main.rs`
  (lane B owns it)" and "do not edit `src/cmd/mail.rs` / `doorbell_supervise.rs`
  (lane C)". The board shows B did hold `main.rs`; C's set never overlapped.
- **Lane A (SunnyMarsh) held no reservation at all** while editing
  `toron/crates/toron-core/*` (`holders` = BuffyLark, SwiftBay, WindyCreek).
  B and C discovered the conflict only when the shared `toron-core` build went
  red. C escalated to the captain with the failing type errors — the correct
  fallback, but for a conflict the board could have shown.
- Stale-looking leases exist from an older lane (`BuffyLark`, expiry far in the
  future); `release-by-reason`/`sweep` exist but nobody ran them.
- No lane used `flywheel collab` to negotiate; C used captain mail (allowed,
  peer-first is cheaper). No push notification exists for an overlap: detection
  is a pull (`reserve conflicts`), not a digest.

## Gaps

1. **Opt-in only.** The edit tools do not check reservations. An agent that
   skips the skill writes anyway; there is no deny at the file layer.
2. **One board, but not enforced.** Cross-repo lanes can reserve
   `toron/...` paths under the captain's project, yet nothing teaches or
   requires it by default (A skipped it entirely).
3. **No push conflict signal.** Overlap is invisible until someone asks
   `reserve conflicts` or the build breaks.
4. **Stale leases are manual.** Bead close does not release; `sweep` is a
   captain chore, and old holders can block a new lane.
5. **Build-breakage propagation is accidental.** A shared crate edit reddens
   another lane's gate; the owner learns by compiling, not by a notice.

## Improvements applied now (skill layer)

- Crew skill: one board for the whole stack (repo-prefixed paths), pre-edit
  `reserve conflicts`, exclusive leases for write paths (or `with-lease`),
  release-by-reason/sweep at close, peer-first mail/`flywheel collab`,
  escalation to the captain only when the holder is silent or the build breaks,
  and a same-message announcement when a shared crate goes red.
- Captain skill: sweep closed-bead leases and run `conflicts` on each lane's
  file set before arming; disjoint sets; a stale lease after a close is a
  captain bug.

## Proposed (not built)

- A pre-edit guard in the harness/tool layer (deny when another exclusive
  holder overlaps) so awareness does not depend on reading a skill.
- A doorbell conflict digest: on sweep, compare active reservations pairwise
  and mail both holders + captain on the first overlap of an episode.
- `reserve acquire --exclusive` refusal (not warning) on an overlapping
  write path, with the holder named; and auto `release-by-reason` on bead
  close from the tracker path.
- Cross-repo lease rows that record the repo root, so `list --repo` can prove
  which checkout a lease covers.

## Correction: leases are 180 s heartbeats, not deeds

`toron reserve acquire --ttl-secs` defaults to **180 s** ("a hold is a
heartbeat, not a deed"). `reserve renew` is the heartbeat; `with-lease`
renews every half-TTL and always releases. Live evidence at the time of
writing:

- WindyCreek's five leases were **expired** while it was blocked on the
  shared build (it had stopped renewing).
- SwiftBay's leases were **live** because its loop renews them
  (`cmd_internal_renew` / `durable::renew`).

So the auto-release the operator remembered is real, and it is the backstop
for a crashed or idle lane: a dead holder frees its claim within three
minutes. `release-by-reason`/`sweep` are hygiene, not the anti-blocker.

## Does the conflict protocol serialize lanes?

No, if it is used as written (skill updated 2026-11-07):

- `reserve conflicts` is a **read**, not a lock. A conflict means re-plan the
  file set, split the change, or `with-lease` the one shared file; it never
  means wait for a holder.
- Leases are narrow file sets, never `src/**`. Broad globs manufacture false
  conflicts (symmetric fnmatch) and are the only way this protocol could slow
  the fleet.
- Captain dispatch gives lanes disjoint file sets, so same-file overlap is the
  rare exception, not the scheduling primitive.

The real drag observed this wave was **not a lease wait**: lane A's in-flight
`toron-core` edits made the shared crate fail to compile, and flywheel
path-deps it, so B and C could not verify. That is a dependency
serialization, not a lock. Mitigations now in the skill: compile-green
slices, immediate red announcement to the captain and affected pins, and
captain sequencing (the shared-crate lane lands a compiling slice before
other lanes' verify, or runs first). Reservation waits contributed nothing
to the stall.

A future push digest (overlap mail) and a pre-edit advisory check remain
useful, but both must stay advisory; a global write lock would trade a
three-minute auto-heal for a fleet-wide stall.

## Same-repo lanes share a gate (the serialization that did bite)

Disjoint file reservations let B and C **edit** in parallel, but they cannot
**verify** in parallel: `verify-clippy.sh` and `cargo test` are crate-wide, so
one lane's in-flight `main.rs` (B) failed C's gate even after the toron
dependency went green. C's correct move was to stop and escalate; the wave
still serialized at the gate.

Options, in order of preference:

1. **One flywheel lane at a time** when both touch the same crate (today's
   model: reservations serialize, the gate enforces it).
2. **Per-lane worktree** (`flywheel worktree`) with a shared `CARGO_TARGET_DIR`
   so each lane has a clean crate and its own gate; the captain merges green
   commits. The crew skill says "no worktree" for *loop-owned beads* (the loop
   owns checkout state); a captain-dispatched parallel wave is the case where
   a worktree is the right isolation.
3. Cross-repo lanes (toron vs flywheel) still share the path dependency; a
   red shared crate is a compile dependency, not a reservation, so the
   compile-green-slice rule remains the only mitigation.

This is a scheduling fact, not a reservation-lock bug: the 180 s heartbeat
never blocked anyone; the crate-wide gate did.

## Implemented: advisory push digest (and why it is mostly a backstop)

`src/reservation_digest.rs` is a pure overlap detector (`Lease` rows in,
`Conflict` rows out; expired rows ignored, same-holder ignored, exclusive
only, symmetric fnmatch, one episode key). The embedded doorbell host reads
the board through the **same mail facade** it uses for mail (remote MCP or
in-process, not the in-process singleton) every 30 s; on the first sweep an
episode appears it:

- types one advisory line to each holder (`⟦reserve⟧ conflict with … —
  re-plan/split or request-release; never wait on a lease`),
- sends the captain a `Fact` (which the workspace fold turns into a prompt),
- appends a `reservation_conflict` audit row,
- stays quiet until the overlap clears, then re-arms for the next episode.

Live probe finding: **`reserve acquire --exclusive` already refuses the
second overlapping holder** (HolderB's acquire returned the HolderA
conflict; only HolderA held the path). So two *live* exclusive overlaps
cannot normally coexist — the digest fires only for legacy 90-day holds,
cross-spelling gaps the acquire check misses, or a check/grant race. That is
the intended order: the native refusal is the gate, the digest is the
backstop. Its detection is covered by three pure unit tests; the live board
refusal is the evidence for the stronger path.

## Residual closure wave (same day, late)

Three lanes cleared the coordination residuals in parallel; the captain
verified and, where a lane stalled at close, committed its verified tree.

| Residual | Sealed by | Evidence |
|---|---|---|
| `toron next` blind to delivered mail | `6860f18` (toron, lane A; `bd-next-envelope-ingest-yl0cx`, live-verified) | ingest of delivered mail (sender producer, rumor dedupe, SeqGap retry) + `STRAGGLER_SQL`; live: `next` returned 8 live stragglers, `ack` removed one (8 -> 7), new mail advanced the cursor 532 -> 549 |
| wake hint pointed at the high-water door | `188d98d6` (`bd-wake-hint-next-again-a7gzr`) | `pull_hint` back to `toron next`; doorbell-core 35 + doorbell tests 80 green |
| hc registry split + async labels + recover respawns the captain | `ea19c4e0` (`bd-hc-registry-recover-ywypf`, unit-test-verified) | alias-file fold + pane dedupe (one key per project), label-missing retry, `recover` skips the captain unless `--include-captain`; gate 0, recover 31, registry 75; leases released 7 |
| two doorbell host supervisors + arm not applied + collab no re-ring | `e393f0de` (`bd-supervisor-collab-rering-87gzr`, unit-test-verified) | cron-healed children are the one owner (duplicate units retired), arm recorded before proxy, collab re-rings on the shared ladder; gate 0, collab 3, supervisor 153 + 1 load flake (3/3 alone) |
| advisory push digest | `a80b8ca3` + `986fe43f` | pure `live_conflicts` + host board read through the mail facade; unit tests; live board proves `acquire --exclusive` already refuses the second holder, so the digest is the backstop for legacy/racy overlap |

Live `next` semantics verified against the operator daemon: the per-message
door returns the live straggler set on every call until the reader acks one;
ack removes exactly that message, and no message is skipped by the
high-water cursor any more.

## Live finding: an answering agent stayed unanswered

The live probe filed `bd-4jg3n` ("unanswered doorbell: SwiftBay") and SwiftBay
answered (`[lane-b] heard`) without the status clearing. Two halves, one
defect:

1. `RtHost` never overrode `Host::pin_npub`, so the trait default `None`
   reached `HearTracker::note_inject`. The ring carried no npub and
   `note_heard_pubkey` could not match the agent's own reply. (`FileHost`
   already implemented it from its npub map — a parity gap, not a design
   choice.)
2. A ring loaded from `.unanswered` at start has no pending entry at all, so
   even with (1) fixed it needed the pin→npub map seeded from the host index
   before any hear could map a pubkey back to it.

Fix `ce23c652` + `0fff560f`: `RtHost::pin_npub` delegates to the SelfIndex
built by `build_index` (`whois` per registered pin), and `Brain::new` fills
the hear map for every seeded pin. Proven on the installed binary: after a
reply the status stayed `unanswered []` past the 120 s expiry (before, the
pin was stuck for 40+ minutes across restarts).
