<!-- governed-by: ADR-0015, ADR-0016 -->
---
status: proposed
date: 2026-09-22
decision-makers: bangedorrunt
consulted: effect-oriented skill; flywheel ADR-0015 and ADR-0016; toron ADR-0034 and ADR-0035; chiebukuro ADR-0050
informed: flywheel, toron, beads, chiebukuro maintainers
---

# ADR-0017: A shared fold crate in toron

**Status:** Proposed. The crate is not created by this record. Nothing depends on it until the preconditions in §7 are fixed in place and this status is flipped to accepted.

**Decision in one sentence:** Do not build an Elm or Effect runtime. Add one zero-dependency Rust crate, `foldcore`, inside the toron repo.

## 1. Why this exists

Flywheel, toron, and chiebukuro already use the same shape: a pure function from `(model, message)` to `(model, commands)`, then an interpreter that runs the commands. Each repo wrote that shape by hand. The copies disagree, and the bugs are in the interpreters, not in the absence of a framework.

A framework that schedules fibers, injects layers, or replaces Tokio and Resonate would be a fourth runtime. Chiebukuro ADR-0050 already rejects that. Toron ADR-0035 says to copy a core, not import a kernel. This ADR keeps that refusal and extracts only the four values that have been reimplemented wrong.

## 2. Where the crate lives

The crate is created at `toron/crates/foldcore`, package name `foldcore`.

It does not live in flywheel. Flywheel depends on toron. Toron must not depend on flywheel. A crate inside flywheel cannot be used by `toron-core`.

It is not a module of `toron-core`. Chiebukuro and beads must be able to depend on the fold without linking the mailbox, Nostr, or SQLite. `toron-core` may depend on `foldcore`. The arrow stays one way.

It is not a fourth repository yet. A path dependency from flywheel, doorbell-core, chiebukuro, and beads to `../toron/crates/foldcore` matches the dependency flywheel already has on toron. Revisit a separate repository only if `foldcore` is published and a consumer must not track toron git history.

```text
foldcore          std only, no tokio, no IO
  ^
  |-- toron-core
  |-- flywheel
  |-- doorbell-core
  |-- chiebukuro
  \-- beads
```

## 3. What the crate is

Four pure types. The host still performs every effect.

### 3.1 Log fold

```rust
pub struct Step<Model, Cmd> {
    pub model: Model,
    pub commands: Vec<Cmd>,
}

pub fn fold_log<Model, Msg, Cmd>(
    init: Model,
    msgs: &[Msg],
    tick: impl Fn(&Model, &Msg) -> Step<Model, Cmd>,
) -> Step<Model, Cmd>
```

`tick` is supplied by the repo. `foldcore` does not know mail, panes, or QMD. A test asserts one message slice and gets one model and one command list.

A `Phase` enum is provided so a core can represent `Idle | Running | Terminal`. `foldcore` does not invent the phase for a domain. A helper `commands_if_running` returns an empty command list when the caller marks the step terminal. That is the whole "terminal emits nothing" check.

### 3.2 Journal gate

```rust
pub enum Gate<T> {
    Prepared,
    Claimed,
    Settled(T),
    Failed,
    Unknown,
    Interrupted,
}

pub enum GateOrder {
    Proceed,
    Refuse,
    Replay(Gate<T>), // already settled; do not call the provider
}
```

`prepare` is the only transition into `Claimed`. `order(before)` returns `Refuse` when there is no prepared row, `Replay` when the key is already `Settled`, and `Proceed` only from `Claimed`. The host inserts the row, then calls the provider. `foldcore` does not open a database. It only names the legal transitions so each repo stops inventing a fifth outcome or calling the provider first.

`Interrupted` is not `Failed` and not `Unknown`.

### 3.3 Breaker

```rust
pub enum Breaker<Id> {
    Closed { failures: u32 },
    Open { until: Id },
    HalfOpen { probe: bool },
}

pub enum BreakerDecision {
    Allow,
    Probe,
    Reject,
}
```

`Id` is the caller's clock unit (`u64` seconds, a monotonic tick). The function takes `now`. It does not read a clock.

`Probe` is returned for at most one call. A second call while `HalfOpen { probe: true }` is `Reject`. Failure on the probe returns to `Open`. Success returns to `Closed { failures: 0 }`. This replaces flywheel `ConsecutiveBreaker`, which clears the count and reports closed when the cooldown elapses, and toron `send_armor`, which flips to half-open and then admits every later caller.

Threshold and cooldown are fields. The crate does not pick production numbers.

### 3.4 Retry policy

```rust
pub struct RetryPolicy {
    pub cap: u32,
    pub base: Duration,
    pub ceiling: Duration,
    pub jitter: bool,
}

pub enum RetryChoice {
    Again { delay: Duration },
    Stop,
}
```

