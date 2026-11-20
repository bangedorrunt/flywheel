---
name: verify-protocol-tla
description: Run hand-written TLA+ protocol models with everystate (native, no Java) — toron job claim/lease/settle (fencing) and the reconciler respawn's one-live-pane-per-pin plus reservation retention (ADR-0022 V11). Use whenever changing Journal::claim/settle/attempt fencing, job lifecycle handling, envpin respawn ordering, taken-name clearing, or pin reservations on respawn.
---

# Verify Protocol Fences (`flywheel` + sibling `toron`)

Synced to commit: `ccf015a1` (models + `scripts/verify-formal.sh`)

`bash scripts/verify-formal.sh` runs the hand-written TLA+ models with a pinned
everystate and asserts both directions: the models of the code as written pass, and
every negative fixture (one fence removed) fails with its named invariant.
Read any counterexample with `/statechart` — it renders each violation as its
shortest path in the code's words. No commands from `/statechart` belong here.

## What is verified

### 1. `NoSteal` — a foreign live lease is never stolen
- **Plain statement**: When one owner's job lease is still live, a second owner can never take the row; it must wait for the deadline.
- **TLA+ invariant**: `stolen = FALSE` (`models/JobLease.tla`), where `stolen` is set only by a `Claim` that crossed a live foreign lease.
- **Rust correspondence**: `check_claim` lease-live arm (`../elm-effect-rs/crates/elm-effect-journal/src/plan.rs`), called by `Journal::claim` (`../toron/crates/toron-core/src/journal.rs:269`).
- **Negative fixture**: `models/JobLease.broken-steal.cfg` (`Bug = "steal"`) removes the lease-live refusal; everystate reports `NoSteal` violated.

### 2. `NoStaleSettle` — a stale generation never settles
- **Plain statement**: A worker that lost its lease (older generation) cannot settle the row; only the current generation can.
- **TLA+ invariant**: `stale_settled = FALSE`, set only by `ZombieSettle`.
- **Rust correspondence**: `check_settle` generation fence (`../elm-effect-rs/crates/elm-effect-journal/src/plan.rs`), used by `Journal::settle` (`../toron/crates/toron-core/src/journal.rs:303`) and `Journal::attempt` (`:326`).
- **Negative fixture**: `models/JobLease.broken-stale-settle.cfg` (`Bug = "stale-settle"`); everystate reports `NoStaleSettle` violated.

### 3. `TerminalStable` — settled or failed work is never revived
- **Plain statement**: A terminal row (`settled` / `failed`) never changes owner or generation again.
- **TLA+ invariant**: `revived = FALSE`, set only by `ReviveTerminal`.
- **Rust correspondence**: `check_claim` and `check_settle` terminal arms (`../elm-effect-rs/crates/elm-effect-journal/src/plan.rs`); the row states are the `EffectState` values journalled by toron job kinds 43001–43006 (`../toron/crates/toron-core/src/job_kinds.rs`).
- **Negative fixture**: `models/JobLease.broken-revive-terminal.cfg` (`Bug = "revive-terminal"`); everystate reports `TerminalStable` violated.

### 4. `OneLiveHolder` — at most one live pane holds a pin (ADR-0022 V11)
- **Plain statement**: A respawn may never leave two live panes holding the same pin: the retired pane's binding is released before the replacement claims the pin, and only then is the old pane closed.
- **TLA+ invariant**: `Cardinality({p \in Panes : p \in alive /\ holder[p] = Pin}) <= 1` (`models/Respawn.tla`).
- **Rust correspondence**: the taken-name refusal/clear in `start_agent_with_taken_name_clear` (`src/main.rs:7173`), the respawn order in `envpin_hop_respawn` (`src/main.rs:7840`) and the reroute sites around `SiteHop` (`src/main.rs:7936`), and the reconciler in `src/seat_reconcile.rs`.
- **Negative fixture**: `models/Respawn.broken-claim-before-free.cfg` (`Bug = "claim-before-free"`) is the ADR's variant that closes the old pane after the new one claims; everystate reports `OneLiveHolder` violated.

