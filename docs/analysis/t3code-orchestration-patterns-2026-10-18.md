# t3code orchestration patterns: what to adopt, what we already beat

Research date: 2026-10-18.
Upstream: `github.com/pingdotgg/t3code` at `13aa882` (2026-10-17).
Local: flywheel `24f045e` plus sibling `../toron`.

## Bottom line

T3's advantage is not its feature list. It is three disciplines we only half-apply:
app-owned identity with provider refs kept as evidence, one serialized command
ingress with durable receipts and an effect outbox, and tests that replay real
provider frames through the real orchestrator. Our effect journal is already at
or ahead of theirs. Our unreliability lives one layer up: at the command doors
(spawn, send, steer, halt) where a timeout can be applied twice, and at the
identity layer where a name can route to a key nobody reads.

## What T3 actually built

One server per environment owns execution. Inside it:

```
command (CommandId)
  -> Orchestrator: per-thread lock, no I/O, decides events
  -> EventSink: one transaction = events + projections + receipt + effect outbox rows
  -> EffectWorker: claim (lease) -> provider call -> append result events
  -> Projection snapshot at sequence N, stream events > N
```

Durable entities are `AppThread -> Run -> ExecutionNode tree`, with
`ProviderThread` / `ProviderTurn` / `RuntimeRequest` as handles to provider-native
state and `ContextTransfer` / `ContextHandoff` for forks, provider switches,
merge-back, and subagents.

The ten stated invariants (`docs/orchestration-v2/README.md`):

1. App ids are primary. Provider ids are refs.
2. Provider events are never rewritten to look like another provider event.
3. Child execution completion never closes the parent run.
4. Checkpoints attach to checkpointable scopes; child checkpoints never advance
   the parent run count.
5. Rollback is expressed in app run count, reconciled with provider state.
6. Every command targets app ids; adapters translate at the edge.
7. Missing provider capability is explicit and handled by policy.
8. Provider switches create auditable context handoff artifacts.
9. Forks record app-level lineage first, resolve provider work lazily.
10. Tests prefer replay-backed integration over mocked units.

Note: the docs are target architecture. Implemented today: Orchestrator,
EventSink, CommandReceiptStore, EffectOutbox/EffectWorker, projections,
queue/steering, replay testkit, MCP orchestration tools. Design-only:
`IdentityBinding` correlation-strength table and parts of the capability policy.

## The seven patterns worth taking

### 1. Command receipts and request-key idempotency at every door

T3 gives every mutation a `CommandId` and stores a receipt in the same
transaction as its events. A replay returns the original `resultSequence` and
the stored events, never re-runs the side effect. Reusing a command id against
another thread is a typed conflict, not a silent success
(`Orchestrator.ts:252-267`, `CommandReceiptStore.ts`). MCP tools derive stable
ids from `clientRequestId`, so a retried tool call returns the same durable work.

Our gap: only `workflow compose --run` accepts `--idempotency-key`
(`src/cmd/workflow.rs:172-174`). Spawn has a name-based *best effort* guard with
a documented crash window between the guard and the spawn
(`src/main.rs:33214-33230`). Mail has corr-keyed dedupe on the toron side
(`crates/toron-core/src/mailbox/idempotent.rs`), but `flywheel send` does not
mint a stable corr per intent. A timeout plus retry can therefore double-spawn,
double-send, or double-steer.

Adopt: one `CommandId`/`client_request_id` on `spawn`, `send`, `steer`, `halt`,
`add`, `br` mutations, and `dispatch_job`. Persist `(command_id) -> (status,
result_sequence)` and replay it. Derive the spawned pane/agent identity from the
request key rather than from the pin name.

### 2. The outbox distinguishes process-bound from replay-safe effects

