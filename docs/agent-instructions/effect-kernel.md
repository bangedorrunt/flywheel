# Functional effect-kernel discipline

This page is the contract for reducers, journals, and interpreters. Use it while editing code. Rationale and migration history live in `docs/decisions/0016-functional-effect-kernel-redesign.md`.

## The boundary

Every live path has one shape:

```text
wire / CLI / timer / process observation
    -> parse and validate
    -> Msg
    -> pure tick(Model, Msg)
    -> Cmd values
    -> effect interpreter
    -> result Msg
```

`workspace::tick`, `doorbell-core::tick`, and `durable::tick` are reducers. They may inspect only their model and message. They must not read the environment, clock, filesystem, network, process state, randomness, locks, or mutable global state.

## Laws

**1. One model owns each fact.** The live workspace model is the composition authority. Runtime brains, durable snapshots, JSONL, and SQLite are adapters or projections. They must not independently decide the same transition.

**2. Time enters as data.** An interpreter samples wall or monotonic time and sends it in a message. A reducer never calls `SystemTime::now` or `Instant::now`.

**2a. Policy enters as data.** Host configuration is resolved once into `WorkspacePolicy` (workspace) and `LoopParams` fields `reclaim`, `pane_stall_secs`, `dispatch_guard_ttl_us`, `mail_health_escalate_at`, `pause_resurface_secs`, `walk_timeout_secs`, plus `strict` (durable loop) before the live model or workflow starts. Reducers and leaves read policy fields from the model/params; they never read environment variables through convenience helpers, and leaves receive policy on their args structs. The env resolver runs exactly once, at the construction site in `main.rs` — a reducer, tick helper, or checkpointed leaf reading `FLYWHEEL_*` directly is a review reject.

**3. Effects are values first.** A reducer emits a command; it does not perform the command. Each command has exactly one named interpreter. Unknown commands fail loudly rather than being ignored.

**4. Facts precede judgments.** Presence, delivery, run status, and provider results are observations. Jev may annotate or rank an already-filtered question, but cannot create a command, settle an effect, close a bead, or change sticky topology.

**5. Unknown stays unknown.** Timeout, crash, malformed response, missing route, and unavailable provider are typed outcomes. Never map them to `done`, `delivered`, `verified`, or `succeeded` for convenience.

**6. Queueing is not delivery.** A sink accepting work proves queue acceptance only. Provider delivery requires its own receipt. Digest, shed, unavailable, and unknown remain visible.

**7. Journal before provider.** A real effect is prepared and claimed before its provider is invoked, then settled once. Dry arms do not mint settled effect rows. An uncovered effect refuses in enforced mode.

**8. Retry preserves identity.** A retry preserves the source message identity, correlation, effect key, goal, reason, and redacted input. A retry is another attempt, not a new logical action.

**9. Determinism is observable.** Same model plus same message produces the same next model and command sequence. Output-affecting maps are ordered. New reducer paths require same-log/same-effects coverage.

**10. Bound all work.** Network, subprocess, retry, queue, digest, and recursion paths have explicit limits. A fallback is a typed transition, not an unbounded loop. The order on a remote command is fallback outside, breaker inside the retry, budget timeout around the chain, attempt timeout on the call. Interruption is not `Unknown` and not a typed provider failure.

**11. Parse at the edge.** External strings and JSON become validated kernel values at the boundary. Internal code should not repeatedly re-interpret raw identifiers or invent defaults for malformed values. Use `kernel::ActionStage` at typed call sites; retain string conversion only at stable wire/reporting fields.

**12. Close authority is singular.** Beads close beads. Toron owns mailbox and reservation truth. The host owns run/effect journaling. A worker, Jev result, or report cannot self-admit completion.

## Delivery cursors

A cursor is a persisted position that gates delivery: everything before it
is handled. It is the one durable value whose failure can be silent: a
cursor that jumps drops work with no error. The honesty laws above apply
to it with extra force. The contract for any code that reads or
advances one:

- **One cursor per consumer and scope, one owner.** The agent's toron
  `--since last` cursor and the sweep's `since_by_pin` are two consumers
  with two positions, not two copies of one. Do not add a second cursor
  path for one consumer or join two cursors with gap-skip logic, because
  that is two failure modes and no arbiter (ADR-0012 C6 rejected that
  shape).
- **Advance on what it could read, never past what it could not.**
  `inbox_high_water_from_values` takes the newest time over rows whose
  timestamp parses, and `None` when none do; `on_inbox_fetched` advances
  `since_by_pin` only on that result, so a batch it cannot read stays in
  place and re-sweeps. Re-reading is safe because redelivery is absorbed
  by the dedup lattice; the cursor itself is at-least-once, never
  at-most-once (bd-7qg6).