`decide(policy, attempt, retryable, rng_byte)` returns `Stop` for a non-retryable error, for attempt >= cap, and for cancellation. `rng_byte` is passed in. The function does not sleep and does not read the process RNG. Jitter is a pure function of that byte.

The host applies the choice. Order of wrappers is not encoded as a tower. The normative order remains flywheel ADR-0016 §5: fallback outside, cache inside fallback when a cache exists, budget timeout around the retry, breaker inside the retry, rate limit and bulkhead inside the breaker, attempt timeout on the call.

## 4. What the crate is not

- Not an async runtime, a fiber scheduler, or a dependency-injection layer.
- Not a replacement for Tokio, Resonate, SQLite, or the effect journal tables.
- Not a place for domain messages. `WorkspaceMsg`, `ReceiptsMsg`, and `AgentMsg` stay in their repos.
- Not a reason to rewrite working interpreters before §7 is done.
- Not a TypeScript or Zig port. Those stay in the effect-oriented skill as disciplines. This crate is Rust because these four repos are Rust.

## 5. Consumers and the order they migrate

After acceptance, and only after §7:

1. Add the crate and its unit tests. No production caller yet.
2. `doorbell-core` uses the breaker and the journal gate. It is already a pure crate.
3. `toron-core` `dispatch_core` uses the fold helper in tests. `Release` is fixed in toron before that move, so the crate does not inherit the bug.
4. Flywheel `ConsecutiveBreaker` becomes a thin wrapper or is deleted in favor of `foldcore::Breaker`. Call sites pass the injected clock.
5. Chiebukuro and beads take the path dependency when they next touch a reducer test. They do not import `toron-core`.

Each migration is a separate change. A repo that has not migrated keeps its current types.

## 6. Relationship to existing decisions

- ADR-0015 §2 item 4's wrap order is retired. ADR-0016 §5 is the stack. This ADR does not reopen P1–P7.
- ADR-0016 stays the flywheel reducer law. `foldcore` does not replace `workspace::tick`.
- Toron ADR-0035 stays the proposal for toron cores. This ADR does not accept 0035 and does not build the judgment plane.
- Toron ADR-0034 stays the list of toron effect gaps. `foldcore` is not a substitute for those fixes.
- Chiebukuro ADR-0050 stays the local reducer. The "no effects framework" alternative still holds. `foldcore` is a library of values, which 0050 already allows.

## 7. Preconditions, fixed in place before the crate exists

These are interpreter bugs. A new crate must not be the patch.

1. **Toron `dispatch_core::Release`.** Decrement `used` only when that assignee was in flight. A second release emits no freed slot.
2. **Toron receipts.** The prune tombstone is visible to the next write. The monotonic check and the upsert run in one transaction. A late owed-on-send does not recreate a pruned corr.
3. **Toron `ask_judgment`.** The attempt is journaled before the external call. A missing journal row refuses the call. The in-flight guard has to see a model that outlives one function call, or it is deleted.
4. **Toron `send_armor`.** While half-open, one probe is in flight. Every other caller is rejected until that probe settles.
5. **Chiebukuro `finish_queue`.** The update matches `event_id` and the current claim. A `done` row is not moved back to `pending`. An unknown investigation tool is stored as an error, not as `finish`.

Flywheel's cooldown breaker is not a precondition. It is migrated in §5 step 4, after the crate's tests exist. Until then, docs call it a cooldown reset.

## 8. Non-goals

- Implementing the crate in the same change as this record.
- Publishing to crates.io.
- Sharing the crate with TypeScript or Zig.
- Moving journal tables out of toron SQLite. The gate type is pure. The tables stay.

## 9. Verification, once accepted and implemented

- `foldcore` unit tests cover: log fold equality, terminal step with empty commands, journal refuse / replay / proceed, breaker single-probe, retry cap and non-retryable stop. No clock, no network, no temp directory.
- `mbx test -p foldcore` from the toron workspace is hermetic.
- A consumer migration test in doorbell-core or toron-core shows the same decisions as the old type on a fixed table of `(state, now, event)`.
- `cargo tree -p foldcore` shows no dependency except the standard library.
- `cargo tree -p toron-core -i foldcore` shows toron-core depending on foldcore, and no edge from toron-core back to flywheel.

## 10. Consequences

Easier: one breaker and one journal vocabulary. Tests in four repos call the same helpers.

Harder: a path dependency from chiebukuro and beads into the toron tree. A breaking change to `foldcore` is a four-repo change. That is why the API in §3 is closed and small.

Risk: extracting before §7 copies today's bugs into the shared type. The status stays proposed until those fixes land and the captain flips this file.
