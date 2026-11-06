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
