<!-- governed-by: ADR-0010, ADR-0012, ADR-0013, ADR-0014, ADR-0015, toron ADR-0027/0032 -->
---
status: accepted
date: 2026-09-21
decision-makers: Captain (bangedorrunt) — accepted in-session after architecture review
consulted: effect-oriented skill; TypeSafe System One / Jev guidance; Elm/Redux architecture review; existing flywheel ADRs
informed: flywheel, doorbell-core, doorbell-rt, doorbell-cli, toron-workflow, beads maintainers
tracker: bd-6si7
---

# ADR-0016: Adopt one typed functional effect kernel across flywheel

## Status

**Accepted.** This ADR records the approved redesign and is the implementation contract for bead `bd-6si7`.

The redesign is evolutionary. It must preserve existing CLI behavior, Tick Law purity, sticky-open topology, effect-journal honesty, toron/flywheel dependency direction, beads close authority, and doorbell delivery semantics while removing duplicated authority and making those rules structural.

## Context

Flywheel is a Rust multi-agent orchestrator with three related runtime surfaces:

- the CLI and workflow engine;
- the durable bead-driven loop;
- the always-on doorbell runtime.

The project already uses the correct broad pattern:

```text
Msg -> pure reducer -> Cmd -> effect interpreter -> result Msg
```

The current implementation nevertheless has several overlapping state and effect authorities:

```text
WorkspaceModel
├── DoorbellModel
├── LoopModel
├── assignment and receipts
├── presence and roles
├── human decisions
└── wake, digest, and re-ring state

doorbell-rt::Brain
├── budgets, hear state, cursors, digest, sink routing
└── runtime journal sequencing

ActiveLoop / state files
└── persisted projections of loop state
```

The same concept can therefore be interpreted by several reducers, the runtime brain, JSONL journals, SQLite rows, and snapshots. This increases cross-file reasoning cost and makes future changes prone to semantic drift.

Effect discipline is also partly conventional rather than structural. Environment, clock, process, network, and filesystem effects remain scattered across configuration, CLI, doorbell, runtime, and judgment paths. Jev has a good host-side boundary, but its transport reads ambient environment, uses a local failure taxonomy, and is not yet part of the shared effect lifecycle.

The redesign is intended to improve:

1. correctness under crash, retry, replay, and partial delivery;
2. determinism and explainability of reducer decisions;
3. separation between observed facts, inferred judgments, and executable effects;
4. maintainability of the large `main.rs`, `workspace.rs`, and `durable.rs` modules;
5. safe, measurable use of Jev without turning it into an agent runtime;
6. testability through injected authority and deterministic fakes.

## Decision

Adopt a single typed functional effect kernel as the semantic authority for live orchestration.

The kernel follows this contract:

```rust
(Model, Msg) -> (Model, Vec<Cmd>)
```

The reducer is pure, total over validated messages, deterministic, and free of I/O, clocks, randomness, environment reads, process execution, locks, and mutable global state. Effects are values. Effect interpreters execute them and re-enter results as messages.

The target shape is:

```text
external wire / CLI / timer / process observation
             │
             ▼
      parse + validate at boundary
             │
             ▼
          AppMsg
             │
             ▼
      AppModel + AppReducer
             │
             ├── AppCmd::Persist
             ├── AppCmd::Deliver
             ├── AppCmd::RunWorkflow
             ├── AppCmd::RunProcess
             ├── AppCmd::Fetch
             ├── AppCmd::AskJudgment
             ├── AppCmd::Schedule
             └── AppCmd::Fault
             │
             ▼
       typed effect interpreters
             │
             ▼
          result AppMsg
```

### 1. One live model authority

Introduce a composition-level model named `AppModel` (or the nearest existing canonical equivalent) with typed submodels:

```rust
pub struct AppModel {
    pub session: SessionModel,
    pub assignments: AssignmentModel,
    pub delivery: DeliveryModel,
    pub execution: ExecutionModel,
    pub catalog: CatalogModel,
    pub judgments: JudgmentModel,
    pub faults: FaultModel,
}
```