T3 splits effect types into two lists
(`EffectOutbox.ts:113-145`). On process loss, `reconcileProcessLoss` cancels
`provider-turn.start|interrupt|steer|restart` and
`runtime-request.respond` (cannot be safely repeated), and requeues
`checkpoint.capture`, `thread.rollback`, `cleanup`, `title.generate`,
`delegated-tasks.stop` (safe to repeat). The worker then claims with a lease,
retries with `min(30s, 100ms * 2^(attempt-1))` up to 5 attempts, and completion
is fenced by `status='running' AND lease_owner=worker`
(`EffectOutbox.ts:529-620`, `EffectWorker.ts:620-740`). Settled rows are kept 7
days for debugging.

Our state: `EffectState` already has `prepared/in_flight/accepted/settled/
failed/unknown/reconcile_pending/cancelled/parked` and lease expiry is honored
on claim (`crates/toron-core/src/index/workflow_effects.rs:1215-1280`). But
`plan_attempt` refuses blind retry for any action type without an idempotency
contract (`crates/toron-workflow/src/engine/reconcile.rs:101-122`), and
`capability_for_action_type` declares *nothing* idempotent except
`cleanup:release-reservations` (`:234-242`). Effect rows that crash mid-flight
therefore stay stuck and nobody sweeps them. There is no separate worker: the
journal dispatches inline and dies with the run.

Adopt: a `flywheel effect sweep` pass that lists `in_flight` rows with expired
leases, classifies them with an explicit per-action policy (`replay-safe`,
`process-bound`, `query-then-settle`), pages on `unknown`, and never retries a
blind spawn. Declare `send_message`/`send_dm` replay-safe on the in-process
plane, since toron's corr claim already makes the transport idempotent.

### 3. Identity is app-owned, correlation is scoped, and weak identity is typed

T3's rule is one line: provider ids are evidence, app ids are identity
(`docs/orchestration-v2/entity-ids-and-correlation.md`). Display names are never
keys. Native ids are matched only inside a scope
(`provider + providerSessionId + nativeThreadRef`), and the design records a
correlation strength: `native_exact | native_scoped | ordinal | fingerprint |
synthetic`. Pending requests after restart stay visible but carry
`not_resumable`, so the UI can say why a click will fail instead of pretending.

Our gap is the measured one already recorded in the global instructions: a pin
can be registered in two projects and resolve to two different npubs, and a send
to the wrong one reports `ok` while the message is invisible. That is exactly the
failure "names are identity" produces. `identity_divergent` exists as a warning,
not a refusal.

Adopt: a durable `(project, pin) -> npub` binding with a strength word, minted at
registration and never inferred. The send door refuses to route on a name whose
binding is missing, divergent, or stale unless an explicit npub is supplied.
Pane correlation should include the herdr workspace id, never the display title
alone.

### 4. A run attempt is a first-class row, so steering has somewhere to land

T3 separates the app `Run` from `RunAttempt` and `ProviderTurn`
(`core-graph-and-data-model.md`). Steering a provider that cannot steer becomes:
interrupt the active turn, mark the attempt interrupted or superseded, create a
replacement attempt under the same `RunId`. Only the final attempt completes the
run and captures the checkpoint. Earlier attempts stay in the graph for audit.
This is what stops "steer" from being either a silent no-op or an unrelated new
turn.

Our gap: a loop pass is the only attempt-like unit, and superseded work is not
recorded. The `resume-window` churn documented in the loop instructions is the
symptom: the same bead is re-resumed because there is no durable statement of
"attempt 2 replaced attempt 1 for this bead and pane". A steer that fails to
inject leaves no attempt row to reconcile against.

Adopt: a durable work attempt keyed by `(bead, pane, generation)` with
`queued|running|completed|interrupted|failed|superseded` and the injected
prompt's corr. Steering writes a new generation under the same bead intent.
`loop-mode` reads attempts, not just bead status, when deciding to resume.

### 5. Capability records with a declared degradation policy

T3 forbids provider-name conditionals in orchestration
(`provider-capability-system.md`). Every adapter emits capabilities
(`supportsActiveSteering`, `supportsInterrupt`, `terminalStatusQuality`,
`supportsMcpTools`, `exposesNativeThreadId`, ...), and each missing capability
names its fallback: steering degrades to interrupt-and-restart, fork degrades to
portable context, rollback degrades to checkpoint plus divergent provider state.
Optional adapter methods are never called without a capability check.

