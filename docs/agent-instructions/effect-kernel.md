# Functional effect-kernel discipline

This guide is the operational contract for ADR-0016. It is intentionally shorter than the ADR: use the ADR for rationale and migration history, and this page while editing code.

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

**3. Effects are values first.** A reducer emits a command; it does not perform the command. Each command has exactly one named interpreter. Unknown commands fail loudly rather than being ignored.

**4. Facts precede judgments.** Presence, delivery, run status, and provider results are observations. Jev may annotate or rank an already-filtered question, but cannot create a command, settle an effect, close a bead, or change sticky topology.

**5. Unknown stays unknown.** Timeout, crash, malformed response, missing route, and unavailable provider are typed outcomes. Never map them to `done`, `delivered`, `verified`, or `succeeded` for convenience.

**6. Queueing is not delivery.** A sink accepting work proves queue acceptance only. Provider delivery requires its own receipt. Digest, shed, unavailable, and unknown remain visible.

**7. Journal before provider.** A real effect is prepared and claimed before its provider is invoked, then settled once. Dry arms do not mint settled effect rows. An uncovered effect refuses in enforced mode.

**8. Retry preserves identity.** A retry preserves the source message identity, correlation, effect key, goal, reason, and redacted input. A retry is another attempt, not a new logical action.

**9. Determinism is observable.** Same model plus same message produces the same next model and command sequence. Output-affecting maps are ordered. New reducer paths require same-log/same-effects coverage.

**10. Bound all work.** Network, subprocess, retry, queue, digest, and recursion paths have explicit limits. A fallback is a typed transition, not an unbounded loop.

**11. Parse at the edge.** External strings and JSON become validated kernel values at the boundary. Internal code should not repeatedly re-interpret raw identifiers or invent defaults for malformed values.

**12. Close authority is singular.** Beads close beads. Toron owns mailbox and reservation truth. The host owns run/effect journaling. A worker, Jev result, or report cannot self-admit completion.

## Review checklist

Before merging a change touching a reducer or interpreter, check:

- Does the diff add an ambient authority read to domain code?
- Is the new transition represented by a `Msg` and a `Cmd`, with the result re-entering as a `Msg`?
- Is there one interpreter and one journal identity for the command?
- Can a crash between prepare, provider call, and settle leave an honest state?
- Are accepted, delivered, shed, unavailable, failed, and unknown distinct?
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

A green compile is not enough. The ADR phase is complete only when its behavior witness, replay/parity evidence, and required gate are green.

## Related sources

- [ADR-0016](../decisions/0016-functional-effect-kernel-redesign.md)
- [Doorbell guide](../guides/doorbell.mdx)
- [Session door](../guides/session-door.mdx)
- [Judgment packs](../guides/judgment-packs.mdx)