Existing reducers may remain private slice reducers during migration, but only the composition reducer owns live state. `WorkspaceModel`, `LoopModel`, and the runtime brain must not independently decide the same transition. Durable snapshots and JSONL are persistence/projection mechanisms, not alternate authorities.

### 2. Typed messages and commands

Group messages and commands by domain while keeping one composition boundary. External strings and JSON values are parsed into validated internal values at the boundary.

Required domain types include validated wrappers or equivalent enums for:

```rust
Corr
AgentPin
WorkflowName
EffectKey
ProjectPath
Presence
Vehicle
ActionStage
RunStatus
DeliveryState
SinkResult
```

The implementation must not break serialized formats. New enum fields need explicit compatibility readers where old state exists. Unknown external values must produce a typed fault or `Unknown` state, never an invented success.

### 3. Injected runtime authority

Resolve process authority once at the entrypoint and inject it downward:

```rust
pub struct RuntimeAuthority {
    pub identity: IdentityAuthority,
    pub paths: PathAuthority,
    pub config: ResolvedConfig,
    pub clock: Clock,
    pub monotonic: MonotonicClock,
    pub process: ProcessAuthority,
    pub network: NetworkAuthority,
}
```

Reducers receive clock values through messages. Effect interpreters receive clocks, paths, environment/configuration, subprocess authority, and network clients explicitly. New business logic must not call `std::env`, `current_dir`, `current_exe`, `SystemTime::now`, or `Instant::now` directly.

### 4. One effect lifecycle

All real external effects use the existing host-owned journal contract and a common typed lifecycle:

```text
Prepared -> Claimed -> Running -> Delivered
                              ├── Failed
                              └── Unknown
```

Every effectful request carries:

```text
effect_key
attempt
provider
ActionStage
timeout
retry policy
failure classification
redacted evidence
```

Dry runs remain unjournaled when they perform no external effect. Uncovered effects remain loud and unsettled. A journal row must never claim work that no provider performed.

The existing `src/effect_journal.rs` and toron workflow journal remain the implementation foundation; this ADR unifies other effectful surfaces around them rather than creating a second journal.

### 5. Effect-oriented resilience

Resilience belongs on the command the interpreter runs. The reducer chooses the command. It does not sleep, retry, or read a clock.

Outermost to innermost for one remote command:

1. A skip filter, when the command should not run at all. A skip does not fall back.
2. Fallback. Another strategy, only after the protected command is exhausted.
3. Cache, only when a cache exists. It sits inside fallback and outside retry. It stores a primary success. It does not store a fallback answer as that success.
4. Budget timeout. Bounds the whole chain, retries included.
5. Retry. A bounded schedule, only for typed retryable failures, under the same effect key. The retried region is this command, not the reducer and not a wider workflow.
6. Circuit breaker, inside the retry, so an open breaker fails the attempt without calling through. Do not retry breaker-open, and do not count it as another trip. Size the threshold against the retry cap so one caller cannot open the circuit for the process by exhausting its own attempts.
7. Rate limit, when the provider contract is a count per interval. The wait is not an attempt failure and does not hold an in-flight slot.
8. Bulkhead, when the provider dies under concurrency. The queue is bounded. The slot covers the call, not the backoff.
9. Attempt timeout. Bounds and cancels that one call. It does not cover the rate-limit wait or the bulkhead queue.
10. The call.

Interruption is its own outcome. It is not `Failed`, not `Unknown`, and not a typed provider error. Cancelling a call still releases what the call acquired. A compensating action runs on failure or interruption, and it does not run after success.

`ConsecutiveBreaker` today is a cooldown reset: when the cooldown elapses it returns to `Closed` and clears the failure count before the next call is observed. That is not a half-open probe. The target shape is one probe that re-opens on failure. Until that lands, do not describe the current breaker as half-open.

Retry policies and clocks are injected. Retryability comes from the shared failure taxonomy. Every subprocess, HTTP request, sink, queue, and retry loop has an explicit bound.

### 6. Doorbell runtime as interpreter

`doorbell-rt::Brain` remains the single writer for runtime coordination, but its semantic responsibilities move into the kernel. The brain should only:

