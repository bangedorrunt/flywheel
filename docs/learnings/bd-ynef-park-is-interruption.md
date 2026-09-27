---
tier: perishable
created: 2026-09-27
expires: 2026-10-04
reinforced: 2026-09-27
provenance:
  bead: bd-ynef
  toron_commit: 278ef46507f4ade0fa2659c67149d2ded8a69c12
  corpus: "rg DispatchAcquire, Park in ../toron/crates/toron-workflow; EffectState in ../toron/crates/toron-core/src/index/workflow_effects.rs"
  wiki: none
  memory: none
status: inbox
---

# A park is interruption, and the SDK already had the vocabulary

## The gap

`DispatchAcquire::Park` (GS-062 C) can hand back "park until `T`". Nothing in
`toron-workflow` could act on it. A fence that hands back a park with no way to
resume it is not a durable fence, which is where flywheel's fence adoption
(bd-fen3) stopped.

## Three things that were already in the codebase

Worth knowing before designing a new step, because all three existed and I
initially reached past them:

1. **`ActionStage::Park`** and **`EffectKind::Parked { sink }`** were already
   defined in `action_contract.rs`. The park step's *vocabulary* was waiting for
   an action to hang it on. I first invented `EffectKind::JournaledControl`;
   the honest arm was the existing `Parked { sink: "park_until" }`.
2. **`EffectState` lives in `toron-core/src/index/workflow_effects.rs`**, not in
   `toron-workflow/src/engine/boundary.rs`. I added a `Parked` variant to the
   engine's *local* `EffectState` mirror first; the compiler caught it
   immediately, because `reconcile.rs` imports the real one. A second copy of a
   state enum that is not where it is persisted is a trap.
3. **`ctx.get_dependency::<T>()` panics when `T` is absent.** The existing
   convention is to inject the *option inside the Arc* —
   `get_dependency::<Option<EffectRunner>>()`. Probing with a hypothetical
   `try_get_dependency` does not exist in the 0.6 SDK.

## The one that mattered most: no wall clock

`resonate_sdk::context::Info` exposes `id`, `parent_id`, `timeout_at`, `func_name`,
`tags` — and no clock. That looked like an obstacle. It is the design working:
the wake time is data on the step, and "now" is data injected by the host as
`ParkClock { now_ms }`, exactly the way `LoopParams` policy arrives serialized.

The trap: defaulting a missing `now` to `0`. Every park would then look 56 years
overdue and resume instantly. A missing clock is a **loud error**, not a
default.

## Bounded means two different things

- A request longer than `PARK_MAX_WAIT_SECS` (24h) is **refused loudly**. A
  truncated park resumes early and reads as a successful step, which is worse
  than a refusal.
- Within the cap, one wait is clamped to `max_wait_secs` and the remainder
  resumes in another park, so the run stays interruptible.

`max_wait_secs` is a required schema field, not an option with a default. An
uncapped park is a hang with extra syntax.

## The re-drive key decides idempotency

`park:<run_id>:<node>:<until_ms>`, derived from the step's own data. A run
counter or a park timestamp would make every re-drive look new, so a crash
mid-park would park again forever. Deriving it from the step means parking the
same step to the same `T` twice is **one** resume.

## What to check when a dispatch plan gains an arm

`plan_attempt` returning a new variant broke `toron-mcp/src/workflow_host.rs`
at compile time, and the honest arm was not a refusal:

```rust
DispatchPlan::ResumeParked { attempt } => attempt,  // NOT an Err
```

A parked row is not a failure. Returning `Err` there would have reported a park
as a dispatch failure — the same category error as reporting a park as
`Settled`.

## The one that would have shipped green: a frozen clock

The first `ParkClock` was a struct with a fixed `now_ms: Option<u64>`. Every
unit test passed. In production it would have **frozen time**, so a park would
never notice its wake time arriving and a re-driven run would decide exactly as
it did the first time, forever.

The fix is that `ParkClock` is a **function**, and the engine is the host:

```rust
pub type ParkClock = Arc<dyn Fn() -> u64 + Send + Sync>;
// both constructors:
.with_dependency::<park::ParkClock>(Arc::new(park::system_clock_ms))
```

The leaf still never reads a clock. It calls the host's. That is the whole
design: the SDK gives a durable function no wall clock so a leaf cannot take
ambient time as authority, and the host is the one place allowed to look.

**The lesson generalizes past this bead: a pure function is testable, and that
is exactly why a pure function can hide an unwired dependency.** `plan_park`
took `now_ms` as a parameter, so no test could ever notice that no real caller
supplied a live one. Wire the dependency, then assert on the WIRING
(`park_live_host_clock_is_real_and_moves`) — a frozen stamp cannot satisfy
monotonicity. If the value is a time, test that it moves.

## Assert on the contract, not on a string marker

My first live assertions used `find_marker(&output, "resumed")`, which only
matches *string* values. The engine returned `{"after":null, ...}` because the
recording runner returns `Null`, so the assertion could never pass and I had to
tell "my test is wrong" from "the engine is wrong" by reading the output. The
fix was to assert the real contract: the step's key exists, and the park's value
carries `overdue_ms` — a field that is only non-zero if something compared a
real clock against T. That assertion is worth more than the string match: it
cannot pass without the wiring being real.

