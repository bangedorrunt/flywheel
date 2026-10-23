# Resonate → elm-effect: tranche design

The flywheel durable stack moves off Resonate onto the elm-effect framework
without breaking the running loop. Resonate stays until each tranche's exit
gate passes. Nothing here re-argues whether; it sequences how.

## Where things actually run (verified, not assumed)

- The live fold is `workspace::tick` (`src/workspace.rs`): pure,
  `(WorkspaceModel, Msg) -> (WorkspaceModel, Vec<Cmd>)`. `durable::tick` is
  NOT the live entry (ported to `workspace::tick` per bd-3wzl).
- The live interpreter is the doorbell host's `Brain::tick` mapping
  (`src/doorbell_host.rs`): `workspace::Cmd` → host `Effect`, executed by
  the doorbell-rt pump.
- Effect leaves already journal through `effect_journal.rs`
  (intent → claim → invoke → settle), and 11 leaf call sites now run a
  journaled `LeafProgram` loop instead of `ctx.run` (mail_done,
  mail_not-ready, close_check, reopen ×2, verify_dispatch, poll_inprogress
  ×2, poll_ready, release_bead_locks, reclaim_stranded), with per-project
  FileLog journals that truncate on success. Provider-nested async work
  (mail send, roster lookup) runs on its own current-thread runtimes, never
  the shared journal pool: a full-suite run completes with zero hangs where
  5 tests deadlocked before. Remaining `ctx.run` sites are excluded with
  reason (`loop_run_verify`: custom timeout + authority; `dispatch_guard`:
  in-leaf eval with caller time; `reservation-board`: pinned live).
- The toron journal (`workflow_effects`: intent/claim/settle, `may_settle`,
  `plan_attempt`) is the fenced store of record; `FileLog` journal files
  cover crash recovery for kernel loops.

## Hard constraints (do not relitigate per tranche)

1. **Resonate `ctx` is borrowed, never `'static`.** `ctx.run` returns a
   future borrowing `ctx`, so no kernel `Interpret` can hold it and no
   `Layer` service can own it. A hybrid Loop that delegates leaves to
   `ctx.run` is impossible without restructuring the driver around detached
   spawns — that restructure IS the adapter, not a shortcut around it.
2. **Never nest journal-pool work.** Fixed 2026-10: provider-nested async
   (mail send, roster lookup) runs on its own current-thread runtime via
   `submit_provider_wait`, never the shared 4-worker journal pool. Any new
   interpret path that submits to the pool from inside a dispatched journal
   future reintroduces the deadlock; route it through the nested lane.
3. **Replay must equal old checkpoints.** Every moved leaf keeps exact argv,
   exact logging, exact error shape. The guide's dual-run gate stands: the
   journaled run's outcomes must read back equal to the Resonate
   checkpoint's before the old call site is removed (old `#[function]`s stay
   registered until then, so in-flight executions still resolve).
4. **`WorkspaceModel`/`Msg`/`Cmd` already serialize.** No model changes are
   needed for journaling; only the `Program`/`Cmd` trait impls plus the
   failure mapping, which must name an outcome for every cause kind.

## Tranches (each lands independently, each gated)

### T0 — Leaf programs (DONE: 11 call sites wired, 3 excluded with reason)

`LeafProgram` + `LeafInterpret` + `run_leaf_journaled`: one durable intent
per call, FileLog journal per project, best-effort error mapping identical
to the Resonate leaf. Pattern proven on mail_done, mail_not_ready,
close_check, reopen_bead (x2), verify_dispatch.
Exit: new leaf wiring + fold parity test + production-interpret test.

### T1 — Remaining leaves (mechanical, one bead each)

Wire each remaining `ctx.run` call site the same way, except the two named
exclusions: `loop_run_verify` (custom timeout + authority injection needs
its own `Cmd` fields first) and `dispatch_guard_scan` (the eval reads caller
time; folding the fetch without the eval would silently disable the guard).
Exit per leaf: same tests as T0; old `#[function]` stays until T4.

### T2 — Durable `Loop` boot around `workspace::tick` (shadow mode)

Wrap the live fold as `WorkspaceProgram` (`Model = WorkspaceModel`,
`Msg = workspace::Msg`, `Cmd = workspace::Cmd` with per-variant durability:
data-equivalent reads `Ephemeral`, sends/dispatches `Durable` keyed by
`(bead, kind)`), and boot a kernel `Loop` on it in tests plus a
shadow driver in production: feed every live message to BOTH the existing
driver and the Loop, compare resulting command lists, log divergences, and
change nothing observable. The interpreter for shadow mode replays stored
outcomes for durable commands (never performs effects).
Exit: zero-divergence over a full soak (N polls across real projects);
`Loop::recover` converges to the live model after a kill mid-window.

### T3 — Interpret the loop for real (one executor at a time)

Move one executor arm at a time from the doorbell host's match into
`DurableInterpret::interpret_in_env`, backed by services the host builds
once per boot (store handles, herdr handles, config). Each arm keeps exact
error mapping; failures surface as `Failure::Fail` and the fold names their
message (no new `LoopMsg` variants without a replay test proving they fold
quiet). Resonate still drives the poll loop; only execution moves.
Exit per arm: arm-level tests + shadow parity holds.

### T4 — Cut the Resonate cord per pipeline

When every arm a pipeline touches is interpreted, boot the production Loop
with a `FileLog` journal and stop calling `ctx.run` for that pipeline; keep
the old `#[function]`s registered until no in-flight checkpoint references
them, then delete. Order: leaves first (done), then close path, then
dispatch path, then the poll driver itself.
Exit per pipeline: dual-run equality (journaled outcomes byte-equal to old
checkpoints on the same inputs) + crash test (kill mid-window, recover,
resume without duplicates).

### T5 — Resonate removal

Delete `resonate_sdk` from flywheel only after T4 closes every pipeline and
the gates stay green for one full release cycle. `resonate.db` checkpoints
remain readable for forensics; nothing new writes them.

## Explicitly out of scope

- Rewriting `workspace::tick` semantics (the fold is correct; only its
  driver changes).
- Durable `LoopMsg` streams with offsets in toron rows (guesswork; add a
  stream-offset column only with a same-transaction commit).
- Touching the 5 hanging journal tests' code (pre-existing pool saturation,
  fixed at the submit layer, not in the tests).

## Done when

`cargo test -p flywheel` is fully green with no skips, the supervisor and
inbox suites pass hermetic, every `ctx.run` site is either journaled or
named in T1's exclusion list with its reason, and `resonate-sdk` is gone
from `flywheel/Cargo.toml` with checkpoints retired.