Our gap: `src/harness_caps.rs` models exactly one capability (`hook`) and only
for the supervisor-hook path. Pane injection, interrupt, and steer decisions are
made elsewhere and some harnesses simply cannot be woken by prompt injection
(`/health.injection`: `pins > injectable`). Today that shows up as a wake that
silently does nothing.

Adopt: one `HarnessCapabilities` record per harness covering inject, steer,
interrupt, resume, structured question, and MCP. Every wake/steer call site goes
through a policy wrapper that returns a typed `unsupported` naming the mail
fallback, instead of attempting an injection the harness cannot honor.

### 6. One retry owner, real jitter, and transport health separate from data freshness

T3's connection supervisor owns retry for the whole environment
(`packages/client-runtime/src/connection/supervisor.ts`). Numbers: base 1s,
ceiling `min(300s, 1000 * 2^(failures+1))`, delay in the upper half of the
ceiling (`ceiling/2 + ceiling/2 * random`), backoff reset only after 30s of a
stable connection, 15s establishment timeout, offline and auth states wait for a
wakeup instead of burning attempts. Reconnect does not replay mutations; retry
and idempotency belong to the operation. A socket opening is not proof the data
is fresh, and a failed subscription is not "reconnecting".

Our gap: `src/retry.rs:29-33` computes `capped + capped/10` with no random term,
so every caller sleeps the same amount and retries in lockstep after a restart.
Each channel also retries independently (pane bootstrap, dispatch, send), so one
outage creates several storms.

Adopt: make `delay_for` take a random draw and jitter across the upper half;
reset the failure count only after a stable success window; and give each channel
exactly one retry owner. Keep the emit/wait split: a wake emitted is not a wake
delivered.

### 7. Replay fixtures at the process boundary, not mocks inside it

T3 records raw provider frames with per-provider recorder scripts and replays
them through the real adapter, normalizer, event store, projections, and
checkpoint policy. Only the provider transport is replaced
(`docs/orchestration-v2/testing-strategy.md`). Allowed substitutes are the
transport, Effect `TestClock`, deterministic `Random`, temp FS, and temp DB.
Forbidden: mocked orchestrator, adapter, normalizer, event sink, projection, or
capability policy. The fourth level, command-to-projection integration, is named
as the only level that catches lifecycle mismatches.

Our gap: our tests are strong on pure reducers and on the journal, but the pane
and wake seams are exercised with fixtures we author, not frames a real harness
produced. That is why "the pane never woke" and "the wrong pane answered" are
hard to reproduce.

Adopt: record one real session per harness (agent output plus injection events)
as an ndjson transcript, and replay it through `doorbell-rt` / `Host` with only
the herdr transport replaced. Same for toron wire frames: record send, delivery,
read, and verdict, then replay through the real delivery FSM.

### 8. Context transfer is a durable artifact, not a prompt trick

T3's `ContextTransfer` plus `ContextHandoff` covers fork, provider handoff,
merge-back, and subagent results with one primitive: source point, base point,
provider-neutral relationship, lazy resolution, and a coverage range. The default
return strategy is `delta_since_target_last_seen`, not a full transcript. The
handoff is inspectable, regenerable, and auditable.

Our gap: respawn and context-exhaustion resumes build an ad-hoc brief. There is
no record of which runs the resumed pane already saw, so a resume can replay work
or skip context.

Adopt: a handoff row keyed by `(project, pin, bead)` with `from_run`/`to_run`
coverage, the strategy used, and the artifact path. Respawn reads it and
generates only the delta.

## Where we are already at or ahead

- Effect fencing and terminal states: our `unknown` / `reconcile_pending` /
  `parked` split is finer than T3's five states, and `parked` is a real
  interruption state rather than a reinterpretation of failure.
