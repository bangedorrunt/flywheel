# Effect-oriented stack review: flywheel x toron x chiebukuro

Date: 2026-10-01. Bead: bd-rflj. Reviewer: Buffy (read-only across all three repos;
every finding cites the code path or doc line it came from).
Lens: `~/.agents/skills/effect-oriented/` (SKILL.md, references/architecture.md,
references/patterns.md, references/rust.md), revised 2026-10-01 (Effect 4.0-era
TypeScript, fan-out/reduce, durable effects, delivery cursors).
Follow-up filed: bd-0fvb.

> **Status addendum 2026-10-02.** All four findings below are now closed, so
> this file is kept for its evidence and no longer as a work list.
> F1 closed by bd-0fvb (`durable.rs:8433` resolves authority once at the host
> edge; the guard builds its forbidden strings with `concat()` so an adjacent
> comment cannot satisfy the scan). T1 closed by bd-7n2a (fail-after-collect,
> documented at `control_flow.rs:1607`/`:1664`: every copy is a journaled durable
> child, so a sibling failure must not cancel it). T2 closed by bd-eqwq (ADR-0034
> frontmatter is `status: accepted`). C1 closed by bd-6mt (`RetryStep` /
> `jittered_delay` / `step_after_failure` in chiebukuro's `src/bridge/llm.rs`).
> The flywheel verdict stays **conditional** and that is still true: `swarm_loop`
> is migrated, other durable leaves still sample wall time. Skill-surface findings
> from the same pass live in
> [stack-skill-review-2026-10-02.md](stack-skill-review-2026-10-02.md).

## Verdicts at a glance

| Layer | Verdict | One line |
|---|---|---|
| flywheel | conditionally compliant (its own audit says so, and that is still true) | Reducer fold, effect journal, and cursor discipline are the paradigm's durable half done well; the authority/clock injection migration is the one open item |
| toron | Compliant; the engine is the stack's showcase | Journal + reconcile + one retry coordinator + bounded fan-out; two paper cuts below |
| chiebukuro | Compliant on the memory plane; the LLM bridge is the soft spot | Claim/lease derivation and a budgeted Dreamer; retry/timeout policy lives as constants in the bridge |

## flywheel

### What is strong

- **Reducer fold.** `src/workspace.rs` (`tick`) is a pure `(Model, Msg) -> (Model, Vec<Cmd>)`
  with same-log-same-effects tests; `doorbell-core::tick` and `durable::tick` are
  private slice reducers; time and policy enter as message/model data. The laws and
  checklist are in `docs/agent-instructions/effect-kernel.md`.
- **Durable effects journal.** `src/effect_journal.rs` builds the effect runner and
  the `RunJournal` from one store; live arms are `Enforced` (an effectful leaf with
  no injected runner refuses), dry arms are `Recording` (marker, not refusal).
- **Delivery cursors.** `crates/doorbell-rt/src/brain.rs` (`on_watch` persists the
  plane-S cursor before classification; `on_inbox_fetched` advances only on
  `inbox_high_water_from_values`; `fetch_all_inbox` backfills from the 24h floor) and
  `crates/doorbell-core/src/lib.rs` (`redelivery_floor_rfc3339`, `note_wake` ratchet).
  Documented this week in `docs/agent-instructions/effect-kernel.md` (commit 8d44e52).
- **Substitution tests.** `doorbell-rt` scenario tests script a failing write-ahead
  flush and a failing wake save; pane fan-out has an end-to-end test
  (`pane::tests::fanout_copies_reach_distinct_panes`, cross-referenced from toron's
  `tests/fanout_copy_ordinal.rs`).

### Gaps

- **F1, material: durable loop leaves still self-resolve authority and sample wall
  time.** `src/durable.rs:6017` (`swarm_loop`, a `#[function]` durable leaf) calls
  `RuntimeAuthority::resolve()`, and stamps `now_us`/`now_unix` at
  `src/durable.rs:6781, 6877, 6917, 7817, 7962`. The bd-6si7 comment at
  `src/durable.rs:8533-8537` states the intended shape (the host edge reads the
  clock once and hands the value down); `swarm_loop` has not adopted it. The audit
  text in `docs/agent-instructions/effect-kernel.md` ("Current audit status") names
  exactly this and remains accurate. Outside reducers, so replay witnesses still
  hold, but the strict authority contract is unmet. Filed as bd-0fvb.
- **F2, note: flywheel's own fan-outs are process-level, not combinator fan-out**
  (no `join_all`/`buffer_unordered` in `src/`). They are bounded by host budgets:
  crew cap 5, dispatcher budget 5/30/3, per-workspace loop ticks. No action needed;
  recorded so the boundary with the engine's fan-out (below) is explicit.

## toron

### What is strong

- **Effect journal and run lifecycle.** ADR-0032 (`docs/decisions/0032-...md`)
  accepted; `crates/toron-workflow/src/engine/boundary.rs` (normalized action
  boundary, retry candidacy, execution policy), `engine/reconcile.rs`
  (unknown-outcome reconciliation, `batch_limit: 500`), `engine/park.rs`;
  `EffectJournalMode::{Enforced, Recording}`; engine-side `RunJournal` rows agree
  with effect rows on `run_id` by construction.
- **One retry coordinator.** `engine/control_flow.rs:345-358` (`RetryCoordinator`);
  `lint.rs:673-679` refuses nested retry so one coordinator owns attempts and
  backoff; `crates/toron-core/src/policy.rs` (ADR-0034 G5) carries `Timeout` and
  `RetryPolicy` as values, with pure delay math so tests substitute without
  sleeping.
- **Bounded fan-out.** `engine/control_flow.rs:1594` `MAX_PARALLEL_FANOUT = 32`
  with a loud refusal above the cap; each copy carries a distinct effect key
  (`iteration_key: p{i}`, `fanout_copy_ordinal.rs`); `reduce` is an explicit
  bounded fold (`tests/reduce_action.rs`); dispatch concurrency is a semaphore
  (`dispatch.rs:48`).
- **Substitution tests.** `tests/control_flow_parity.rs` injects recording
  runners and scripts the degradations the ADR forbids; a parallel node never
  reaches the runner raw (`src/engine/tests.rs:787-883`).
- **The seven ADR-0034 gaps landed** (bd-lzo6 closed 2026-09-19; G1-G7, toron-core
  lib 666/0 in the close reason; `policy.rs` carries `// governed-by: ADR-0034 (G5)`).

### Gaps

- **T1, material-ish: `run_parallel` has no sibling-cancellation path.** No
  `cancel` call exists anywhere in `engine/control_flow.rs` (the only match is the
  `"cancelled"` failure-kind mapping). `run_parallel` (`control_flow.rs:1605-1674`)
  dispatches every copy as a durable child, awaits them in order, and on a failed
  copy returns `{ok: false, error, results}` only after all copies settled. For
  journaled durable effects this is a defensible deliberate choice (aborting a copy
  strands its journal row), but the lens requires a stated cancellation story and a
  partial-failure test; tests currently cover the happy path and the cap only.
  Decide and document, or add a cancel-on-first-failure mode for non-durable copies.
- **T2, paper cut: ADR-0034 status drift.** The file still reads
  `status: proposed` ("Acceptance is a captain word on this file") while the bead
  is closed and all seven gaps landed. A reader will treat the gaps as open work.

## chiebukuro

### What is strong

- **Claim/lease discipline on the memory plane.** `src/memory/derive.rs` reports
  `claimed`/`reclaimed`/`released` counters, reclaims stale claims once the lease
  expires (ADR-0039), and converges on replay ("replay and duplicate delivery
  converge instead of...", ~line 196); the derive pass is bounded
  (`max_sessions x max_events_per_session`).
- **Budgeted Dreamer.** `src/memory/dreamer.rs` bounds every pass
  (`max_wall_time_secs`, `max_conclusions`, `max_peers`, ~lines 222-237), records a
  ledger row for idempotency (~line 483), and has `dry_run_is_stable_and_mutates_nothing`
  and `apply_pass_is_idempotent_at_same_revision` tests (~lines 632, 664).
- **One global LLM permit.** `src/bridge/concurrency.rs` (process semaphore at
  line 14/59-65 plus an optional cross-process flock, ~70-81); the architecture
  contract says every LLM call acquires it (`docs/agent/architecture.md`).
- **Boundaries hold.** Atomic writes via `ChiebukuroStore`; protected-store locks
  released by drop; model routing resolved once (`src/bridge/roles.rs`), never read
  ad hoc; no production `unwrap()`/`expect()` and `sanitize_output()` on user-facing
  errors (`AGENTS.md`).

### Gaps

- **C1, minor: policy constants live in the bridge, not as values.**
  `src/bridge/llm.rs` bakes a 300s call timeout and 10s connect timeout (~lines
  267-268) and owns an inline retry loop (429 `retry_after` support, ~325-354). The
  lens would pass a `Timeout`/`RetryPolicy` value and keep the bridge as the
  executor. The bridge is the edge, so this is a pattern-shape note, not a
  correctness defect; the repair-only retry (ADR-0035 D4) is a good precedent for
  handling a complex case inline with a named decision.

## Cross-cutting

- **The vocabulary agrees across layers.** intent/unknown/replay/lease/cursor are
  the same nouns in flywheel ADR-0012, toron ADR-0032, and chiebukuro ADR-0039.
  That is why the stack composes: the durable half speaks one language.
- **The strongest repeated pattern:** every honesty claim is carried by a named
  state plus a test (unknown tables, replay guards, lease counters, atomic
  occupancy tests), never by prose alone.
- **The weakest repeated pattern:** policy constants at the edge instead of
  injected values (flywheel's clock in durable leaves, chiebukuro's LLM timeouts).
  Toron's `policy.rs` is the counterexample to copy.

## Follow-ups

All closed as of 2026-10-02; see the status addendum at the top.

- bd-0fvb: thread RuntimeAuthority (clock + home) into durable loop leaves
  (flywheel; filed from this review). CLOSED.
- T1/T2 (toron) and C1 (chiebukuro) were report-only here; all closed
  (bd-7n2a, bd-eqwq, bd-6mt).