1. receive messages;
2. call the kernel reducer;
3. journal decided commands before delivery;
4. execute commands through runtime adapters;
5. send result messages back to the kernel.

The brain may own task supervision, bounded channels, sink workers, and runtime storage. It must not become a second policy engine for budgets, routing, presence, urgency, or delivery truth.

Sinks implement a common typed result:

```rust
enum SinkResult {
    Accepted { delivery_id: DeliveryId },
    Delivered { receipt: DeliveryReceipt },
    Shed { reason: ShedReason },
    Unavailable { retry_after: Option<u64> },
    Unknown { evidence: Evidence },
}
```

Queue acceptance is not provider delivery. Herdr, webhook, digest, and future sinks must preserve this distinction.

### 7. Jev as a typed judgment effect

Jev remains an optional host-side judgment adapter. It never enters a reducer, selects a topology, constructs a command, closes a bead, settles an effect, or becomes a second source of truth.

The permitted path is:

```text
MaterialContext
  -> deterministic table decision
  -> optional JudgmentRequest effect
  -> JudgmentResult message
  -> deterministic merge
  -> DecisionReady
```

A `JudgmentRequest` includes a versioned question pack, pinned model, bounded redacted state slice, parent effect key, and action class. The transport receives all authority explicitly. Its failures use the shared failure taxonomy and effect lifecycle.

Jev answers remain advisory. Code owns primitive, topology, thresholds, escalation, and commands. Choice/Score probability distributions and confidence are recorded as evidence. Noul probability is not treated as native confidence; any derived risk policy is explicit and independently tested.

Initial judgment surfaces:

- ambiguous material-context interpretation;
- urgency annotation;
- explicit-`none` advisory suggestions;
- inbound wake safety screening;
- workflow-family ranking after deterministic catalog filtering.

Jev is forbidden from:

- changing sticky-open topology;
- changing presence truth;
- deciding delivery settlement;
- deciding bead closure;
- creating a new agent loop;
- inventing green, settled, delivered, or verified outcomes.

### 8. Module boundaries

After the composition seam and typed contracts are stable, decompose large modules by responsibility:

```text
src/main.rs
├── cli.rs
├── dispatch.rs
├── robot.rs
└── commands/

src/workspace.rs
├── model.rs
├── message.rs
├── command.rs
├── reducer.rs
├── routing.rs
├── delivery.rs
└── reclassify.rs

src/durable.rs
├── model.rs
├── reducer.rs
├── lifecycle.rs
├── dispatch.rs
├── verification.rs
└── persistence.rs
```

This is a responsibility refactor, not a mechanical line-count split. Existing public CLI behavior and serialized state are compatibility contracts.

## Invariants

The following are implementation gates:

1. Reducers have no I/O, clock reads, randomness, environment reads, or mutable global state.
2. Same model plus same message produces byte-identical next model and commands.
3. All map iteration that affects output is ordered.
4. Every command has one named interpreter or is rejected loudly.
5. Every external effect has a bounded timeout and bounded retry behavior.
6. Every effectful real run uses enforced journaling; dry runs do not mint settled rows.
7. `Unknown` is persisted and visible; it is never silently converted to success.
8. Queue acceptance is never reported as provider delivery.
9. `Unknown` presence never prompts; unroutable wakes remain digest-visible.
10. Reclassify retries preserve goal, reason, source identity, and effect key.
11. Sticky-open topology changes only on explicit operator intent.
12. Jev cannot change primitive, topology, command, closure, settlement, or verification authority.
13. Beads remain the sole close authority.
14. Toron remains the mailbox and reservation authority; flywheel remains the orchestrator.
15. Existing CLI command names, machine-readable capabilities, and serialized state readers remain compatible.
16. Every new reducer path has same-log/same-effects coverage.
17. Every new external boundary has substitution tests for success, timeout, retry, malformed response, and unknown outcome.

## Non-goals

This ADR does not:

- replace Rust, Tokio, serde, toron, resonate, herdr, or the existing workflow engine;
- introduce an actor framework;
- introduce Redis, OTP, AgentServer, or a second event bus;
- replace the pure routing table with Jev;
- redesign the beads ledger or close ceremony;
- add new MCP tools;
- make remote mail journalable when it cannot carry the required correlation;
- promise exactly-once external delivery;
- change the default user-facing topology rules;
- perform a cosmetic split without a semantic seam;
- enable additional Jev surfaces before evidence and threshold validation exist.