### 5. `ReservationsKept` — a respawn keeps the pin's reservations (ADR-0022 V11)
- **Plain statement**: Respawning a pin must not drop file reservations it holds.
- **TLA+ invariant**: `dropped = FALSE` (`models/Respawn.tla`).
- **Rust correspondence**: the reservation hand-off around `envpin_hop_respawn` / `seat_reconcile` (reservations are keyed by reason bead, not pane).
- **Negative fixture**: `models/Respawn.broken-drop-reservations.cfg`; everystate reports `ReservationsKept` violated.

### 6. `NoClosedRowLost` — a CAS retry never reverts a landed tracker row
- **Plain statement**: two ship lands (or `flywheel commit`s) CAS on HEAD; when one loses the race and retries, it re-splices its row into the *moved* HEAD, so a row another lander closed in between survives (bd-kz3sc rework 2, bd-aberm).
- **TLA+ invariant**: `\A a \in Landers : done[a] => own(a) \in headRows` (`models/TrackerCas.tla`).
- **Rust correspondence**: `commit_tree` CAS loop and `commit_blobs` splice closure (`src/cmd/commit.rs`), `tracker_blobs_for_bead` (`src/cmd/ship.rs`).
- **Negative fixture**: `models/TrackerCas.broken-stale-splice.cfg` (`Bug = "stale-splice"`) reuses the first splice on retry; everystate reports `NoClosedRowLost` violated.

### 7. `BoundedRetries` / `Termination` — retry loops end
- **Plain statement**: a lander never retries past `MaxRetries`; every lander eventually either returns ok or exhausts its budget.
- **TLA+ checks**: invariant `BoundedRetries` and temporal property `Termination` in `models/TrackerCas.tla`, asserted by the positive config.
- **Negative fixture**: none (this is a liveness/termination bound, not a fence).

## How to run

```bash
bash scripts/verify-formal.sh            # verify (positive + negative fixtures)
cargo install everystate --version 0.4.1 --locked   # once, to install the checker
```

