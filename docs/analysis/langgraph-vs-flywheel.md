# LangGraph vs flywheel x toron x beads

**Bead:** bd-ysct · **Pin:** brash-otter · **Date:** 2026-09-26

## Verdict up front

**Reject LangGraph as an engine. Borrow four ideas, and one of them is urgent.**

LangGraph is a single-process state-machine library. Our stack's named
problem (ADR-0016) is *duplicated authority across processes*. Those are
different diseases. Importing LangGraph would add a second orchestration
authority to a stack whose top-priority bead is removing the first one.

The one genuinely urgent borrow: **we have no run-state checkpoint, so we
cannot answer "what was the state of run X at step 3" or replay an
alternative trajectory.** Our Tick Law already makes that nearly free. See
[B2](#b2--checkpoint-per-tick--pending-writes).

---

## Part 1 — What LangGraph actually guarantees

Sourced from `docs.langchain.com/oss/python/langgraph/{graph-api,
persistence,checkpointers,fault-tolerance}` and the repo README. Not from
marketing copy.

| # | Guarantee | Mechanism |
|---|---|---|
| L1 | **One state schema, one authority** | `StateGraph(State)`. Every key has a reducer: `new = reducer(left=current, right=update)`. Default reducer replaces; `Annotated[list, add]` appends. Nodes emit partials; reducers are the only writer. |
| L2 | **Pregel BSP supersteps** | Nodes go `inactive → active` on an incoming channel message. All nodes in one superstep run in parallel. At the end of a superstep, nodes with no input mark themselves `inactive` (halt). Graph ends when all inactive and no messages in transit. |
| L3 | **Checkpoint per superstep + per-task writes** | `put` writes one checkpoint row per superstep. `put_writes` writes one row per node output `(task_id, channel, value)`. |
| L4 | **Pending writes = partial-superstep recovery** | If node B fails mid-superstep, node A's writes in that same superstep are already durable. Resume does **not** re-run A. |
| L5 | **Three durability modes** | `exit` (persist only on exit), `async` (persist while next step runs), `sync` (persist before next step starts). |
| L6 | **interrupt() / Command(resume=)** | `interrupt("approve?")` raises `GraphBubbleUp`, parks mid-node, survives crash. `Command(resume=v)` makes `interrupt()` return `v`. Resumes at the node, from the node's start. |
| L7 | **Time travel** | `get_state`, `get_state_history`, `update_state`, `update_state(as_node=...)`, and `invoke` with a prior `checkpoint_id` to replay. Fork a thread to explore a different trajectory. |
| L8 | **Per-node retry / timeout / error handler** | `RetryPolicy(max_attempts, backoff_factor, jitter, retry_on)`. `TimeoutPolicy(run_timeout, idle_timeout, refresh_on)` — idle resets on heartbeat, stream output, child-task scheduling, or any LangChain callback. `error_handler(state, error: NodeError) -> Command` for Saga compensation. Composes in fixed order: exception → retry → exhausted → handler. |
| L9 | **Resume-safe failure provenance** | Failure context is checkpointed. A handler interrupted before completing sees the same `NodeError` on resume. |
| L10 | **Graph migrations survive resume** | Add/remove nodes and edges freely. Renaming a state key loses that key's saved state. |
| L11 | **Idempotency is the caller's job** | "Design node logic so re-execution does not corrupt state. If a node inserts a database row, running it twice should not create duplicate rows." Side effects before `interrupt()` must be idempotent. |
| L12 | **Checkpointer vs Store** | Checkpointer = thread-scoped short-term graph state. Store = cross-thread long-term app data. Two separate systems, deliberately. |
| L13 | **DeltaChannel** | Stores a sentinel instead of the full value; state rebuilt by replaying ancestor writes. O(1) checkpoint size for append-heavy channels. Beta, `langgraph>=1.2`. |
| L14 | **Studio + LangSmith deployment** | Visual graph, trace, time-travel UI, managed hosting. |

### The two things the docs are careful to warn about

These are load-bearing for our judgment, because they are the exact points
where LangGraph is *weaker* than our stack.

1. **Re-execution is the default, not the exception.** "the affected node
   runs again from the start of its function. Code and side effects before
   the pause run again." LangGraph's answer is the `task` decorator, which
   checkpoints sub-steps inside a node. Without `@task`, a resumed node
   re-runs everything.
2. **Correctness under retry is delegated to the author.** No framework-level
   guarantee that an external effect happened at most once. The docs say so
   plainly, in a Warning, in three separate places.

---

## Part 2 — Where we already match, and where we beat it

Measured against the source, not the README.

### We already have the LangGraph execution model

`src/workspace.rs:1561`

```rust
pub fn tick(model: &WorkspaceModel, msg: &Msg) -> (WorkspaceModel, Vec<Cmd>)
```

This is the LangGraph node signature with a different name. `Msg` is our
`channel message`, `Cmd` is our write-and-goto bundle, the returned model is
the post-reducer state. `src/durable.rs:568` has the same shape for the
durable loop: `tick(&LoopModel, &LoopMsg) -> (LoopModel, Vec<LoopAction>)`.

Two pure reducers, 22 `Msg` variants, no I/O in the tick. That is L1 + L2's
*shape*, already Elm-strict and already documented as the Tick Law.

### We have durable execution, and it is not optional

`toron-workflow` embeds **Resonate** (`resonate_server_url()`,
`engine/mod.rs:64`; ADR-0035). A run that hits `request_approval` suspends,
**survives a toron process crash**, and resumes when the approval event
arrives. That is L6 plus more — LangGraph needs a `thread_id` and a
checkpointer; we need a live Resonate promise.

### Our human-in-the-loop is cryptographic and remote-capable

The approval path, from `toron-workflow/src/engine/mod.rs:7-21`:

1. run hits `request_approval` → creates a Resonate promise
2. publishes kind-46011 `approval-pending` carrying `promise_id`
3. operator signs kind-46011 approval/rejection
4. the parked durable function unblocks → continues → kind-46012

Backed by `ParkKind::{Approval, Spawn}`, `PendingRegistry`, `PendingEntry`
(identity `(run_id, promise_id, generation)`), `attribute_parks`,
`validate_resolution`, `terminal_digest`.

An operator can approve a run from another machine, and the approval is a
signed Nostr event. LangGraph's interrupt is an in-process value you pass
back in. **We are strictly better here and should not regress.**

### Our effect journal beats LangGraph's idempotency story

This is the sharpest difference and it runs in our favour.

| | LangGraph | flywheel |
|---|---|---|
| Granularity | per superstep, per node | **per effect** |
| Identity | none | `derive_effect_key` |
| Lifecycle | checkpoint → resume | `Prepared → InFlight → Settled` (or failed / unknown) |
| Missing journal | n/a — you just re-run | **`EffectJournalMode::Enforced` REFUSES the effect** |
| Dry arms | n/a | `Recording` mode, visibly bypassed, never a silent success |
| Who guarantees at-most-once | the author, per node | the framework, per effect |

From `toron-workflow/src/engine/mod.rs:329-345`: *"`Enforced`: fail closed.
An effectful leaf with no runner is refused... There is no mode where an
unjournaled effect reads as success."*

LangGraph's L11 pushes idempotency onto the node author. We enforce it in
the host and we do it at finer granularity. **Do not give this up.**

### Our run ledger is more honest than LangGraph's

`src/main.rs:23718`

```rust
fn history_row_status(data: &Value) -> String {
    data.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string()
}
```

A row that recorded no status reads `unknown`, never a default `done`. One
reader rule backs `workflow history`, `runs list`, `--robot-snapshot`, and
`cron history`. Annotation rows declare themselves via `annotation_kind` so
they never count as run outcomes.

LangGraph has `get_state_history` but no equivalent rule that a
missing-status row is a *hole* rather than a *success*. Our honesty
convention is better and should be preserved under any redesign.

### Also already present

- `Graph` / `GraphNode` schema (`toron-workflow/src/schema.rs:165,172`) — a
  real graph type, not just YAML steps.
- `Action` with 16 variants including `request_approval`, `parallel`,
  `foreach`, `branch`, `reduce`, `call_workflow`, `verify` — L2's fan-out and
  map-reduce are here as `parallel` / `foreach` / `Send`-equivalent.
- `Phase` — a **closed** 6-value vocabulary (resolve, plan, implement, review,
  verify, compound) with a custom deserializer that rejects unknown variants
  at parse time. LangGraph has no closed phase vocabulary.
- `ActionStage` (`normalize | input | execute | output | effect | park`),
  `ProviderErrorClass`, `EvidenceRef` — structured failure typing, finer than
  LangGraph's exception-type retry matching.
- `Lint` (1378 lines) + `playbook_lint` (1143 lines) — workflows are
  validated before they run. LangGraph's `compile()` only checks for orphaned
  nodes.
- Catalog tiering builtin < user < project; 40 builtin workflows across 9
  families; 27 swarm / 8 none / 3 open / 2 loop topologies.
- Closed `ReduceOp` (`All | Any | Count | Sum | Join`) — L1's idea, but
  restricted to one `reduce` action rather than per-state-key.

---

## Part 3 — Where we are genuinely behind

### G1 — No run-state checkpoint. No time travel. (the real gap)

Grepped both repos for `time_travel`, `fork`, `rewind`, `replay_from`,
`resume_from_checkpoint`, `get_state_history`: **zero hits** in any `.rs`
file. Grepped for `pending_write`, `superstep`, `pregel`: **zero hits**.

So: no L3, no L4, no L7.

Concretely, today you cannot:
- ask what state run X was in after step 3
- fork a run and explore what would have happened on the other branch
- replay a failed run with one branch changed
- recover only the *successful* nodes of a partially-failed superstep

Debugging a failed run is log archaeology across JSONL, SQLite, and panes.

**And our `Checkpoint` is a naming trap.** `src/state.rs:1556`:

```rust
pub struct Checkpoint {
    pub ts: String, pub project: String, pub message: String,
    pub workspace_id: Option<String>,
    pub agents: Vec<CheckpointAgent>,   // { name, kind, status, pane_id }
    pub git_head: Option<String>,
}
```

That is a **swarm roster snapshot**, not a graph-state checkpoint. We use the
word "checkpoint" for something LangGraph means something else by. Anyone
arriving from LangGraph will misread this.

Our own reducer state is recovered by **projection seeding**
(`durable_resume.rs: seed_swarm_resume`) — reading persisted counters and
reseeding the model. That is not replay, and it is not a state log.

### G2 — Reducers are not per-state-key

L1's real content is that *every* state key has a reducer and the reducer is
the only writer. We have `ReduceOp` as a five-value enum reachable only
through a `reduce` action. Our state authority is spread across
`WorkspaceModel`, `DoorbellModel`, `LoopModel`, `doorbell-rt::Brain`,
`ActiveLoop` projections, JSONL journals, SQLite rows, and snapshots.

This is verbatim ADR-0016's Context section. `bd-6si7` (P1, in progress).

### G3 — Conditional status

`AGENTS.md` audit status, 2026-09-22: verdict is *conditional*. Legacy Jev
helpers still self-resolve `RuntimeAuthority`, and several durable effect
leaves sample wall/monotonic time directly — both outside reducers but
blocking a strict end-to-end authority claim.

### G4 — No live graph inspector

No Studio equivalent. `artifacts stitch <run_id>` renders evaluation
evidence, and herdr panes are the live UI, but there is no "show me this run's
graph and where it is right now" surface. L14 is a real UX gap.

### G5 — Scale and ownership

152,532 LOC. `src/main.rs` is 36,640 lines. `src/durable.rs` is 16,691.
`main.rs` holding 24% of the crate is a hazard independent of LangGraph.

### G6 — Ambiguous stopping

`durable.rs:299 transition()` is a hand-written legal-transition table
(`ClaimWatch --Verify--> IllegalTransition`, etc.). It encodes real
knowledge, but it is a parallel authority on top of the reducer: the
reducer's behaviour and the transition table can drift.

---

## Part 4 — Recommendations

### ADOPT: nothing wholesale

Adding LangGraph means: a second graph engine beside `toron-workflow` +
Resonate; Python in a Rust stack (or a JS sidecar); LangChain coupling for
the parts that matter (callbacks, `store`, tracing); and a second
checkpoint store to reconcile against ours. For a stack whose P1 bead is
"remove duplicated authority," this is the duplicated authority.

It would also lose, on adoption: our fail-closed journal, our signed
remote approval, our closed phase vocabulary, our lint gates, our honest
run ledger, and the Nostr-native identity model. None of those exist in
LangGraph in any form.

### BORROW

**B1 — Per-state-key reducers. (urgent, this is ADR-0016's mechanism)**
LangGraph's L1 is the concrete answer to G2: not "be careful", but *every
state key names its own merge function and there is exactly one writer per
key.* Promote `ReduceOp` from a `reduce`-action-local enum to a property of
each field in the state schema, and make the reducer the only mutation path.
This is the single highest-leverage thing in the comparison, and it is a
design move, not a dependency.

**B2 — Checkpoint per tick + pending writes. (the capability gap)**
Our Tick Law makes this unusually cheap: `tick` is already pure, so a state
snapshot is a value, not a reconstruction. Two pieces:

- **state log**: after each `tick`, write `(seq, model, next_msg)` keyed by
  `(thread_id, seq)`. A ULID/seq-ordered key gives O(1) "latest" and O(1)
  "by id".
- **pending writes**: journal each `Cmd`'s effect row *before* the effect
  runs — which `effect_journal.rs` already does per effect. On resume from a
  checkpoint, an effect with a `Settled` row is not re-run; one that is
  `InFlight` is the honest-unknown case the journal already models.

That single change buys L3 + L4 + L7 at once: replay, fork, and
partial-tick recovery, from machinery we already own.

**B3 — Time travel as a verb. (the user-visible win)**
Once B2 lands: `flywheel workflow replay <run_id> --from <seq>` and
`--fork`. Today the fastest route to "why did that run do that" is grepping
JSONL. This is the change an operator would notice.

**B4 — Durability-mode vocabulary. (cheap)**
We have two points (`Recording`, `Enforced`). LangGraph names three
(`exit`, `async`, `sync`). Ours are *better* — `Enforced` fails closed
because it is a correctness switch, not a performance knob. Keep ours, but
name the third point explicitly (`Deferred`?) so the choice is visible
rather than implied. Do not adopt a purely performance-oriented ladder.

**B5 — Checkpointer conformance suite. (cheap, prevents G-class drift)**
LangGraph ships `langgraph-checkpoint-conformance` so any backend can prove
it honours the contract. We should require the same of `effect_journal`
backends and any future state-log store. Directly targets G6 drift.

**B6 — Read `NodeTimeoutError`, do not import it.**
LangGraph's idle-vs-run timeout with `refresh_on="heartbeat"` and an explicit
`runtime.heartbeat()` is more expressive than our breaker. Read it before
redesigning `breaker.rs`. Importing it would mean importing the Python
runtime to get it.

### REJECT

- **LangGraph as the engine.** Redundant with `toron-workflow` + Resonate,
  and hostile to our dependency direction.
- **LangSmith deployment / Studio.** herdr panes are our live UI and they are
  better for this workload (a real terminal, real agents). A second UI is
  more surface, not less.
- **User-supplied reducer lambdas in a dynamic language.** Our closed
  vocabularies and lint gates are a deliberate safety choice made at real
  cost. LangGraph's flexibility is a feature for prototyping and a liability
  for a fail-closed ledger.
- **Idempotency as the author's job (L11).** Strict regression. Keep the
  framework-enforced `Enforced` mode.
- **`@task`-style intra-node checkpointing as the answer to re-execution.**
  B2's per-effect journal gives finer granularity with less machinery.

### WATCH

LangGraph 1.2 moved hard into our territory: per-node `RetryPolicy`,
`TimeoutPolicy`, `error_handler` with `Command` routing, and `set_node_defaults`.
They are converging on `ActionStage` + `ProviderErrorClass` + our breaker,
from the Python side, a year later. That is a signal our model is the more
complete one — and that their `error_handler(state, error) -> Command`
Saga-compensation pattern is worth mirroring in our compose grammar, which
today has `retry(n):` but no first-class compensating action.

---

## Part 5 — Honest limits of this comparison

- **LangGraph was read, not run.** Judged from official documentation and
  the repository README. No benchmark, no side-by-side execution. Any claim
  about relative performance is absent here by design.
- **Version-sensitive.** Timeouts, per-node error handlers, `set_node_defaults`,
  and `DeltaChannel` all require `langgraph>=1.2`; `DeltaChannel` is beta.
- **Single-process assumption is the deepest mismatch.** LangGraph assumes one
  process owns the graph. Our hardest problems — presence forgery, captain
  FCFS, the busy-owner fence, delivery cursors, reservations — are *distributed
  consensus* problems. LangGraph has nothing to say about any of them, so a
  large fraction of our stack is simply out of its scope. That is why this is
  "borrow four ideas", not "evaluate a migration".
- **Our own numbers are from `wc -l` and grep.** The G1/G2 conclusions are
  absence-of-evidence plus ADR-0016's own statement; they should be confirmed
  against `bd-6si7`'s plan before B1/B2 are sized.

## Part 5 — What implementation changed (2026-09-26)

Reading the code to implement B5 falsified one of my own claims and corrected
two design assumptions. All three are recorded here rather than quietly fixed,
because the corrections are the useful part.

### Correction 1 — B5's premise was wrong. The coverage already existed.

I wrote that `EffectJournalMode::Enforced` had "zero tests". That was measured
wrong: I grepped `tests/` and missed the in-crate `#[cfg(test)] mod` inside the
source. `src/effect_journal.rs` already carries ~20 async tests pinning
intent-before-provider, settle-on-delivery, `Unknown`-vs-`Failed`,
terminal-replay-without-redispatch, and the verify sink's
observe-don't-re-perform rule. `toron-workflow`'s suite pins that `Enforced`
refuses an uncovered leaf.

**B5's real value is narrower and still real:** nothing pinned flywheel's own
*choice* of mode at each engine construction site. That is a wiring decision,
unreachable by calling a unit, so it is now pinned as a source contract in
`tests/journal_mode_invariant.rs` (5 tests, mutation-verified).

The invariant it pins is conditional, which is the subtle part:

> `Enforced` IFF a journal is present, on an arm that really dispatches.

Both failure directions are bugs. `Enforced` unconditionally turns a journal
outage into every-live-fire-fails. `Recording` unconditionally on a live arm is
the silent bypass ADR-0032 Spec 6 forbids. `pane.rs` additionally gates on the
caller's live `journal` flag, because one constructor serves both live and dry
arms and a dry arm must not be promoted when a journal happens to be open.

### Correction 2 — the state-log snapshot point is NOT the tick boundary

The Tick Law's natural reading is "snapshot where the fold ends". Wrong here.
`src/doorbell_host.rs:342` calls `handle_workspace_cmds(&mut self.model, ...)`,
which **mutates the model after the tick returns**. A snapshot taken at the tick
boundary would be an incomplete model and would not resume.

The only correct resume point is after the effect layer returns, because that is
the state the next tick will read. The interpreter loop is:

```text
BrainMsg -> wmsg -> workspace::tick(&self.model, &wmsg) -> (next, cmds)
  self.model = next                                       // fold
  expand_reclassify_cmds(...)                             // pure
  handle_workspace_cmds(&mut self.model, &cmds, ...)      // effects, MUTATES model
  cmds -> Effect::Prompt / FetchInbox / ...               // brain effects
```

### Correction 3 — B2's serialization cascade is real

`WorkspaceModel` is not `Serialize` (`src/workspace.rs:253` derives only
`Debug, Clone, PartialEq, Eq, Default`). The cascade is bounded:
`WorkspacePolicy`, `WakeModel`, `Receipt`, and `Question` already derive it;
`DoorbellModel` (`crates/doorbell-core/src/lib.rs:624`) and `Presence` do not,
plus their nested types. Two crates, not thirty types.

### What B2/B3 actually need (not yet built)

1. Derive `Serialize`/`Deserialize` on `DoorbellModel` + `Presence` + nested.
2. A state-log store. This needs a **new migration in `toron-core`** (next free
   number after `0012_event_log.sql`) — cross-repo, not a flywheel-only change.
3. Wire the snapshot into `RtHost` at the corrected point above.
4. Reuse the effect journal's `outcome_json` for skip-and-replay. This one is
   **already viable**: `WorkflowEffect` persists `outcome_json` and
   `failure_json`, so a `Settled` row can be replayed as a result message
   without re-invoking the provider.
5. Retention. Checkpoints grow unbounded — LangGraph hit this too and needed
   `DeltaChannel` (beta) to mitigate.
6. Replay/fork verbs, with fork dry-by-default.

Steps 1-3 land on the same model `bd-6si7` is reshaping, which is why B1 was
left with that bead. B2/B3 should be their own bead, sequenced after or
alongside `bd-6si7` rather than racing it.

### Unrelated pre-existing failures found while verifying (not mine, not fixed)

- **`bug_fix_cli_control_flow_reports_all_nodes`** (`tests/sc_bugfix_playbook.rs`)
  fails with `unresolved template ref {{inputs.bead_id}}` in
  `repair-until-verified` node `repair-1`. This is the SAME class commit
  `7f9e117` fixed for `{{inputs.coder_profile}}` (bd-adv4) — an input ref with
  no declared default. No open bead owns it.
- `event_poll_parity_unit_suite_is_the_default_gate` was a stale static gate
  asserting seven `durable.rs` test names that commit `7f9e117`
  ("test-audit -25, no coverage loss") had deliberately triaged away. Gate
  removed; the live integration test in the same file is kept and passing.

---

## Sources

- <https://github.com/langchain-ai/langgraph> (README)
- <https://docs.langchain.com/oss/python/langgraph/graph-api>
- <https://docs.langchain.com/oss/python/langgraph/persistence>
- <https://docs.langchain.com/oss/python/langgraph/checkpointers>
- <https://docs.langchain.com/oss/python/langgraph/fault-tolerance>
- Local: `src/workspace.rs`, `src/durable.rs`, `src/durable_resume.rs`,
  `src/durable_transition.rs`, `src/state.rs`, `src/main.rs`,
  `docs/decisions/0016-functional-effect-kernel-redesign.md`
- Local: `../toron/crates/toron-workflow/src/{engine/mod.rs,engine/boundary.rs,engine/sinks.rs,schema.rs}`