## Alternatives considered

### Keep the current layered architecture and add conventions

Rejected. The project already has the conventions, but duplicated models and ambient effects allow drift. Comments cannot enforce single ownership.

### Rewrite the project around an actor framework

Rejected. It would add a second concurrency vocabulary and runtime dependency while the current single-writer brain and message reducers already provide the required semantics.

### Make Jev the orchestration loop

Rejected. It violates Tick Law, makes control flow nondeterministic, complicates effect honesty, and conflicts with TypeSafe’s own code-owned workflow model.

### Replace SQLite/JSONL journals with a new event platform

Rejected. The existing journal and toron store already provide the required durability and replay surfaces. The problem is fragmented ownership, not insufficient storage technology.

### Introduce one giant generic effect monad

Rejected. The project should use typed Rust commands and interpreters. A universal abstraction would obscure domain policy and increase reader load.

## Implementation plan

Implementation is sequential under `bd-6si7`. Each phase must leave the repository buildable and its verification gate green before the next phase begins.

### Phase 0: contract and baseline

Affected paths:

- `docs/decisions/0016-functional-effect-kernel-redesign.md`
- `src/workspace.rs`
- `src/durable.rs`
- `crates/doorbell-core/src/lib.rs`
- `crates/doorbell-rt/src/{brain,host,msg,sinks}.rs`
- `src/effect_journal.rs`
- `src/judgment.rs`
- `tests/`

Actions:

- preserve and extend reducer replay witnesses;
- add explicit architecture contracts for command ownership and delivery truth;
- identify serialization compatibility fixtures;
- use `mbx +nightly-aarch64-apple-darwin test` as the phase gate.

### Phase 1: typed kernel contracts

Actions:

- add the smallest internal validated domain types;
- introduce grouped application messages and commands without changing external CLI syntax;
- keep adapters from old `Msg`/`Cmd` forms so migration is incremental;
- add exhaustive command ownership tests.

Verification:

- compiler exhaustiveness;
- reducer determinism tests;
- old serialized state fixtures load unchanged;
- existing `tests/adr0027_slice1.rs` remains green.

### Phase 2: authority injection

Actions:

- extend `src/authority.rs` into the runtime authority seam;
- resolve environment, paths, identity, clocks, and process paths once at entrypoints;
- migrate touched call sites away from ambient reads;
- make breaker and timing logic clock-injected;
- make Jev transport authority-injected.

Verification:

- fake-clock tests;
- no new direct ambient authority in domain/reducer modules;
- deterministic breaker and timeout tests;
- environment-independent unit tests.

### Phase 3: single live state composition

Actions:

- introduce composition model/reducer around existing private slices;
- route live workspace, durable loop, and doorbell decisions through the composition boundary;
- retain persistence projections only for compatibility and recovery;
- remove duplicate live decisions before deleting transitional adapters.

Verification:

- replay same-log/same-effects tests across workspace, loop, and doorbell;
- crash-resume fixture tests;
- no dual live model construction in runtime paths;
- `doorbell why` remains mismatch-free.

### Phase 4: unified effect lifecycle

Actions:

- define typed effect request/result lifecycle;
- route process, sink, mailbox, verify, fetch, spawn, and Jev effects through shared policies;
- preserve `Enforced` versus `Recording` journal modes;
- add typed unknown/reconciliation paths;
- replace local retry/breaker behavior where semantics overlap.

Verification:

- prepared-before-provider tests;
- replay does not duplicate provider calls when settled;
- crash at each lifecycle boundary records honest state;
- no dry run creates a settled effect row;
- timeout, 429, provider failure, and malformed result tests.

### Phase 5: doorbell interpreter consolidation

Actions:

- keep `doorbell-core` pure;
- move policy decisions from `Brain` into the composition reducer;
- keep runtime supervision, bounded channels, persistence, and sink workers in `doorbell-rt`;
- standardize sink results and route existence;
- preserve digest, budget, cursor, wake, and re-ring behavior.

