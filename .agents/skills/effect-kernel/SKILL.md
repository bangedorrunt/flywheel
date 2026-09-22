---
name: effect-kernel
description: Enforce flywheel ADR-0016 while changing reducers, runtime effects, journaling, doorbell, durable loops, or Jev integration.
---

# Effect kernel

Use this skill for any change that crosses `workspace`, `durable`, `doorbell-core`, `doorbell-rt`, `effect_journal`, `authority`, `breaker`, or `judgment`.

## Required shape

```text
Msg -> pure tick(Model, Msg) -> Cmd -> interpreter -> result Msg
```

Reducers must not perform I/O, read time or environment, use randomness or locks, mutate external state, or silently drop a command. Time is an input message, and host wake policy is model data resolved before the live model is built and reused by replay folds. Runtime authority is injected into interpreters.

A command is an effect value. Interpret it with the effect-oriented stack: fallback outside, budget timeout around the retry chain, breaker inside the retry, attempt timeout on the call alone. ADR-0016 §5 binds that stack to a journaled command. Interruption is not success, not a typed provider failure, and not `Unknown`.

## Required checks

1. Identify the single model that owns the fact.
2. Parse external strings into validated values at the boundary.
3. Add a command/result message instead of calling an effect from a reducer.
4. Give every real effect one journal identity and bounded retry/timeout behavior.
5. Preserve `Unknown`; never infer success from missing evidence.
6. Use typed kernel values actively: validate `Corr`, `AgentPin`, `WorkflowName`, and `ProjectPath` before mutation or process effects; use `ActionStage` at typed failure call sites.
7. Keep queue acceptance distinct from provider delivery.
8. Keep Jev advisory. It cannot choose topology, settle an effect, close a bead, or verify a run.
9. Add same-log/same-effects replay coverage and boundary tests for malformed, timeout, retry, and unknown results.

Read `docs/agent-instructions/effect-kernel.md` before editing and `docs/decisions/0016-functional-effect-kernel-redesign.md` for rationale and phase status.

## Verification

```bash
mbx +nightly-aarch64-apple-darwin test <focused-filter>
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
```

Do not mark a phase complete from compilation alone. Record the behavior witness and parity evidence in the ADR implementation record.

## Audit status (2026-09-22)

The reducer/Tick Law and serialized-policy boundaries are green under a fresh full workspace run. Treat ADR-0016 as **conditionally compliant** until the remaining legacy Jev helpers stop resolving `RuntimeAuthority` internally and durable effect leaves receive injected wall/monotonic clocks rather than sampling them directly. Those are effect-edge migrations, not reducer violations, but they remain required for strict end-to-end authority injection.
