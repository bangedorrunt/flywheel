---
name: effect-kernel
description: Enforce the flywheel effect-kernel laws while changing reducers, runtime effects, journaling, doorbell, durable loops, or Jev integration. Pure tick, journal before the provider, remote-command stack.
---

# Effect kernel

Use this skill for any change that crosses `workspace`, `durable`, `doorbell-core`, `doorbell-rt`, `effect_journal`, `authority`, `breaker`, or `judgment`.

## Required shape

```text
Msg -> pure tick(Model, Msg) -> Cmd -> interpreter -> result Msg
```

Reducers must not perform I/O, read time or environment, use randomness or locks, mutate external state, or silently drop a command. Time is an input message, and host wake policy is model data resolved before the live model is built and reused by replay folds. Runtime authority is injected into interpreters.

A command is an effect value. Interpret it with the effect-oriented stack: fallback outside, breaker inside the retry, budget timeout around the chain, attempt timeout on the call alone. Journal that command before the provider runs. Interruption is not success, not a typed provider failure, and not `Unknown`.

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

Read `docs/agent-instructions/effect-kernel.md` before editing. Rationale and phase status are in `docs/decisions/0016-functional-effect-kernel-redesign.md`.

The paradigm-level rules behind the laws (defer, four outcomes, policy on the value, the reducer fold, map-reduce fan-out, durable intent, substitution testing) live in the effect-oriented skill: `~/.agents/skills/effect-oriented/SKILL.md` and its `references/architecture.md`. This skill is their enforcement in this repo.

## Verification

```bash
mbx +nightly-aarch64-apple-darwin test <focused-filter>
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
```

A phase is complete when its behavior witness and parity evidence are recorded, not when the code compiles. Write that witness in `docs/decisions/0016-functional-effect-kernel-redesign.md`.

## Audit status (2026-09-22)

The reducer/Tick Law and serialized-policy boundaries are green under a fresh full workspace run. Treat the kernel as **conditionally compliant** until the remaining legacy Jev helpers stop resolving `RuntimeAuthority` internally and durable effect leaves receive injected wall/monotonic clocks rather than sampling them directly. Those are effect-edge migrations, not reducer violations, but they remain required for strict end-to-end authority injection.