Verification:

- embedded/daemon parity fixtures;
- sink acceptance versus delivery tests;
- bounded queue and shed-to-digest tests;
- restart and undelivered-intent tests;
- live daemon degradation remains visible and non-fabricated.

### Phase 6: Jev integration hardening

Actions:

- make transport dependency-injected and shared-effect-aware;
- map Jev failures into the common failure taxonomy;
- persist judgment effect evidence under the parent reclassify key;
- distinguish Choice/Score confidence from Noul probability;
- keep table primitive/topology authoritative;
- add deterministic offline judgment fixtures and outcome measurement fields.

Verification:

- missing key, explicit off, 429, timeout, malformed body, low confidence, and topology-drift tests;
- no Jev call from reducers;
- no Jev result can settle, close, or flip topology;
- same parent effect key on retry;
- redaction and byte-cap tests.

### Phase 7: responsibility decomposition

Actions:

- extract stable semantic seams from `main.rs`, `workspace.rs`, `snapshot.rs`, and `durable.rs`; a line-count split without a distinct authority is forbidden;
- preserve command enum and robot capability output;
- add `ADR-0016` references at new composition and interpreter entrypoints;
- remove obsolete policy duplication while retaining only named compatibility wrappers at public compatibility boundaries.

The phase is complete when the remaining large files contain only responsibilities that still share a semantic authority. A cosmetic `main.rs`/`durable.rs` split is explicitly not required: it would move code without reducing authority or reader load.

Verification:

- CLI help and robot-capability golden tests;
- focused deterministic witnesses for every extracted seam;
- full workspace test and clippy gates;
- no public API expansion without concrete need;
- no behavior-only diff in parity fixtures.

## Verification commands

The required repository gates are:

```bash
mbx +nightly-aarch64-apple-darwin test
bash scripts/verify-clippy.sh
```

Relevant focused gates include:

```bash
mbx +nightly-aarch64-apple-darwin test adr0027_slice1
mbx +nightly-aarch64-apple-darwin test effect_journal
mbx +nightly-aarch64-apple-darwin test judgment
mbx +nightly-aarch64-apple-darwin test --package doorbell-core
mbx +nightly-aarch64-apple-darwin test --package doorbell-rt
```

A phase is not complete because it compiles. It is complete only when its behavior witness, replay/parity evidence, and required test gate pass.

## Consequences

Positive consequences:

- one place to understand orchestration decisions;
- stronger compiler enforcement of lifecycle and delivery invariants;
- deterministic replay extends beyond individual reducers;
- shared resilience behavior and failure vocabulary;
- Jev remains useful without becoming a hidden controller;
- doorbell and durable loop become easier to test and operate.

Costs:

- temporary adapters and duplicate types during migration;
- more explicit dependency wiring;
- persisted schema compatibility work;
- larger test fixtures and replay logs;
- eventual deletion work after parity is proven.

Risks:

- migrating too much at once could break subtle delivery semantics;
- collapsing models prematurely could conflate persistence projections with live authority;
- typed wrappers can increase ceremony if introduced without stable seams;
- Jev thresholds require domain measurement, not assumption;
- a broad module split before seam stabilization would merely relocate complexity.

Mitigations are the phase gates, vertical slices, characterization tests, explicit compatibility readers, and fail-closed journal behavior defined above.

## Revisit conditions

Revisit this ADR if:

- replay requires a second semantic reducer;
- a new external provider cannot fit the effect lifecycle;
- a persisted state format cannot be migrated without user-visible loss;
- Jev evidence shows the selected judgment surface is less reliable than the deterministic baseline;
- the single-writer model becomes a measured throughput bottleneck rather than a hypothetical concern.

## Implementation record