Pinned tool: [everystate](https://github.com/mattzcarey/everystate) 0.4.1, a
native TLA+ model checker that reads the same `.tla` and TLC `.cfg` files and
keeps TLC's exit codes. There is no Java, jar, or JVM in this check. The script
reads `FLYWHEEL_EVERYSTATE`, then `everystate` on `PATH`, and refuses any other
version. Each run checks a temp copy with `-json -deadlock` and compares the
JSON verdict (`status`, violated `name`) to the expectation.
`scripts/clean-proof.sh` runs this script as part of the clean-export proof.

Migration evidence (2026-11-20): every model and negative fixture was run on
TLC 1.8.0 and on everystate 0.4.1. All 43 checks matched on exit code and on
the violated invariant, and every passing model matched on its distinct-state
count. TLC is no longer used anywhere.

Ad hoc: `everystate -json -deadlock -config <cfg>.cfg <Model>.tla` prints one
JSON line whose `trace` is the shortest counterexample; `-workers auto` searches
on every core with identical results.

### Trace conformance (bd-uawqo)

A model check verifies the model, not the Rust. `everystate conform -trace
run.jsonl <Model>.tla` checks a recorded run of the real code against a model.
The effect journal is already ordered and written before each effect runs, so
it is a faithful recording.

- Pure converter: `crates/doorbell-rt/src/effect_trace.rs` maps `DeferredQueue`
  journal rows to observations naming the spec action (`Arrive`,
  `RereadByTheIdleEdge`, `ReachPrompt`, `Ring`, `Idle`, `Audit`) and the values
  of every model variable. No clock, socket or env in the converter.
- Recorder: `tests/conformance_deferred_queue.rs` drives `CoreReducer` through
  a deferral, a second producer's copy, a watch restart and the boot drain,
  writes the real journal through `FileStore`, converts it and checks the
  trace. Set `CONFORM_TRACE_OUT`/`CONFORM_BAD_TRACE_OUT` to regenerate the two
  fixtures; with `FLYWHEEL_EVERYSTATE` set the test runs both checks itself.
- Fixtures: `models/traces/DeferredQueue.recorded.jsonl` (conforms, exit 0)
  and `models/traces/DeferredQueue.double-ring.jsonl` (a doctored second
  `Ring`, exit 20 naming the step). Both are wired into `verify-formal.sh` via
  `expect_conform`/`expect_nonconform`, so `clean-proof.sh` gates on them.
- No Java: the same pinned `everystate` 0.4.1 binary does model checking and
  conformance.

Coverage is still partial: only the doorbell deferral/drain is recorded today;
the commit/ship-land CAS (`TrackerCas`) is the next trace to add.

## Bounds, assumptions, limitations

- **Bounds**: `Owners = {o1, o2}`, `LeaseMs = 1`, `MaxTime = 2`, `MaxGen = 3` (JobLease); one pin over panes `{old, new}` (Respawn); `Landers = {l1, l2}`, `MaxRetries = 2` (TrackerCas). Small bounds keep each run under a second.
- **Abstractions**: time is a bounded tick, not a clock; retries, backoff, pane transport (hc/rex), mail, and CLI serialization are out of scope; reservations are one abstract set, not the `bd-…` reason keys; heartbeat/renew is not modelled (leases only expire).
- **What a passing check does NOT prove**: it does not prove the Rust code matches the model — the coverage table is the review surface for that — nor correctness under more than two owners, more than one pin, or an unbounded generation count.

## Coverage record

| Code symbol | File & lines | TLA+ action / operator |
|---|---|---|
| `Journal::claim` | `../toron/crates/toron-core/src/journal.rs:269` | `Claim` |
| `check_claim` (terminal, generation, lease-live) | `../elm-effect-rs/crates/elm-effect-journal/src/plan.rs:380` | `Claim` guard, `Terminal`, `ForeignLiveLease` |
| `Journal::settle` / `check_settle` | `../toron/crates/toron-core/src/journal.rs:303`, `plan.rs:523` | `Settle`, `NoStaleSettle` |
| `Journal::attempt` fencing | `../toron/crates/toron-core/src/journal.rs:326` | `NoStaleSettle` |
| job lifecycle 43001–43006 | `../toron/crates/toron-core/src/job_kinds.rs` | `Terminal` |
| `start_agent_with_taken_name_clear` | `src/main.rs:7173` | `ClaimNew` guard, `FreeOld` |
| `envpin_hop_respawn` / `SiteHop` reroute sites | `src/main.rs:7840`, `:7936` | `SpawnNew`, `ClaimNew`, `CloseOld` |
| reconciler respawn | `src/seat_reconcile.rs` | `CloseOld` (reservations) |
| `commit_tree` CAS loop | `src/cmd/commit.rs` | `CasOk`, `CasFail`, `Exhaust` |
| `commit_blobs` splice closure | `src/cmd/commit.rs` | `Refresh` (re-splice on moved parent) |
| `tracker_blobs_for_bead` | `src/cmd/ship.rs` | `Refresh`, `own(a)` |

## When to update

Update the model and this table whenever: `Journal::claim`/`settle`/`attempt`
fencing changes; terminal states are added; respawn ordering changes (anything
that claims the pin before releasing the retired binding); reservation
hand-off on respawn changes. Move the sync commit forward only after
confirming the model still describes the code at the new commit.

## Not covered & related skills

- **Not covered**: Elm durable workflow journal (`effect-kernel`),
  claim-watch convergence (`.agents/skills/verify-claim-watch-convergence/SKILL.md`),
  pane respawn backoff.
- **Related skills**: `/statechart` (read counterexamples),
  `/maintain-formal-verification-skill` (bring a stale verification back in line).