- Delivery semantics: toron's `Intent -> Unknown -> Sent -> Delivered ->
  Acknowledged -> Acted -> Proved` FSM is stronger than T3's notification
  mailbox. Do not replace it with a simpler model.
- Pure reducers: our Tick Law is stricter than T3's service-layer style, and it
  already forbids ambient clock, randomness, and I/O in the decision path.
- Beads as the work ledger: T3 has no equivalent of a fail-closed close
  ceremony. Keep that.
- Run ledger honesty: `unknown` never defaults to `done`. That matches T3's
  "acknowledgement is intent, not completion" rule.

## Proposed invariants for our stack

1. App ids are primary; pane titles, pins, and herdr names are refs.
2. Correlation is always scoped: workspace + project + pin, never a bare name.
3. A command acknowledgement means intent committed, not side effect finished.
4. Child completion never closes the parent lane.
5. Missing harness capability is explicit, typed, and routed to a fallback.
6. A lost process cannot replay a process-bound effect; recovery retires it.
7. Reconnect or retry never replays a mutation without a request key.
8. Every ordered surface has one backend-owned projection; clients do not merge.

## Ranked adoption plan

| # | Move | Effort | Failure class removed |
|---|------|--------|-----------------------|
| 1 | Request-keyed command receipts on spawn/send/steer/halt | M | double-spawn, duplicate sends, phantom halts |
| 2 | Jittered backoff + one retry owner per channel | S | retry storms, lockstep reconnects |
| 3 | Effect sweep: expired leases, per-action replay policy, page on unknown | M | workflows stuck after a crash mid-effect |
| 4 | Durable pin to npub binding with typed refusal at send | S-M | mail that reports ok and is never read |
| 5 | Work attempts with steering generations | M-L | resume churn, lost steers, unauditable retries |
| 6 | Harness capability record + degradation policy | M | silent no-op wakes on non-injectable panes |
| 7 | Replay transcripts for pane/wake and toron wire | M-L | unreproducible wake and routing bugs |
| 8 | Context handoff artifact with coverage range | M | duplicated or missing context on resume |
| 9 | Keyed writer or CAS for file-backed state | S | lost counters, clobbered watermarks |

Start with 1 and 2. They are small, they sit at the doors where the errors are
observed, and they make every later retry safe. Then 3 and 4, which remove the two
silent-loss classes. 5 through 8 are the structural work and should be filed as
separate beads with their own verify commands.

## What not to copy

- The per-thread in-memory lock. It is correct for one server process. We are
  multi-process by design (daemon, CLI, supervisor child passes, cron). We need
  a single writer per aggregate or a CAS on a version column, not a semaphore
  that only one process can see.
- The assumption that a provider adapter can always resume. We have harnesses
  that cannot be injected at all. The capability record has to be honest about
  that rather than degrading to a prompt nobody reads.
- The TypeScript/Effect stack. The patterns port; the runtime does not.
- The doc-only pieces: do not cite `IdentityBinding` as shipped upstream. It is
  a design sketch there too.

## Sources

- `docs/orchestration-v2/README.md`, `core-graph-and-data-model.md`,
  `entity-ids-and-correlation.md`, `feature-lifecycles.md`,
  `thread-lineage-and-context-transfer.md`, `provider-capability-system.md`,
  `orchestrator-mcp-server.md`, `testing-strategy.md`
- `docs/internals/overview.md`, `effect-services.md`, `connection-runtime.md`,
  `remote.md`, `glossary.md`
- `apps/server/src/orchestration-v2/Orchestrator.ts`, `EventSink.ts`,
  `CommandReceiptStore.ts`, `EffectOutbox.ts`, `EffectWorker.ts`,
  `NotificationMailbox.ts`, `DispatchModeLimit.ts`, `QueuedRunOrder.ts`,
  `ProviderEventIngestor.ts`
- `packages/client-runtime/src/connection/supervisor.ts`
- Local: `src/effect_journal.rs`, `src/retry.rs`, `src/harness_caps.rs`,
  `src/main.rs`, `src/state.rs`, `crates/toron-core/src/index/workflow_effects.rs`,
  `crates/toron-workflow/src/engine/reconcile.rs`,
  `crates/toron-mcp/src/workflow_host.rs`