- Accepted: 2026-09-21 after architecture review.
- Tracking bead: `bd-6si7`.
- Phase 0 baseline and Phase 1 typed contracts are landed in `src/kernel.rs` with lifecycle, presence, identifier, and determinism witnesses.
- Phase 2 authority work is landed in `src/authority.rs`, including one-time host resolution, injected wall and monotonic clocks, test clocks, and explicit Jev transport construction.
- Phase 3 composition is represented by the existing single live `WorkspaceModel` seam and its replay/ownership tripwires; `LoopModel` and persisted state remain projections/adapters rather than a second live workspace owner.
- Phase 4 journal coverage remains host-owned in `src/effect_journal.rs`; Phase 5 keeps `doorbell-core` pure, preserves brain single-writer semantics, and makes sink traversal ordered for deterministic runtime behavior.
- Phase 6 Jev is host-only, redacted, bounded, table-authoritative, parent-keyed, and now mapped into the shared failure taxonomy.
- Verification on 2026-09-21: `bash scripts/verify-clippy.sh` passed; the focused authority, kernel, breaker, judgment, doorbell, and CLI-golden suites passed; the full workspace suite passed its main packages and exposed one parallel environment-mutating judgment test race that passes in isolation. The existing live-toron ignored tests remain intentionally ignored.
- Phase 7 is complete under its semantic-seam criterion: pure divergence projections in `src/workspace_reporting.rs`, pure legacy argv normalization in `src/cli_args.rs`, pure snapshot fleet/reporting predicates in `src/snapshot_reporting.rs`, pure durable recovery projection in `src/durable_resume.rs`, and pure durable phase transition policy in `src/durable_transition.rs`. All retain compatibility callers and have focused deterministic witnesses. No remaining split is justified by a distinct authority; a cosmetic line-count split is intentionally rejected.
- Documentation synchronization landed with `docs/agent-instructions/effect-kernel.md`, `docs/guides/effect-kernel.mdx`, `.agents/skills/effect-kernel/SKILL.md`, and cross-links from the session, orchestration, doorbell, judgment, and guide-index pages. `AGENTS.md` now treats the ADR-0016 laws as repository invariants.
- Verification on 2026-09-21 after the latest seams: formatting passed; focused `cli_args`, `kernel`, `workspace_reporting`, `snapshot_reporting`, `durable_resume`, and `durable_transition` tests passed; `bash scripts/verify-clippy.sh` passed; the full workspace suite passed with **1,902 passed, 0 failed, 4 intentionally ignored**. The ADR implementation is complete under the semantic-seam criterion above.
- Follow-up hardening wired `kernel::ActionStage` into failure classification at pane call sites, adapted the receipt/contact edges to the current toron typed APIs, and reverified the corr-preserving degraded/duplicate path. Current verification: `bash scripts/verify-clippy.sh`, focused `adr0010_d8`, and the full workspace suite with **1,904 passed, 0 failed, 4 intentionally ignored**.
- The next authority slice adds `src/workspace_policy.rs`: environment configuration is resolved once at live-host construction and carried as `WorkspaceModel.policy`; digest and re-ring reducers now consume policy data, while their compatibility helpers are deterministic defaults. Live and replay model construction use the same policy boundary. The toron `MessageId` migration is also handled at the backend edge with fail-closed parsing instead of invalid empty defaults. Focused policy/backend tests and clippy pass; the flywheel binary suite passes with **1,906 passed, 0 failed, 4 intentionally ignored**.
- The durable-loop slice moves reclaim and pane-stall thresholds fully behind loop params: `LoopParams.reclaim` (existing `ReclaimPolicy`, already serialized for replay) is joined by `LoopParams.pane_stall_secs`, both resolved exactly once at the CLI construction site in `main.rs`. The tick reads `params.pane_stall_secs` instead of calling the env-reading helper mid-iteration, the env-mutating reclaim test was replaced by a policy-substitution witness, and `stale_reclaim_due`/`plan_reclaim` compatibility wrappers are deterministic default-threshold adapters marked test-only where they are no longer called in production. Pre-field saved params load at the crate default via `#[serde(default)]`, proven by a serde witness.
- The final policy-as-data slice removes the last mid-tick env reads from the durable loop: `FLYWHEEL_VERIFY_STRICT` folds into `LoopParams.strict` at loop start (leaves read `args.strict` only), the dispatch-guard freshness TTL becomes serialized `LoopParams.dispatch_guard_ttl_us` (the guard leaf receives it on `DispatchGuardArgs`), the mail-health escalation threshold becomes `LoopParams.mail_health_escalate_at`, and the declared-pause resurface window plus the write-liveness walk budget become `LoopParams.pause_resurface_secs` / `walk_timeout_secs` — the drain path no longer re-resolves `WalkOpts::from_env()` per pane. All fields carry `#[serde(default)]` pre-field compatibility witnesses. Fresh verification on 2026-09-22: `bash scripts/verify-clippy.sh` passed; the workspace passed **1,908 flywheel tests, 23 doorbell-cli tests, 24 doorbell-core tests, and 105 doorbell-rt tests, with 0 failures and 4 intentionally ignored**; deterministic workspace, doorbell, kernel, policy, judgment, and replay witnesses passed.