- **Write the position before the effect it gates.** The plane-S cursor
  is persisted (`flush_wake("watch-cursor")`) before the line is
  classified, so a crash right after cannot re-ring history. The decided
  journal row is flushed before any Prompt intent reaches a sink
  (bd-jcys); when that flush fails it is named in the audit, and the
  prompt still goes out. The cursor has already moved, so withholding
  would lose the wake.
- **Backfill from a bounded floor.** A consumer with no cursor starts at
  the 24h redelivery floor (`redelivery_floor_rfc3339`), never at
  wall-clock now: a strictly-newer-than-now fetch is empty forever and
  the high-water initializes only on a non-empty batch, so a fresh or
  state-lost pin would be deaf permanently (bd-5d0v).
- **Monotonic, and restored by merge.** `plane_s_cursor_us` moves only by
  `max`; `note_wake` is the same shape for the liveness stamp: a
  same-second or older stamp is refused, never minted. Toron's durable
  workflow cursors (`delivery_cursors`; flywheel bd-8nyh, toron bd-aqvg)
  snapshot without in-flight marks and restore by union + max, never by a
  smaller overwrite: marks are written after success and the delivery is
  journaled before it, so a restart replays only what followed the last
  saved tick.
- **Name retention, never jump it.** A position older than journal
  retention gets an explicit answer instead of a partial fold: `why`
  reports the first foldable seq, SSE reconnects get
  `Replay::Gone { first_replayable }`, rotation records what it dropped
  (`journal_rotated{dropped_lines}`), and a torn tail stops the fold
  loudly (D4.4).

## Review checklist

Before merging a change touching a reducer or interpreter, check:

- Does the diff add an ambient authority read to domain code?
- Is the new transition represented by a `Msg` and a `Cmd`, with the result re-entering as a `Msg`?
- Is there one interpreter and one journal identity for the command?
- Can a crash between prepare, provider call, and settle leave an honest state?
- Are accepted, delivered, shed, unavailable, failed, and unknown distinct?
- Does every persisted cursor have one owner, advance only on input that could be read, and name a retention gap instead of jumping it?
- Does replay avoid duplicate provider calls and terminal-row regression?
- Does a Jev change remain advisory and evidence-bearing?
- Is output ordering stable?
- Are serialized readers backward-compatible without creating a second truth source?
- Is the smallest behavior witness present, including malformed, timeout, and unknown cases?

## Decomposition rule

Split a large module only at a responsibility seam:

- `cli_args`: pure argv normalization;
- `workspace_reporting`: read-only projections;
- `model` / `message` / `command`: domain contracts;
- `reducer`: pure transition logic;
- `dispatch` / `interpreter`: effects and adapters;
- `persistence`: serialization and recovery;
- `durable_resume`: pure recovery projection, with file/state loading kept in the interpreter;
- `durable_transition`: pure phase policy, with process dispatch kept in the interpreter.

Compatibility wrappers are temporary migration seams. Mark their owner and delete them when all callers migrate. Do not split by line count or move policy into a helper that still reads ambient state.

## Verification

Run the narrow witness first, then the repository gates:

```bash
mbx +nightly-aarch64-apple-darwin test kernel
mbx +nightly-aarch64-apple-darwin test workspace_reporting
mbx +nightly-aarch64-apple-darwin test cli_args
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
```

A green compile is not enough. A phase is complete only when its behavior witness, replay/parity evidence, and required gate are green.

## Current audit status

As of 2026-09-22, the reducer/Tick Law and serialized-policy boundaries pass fresh audit and the full workspace gates are green. The kernel remains **conditionally compliant** until legacy Jev authority helpers are fully threaded from the entrypoint and durable effect leaves receive injected wall/monotonic clocks instead of sampling them directly. Those reads are outside reducers and do not invalidate replay witnesses, but they are still migration work under the strict authority contract.

## Related sources

- [Effect kernel decision record](../decisions/0016-functional-effect-kernel-redesign.md)
- [Doorbell plane decision record](../decisions/0012-doorbell-plane-independent-runtime.md)
- [Doorbell guide](../guides/doorbell.mdx)
- [Session door](../guides/session-door.mdx)
- [Judgment packs](../guides/judgment-packs.mdx)
- [Delivery cursor persistence, bd-8nyh](../learnings/delivery-cursor-persist-bd-8nyh-2026-09-29.md)