## Final compliance audit: 2026-09-22

**Verdict: conditional compliance, not unconditional completion.** The ADR-0016 reducer contract, Tick Law, serialized durable/workspace policy boundary, typed lifecycle checks, Jev authority constraints, journal honesty, replay witnesses, and extracted semantic seams are verified in the current tree. No new reducer or tick ambient-authority gap was found, and the required gates are green.

Two residual gaps remain and are explicitly tracked rather than hidden by the green suite:

1. **Authority injection is incomplete at legacy effect edges.** `src/workspace.rs` still constructs `RuntimeAuthority::resolve()` inside the reclassify expansion path, and `src/judgment.rs` retains compatibility helpers (`UreqTransport::from_env`, `prepare_post`) that resolve authority internally. These are outside pure reducers and are fail-closed, but they do not yet satisfy the strictest ADR wording that the entrypoint resolves one authority and threads it through every interpreter.
2. **Some durable effect leaves sample time directly.** `read_output`, `dispatch_guard_scan`, `now_us_micros`, and the subprocess watchdog use `SystemTime`/`Instant` at effect boundaries. They are not reducer reads and their results are checkpointed, but the strict authority contract calls for injected wall and monotonic clocks. This is a migration gap, not evidence of nondeterministic reducer behavior.

Until those two edges are migrated and substitution witnesses cover injected clocks and authority threading, reviewers must describe ADR-0016 as **core-kernel compliant with residual authority-edge work**, not “fully complete.”

### Effect-oriented review: 2026-09-22

Section 5 now states the effect-oriented stack. The previous one-line order (`operation timeout -> bounded retry -> breaker/fallback`) mixed the budget timeout with the attempt timeout and put fallback on the inside. Interpreters follow the numbered list in §5.

Checked against the tree on this date:

- The fold is real. `workspace::tick`, `durable::tick`, and `doorbell-core::tick` are `(model, message) -> (model, commands)`. `doorbell-rt` host `tick` takes `&mut self` because it is the interpreter edge, not a second policy reducer.
- The two gaps in the 2026-09-22 audit are still present. `expand_reclassify_cmds` in `src/workspace.rs` calls `RuntimeAuthority::resolve()` on the host path. `UreqTransport::from_env` still resolves authority for leftover callers. `read_output` and `now_us_micros` in `src/durable.rs` still sample `SystemTime` inside checkpointed leaves. `ConsecutiveBreaker::allows` and `record_failure` still sample `SystemClock` for callers that have not moved to `*_at`. `src/pane.rs` still calls `record_failure()`. That dispatch path also retries with `std::thread::sleep` up to a fixed cap. That is a handwritten retry, not the §5 stack, and the sleep blocks the caller.
- The breaker is not half-open. After cooldown, `allows_at` clears `opened_at` and the failure count, and `state()` is `Closed` before the next call returns. The unit test that claimed a half-open probe was renamed to describe that reset. A failing first call after cooldown must reach the threshold again before the breaker re-opens.
- A journal insert failure in `expand_reclassify_cmds` logs and then continues with an in-memory intent, including the Jev post. That is louder than a silent drop, and it is not a refusal. Enforced mode is supposed to refuse an effect that has no journal row.
- Cancellation is not yet a first-class interpreter outcome next to `Failed` and `Unknown`. Timeouts today fold into the failure taxonomy. A dropped attempt must still release, and it must not be reported as provider success.
