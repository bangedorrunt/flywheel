---
name: verify-claim-watch-convergence
description: Formally verify Flywheel's bead claim-watch and post-close convergence state machine in src/durable.rs (claim_watch_step_with_known, partition_ready, and ClaimWatchAction::ReleaseReservations). Use whenever changing claim-watch transitions, close-check grace ticks, verifier dispatch gating, or lock release on bead close/reopen.
---

# Verify Claim-Watch & Post-Close Convergence (`flywheel`)

Synced to commit: `2e4f63bbf2cad2eb4484cd818e4b0c60ba8327cc`

This skill verifies the `ClaimWatch` and post-close convergence state machine in `src/durable.rs` that reconciles each watched `bead_id` against `ready` (`__ready`), `inprog` (`__inprogress`), `not_ready` (`partition_ready(&ready)`), and `__close-check`.

## Properties Verified

### 1. `NonTerminalPauseNeverReopensOrDropsLocks`
- **Plain statement**: A watched bead that leaves `ready` and `in_progress` because it entered a non-terminal paused state (`Blocked`, `DeferredFuture`, or `CaptainHeld`) must never be treated as a closed bead (`ClaimWatchAction::ReleaseReservations`), reopened via `__reopen-bead`, or stripped of its file reservations via `__release-bead-locks`.
- **TLA+ invariant**: `(ledger_state \in {"Blocked", "CaptainHeld"}) => (~reopened_by_loop /\ ~locks_released)`.
- **Rust correspondence**: `claim_watch_step_with_known` (`src/durable.rs:4661-4787`), `partition_ready` (`src/durable.rs:6514`), and the `ClaimWatchAction::ReleaseReservations` handler (`src/durable.rs:6822-7081`).
- **Current status**: **Verified & Passing** on `FlywheelClaimWatch.fixed.cfg`. In unpatched code at `2e4f63bb` it was violated in 3 steps (`WorkerClaim -> PauseAsBlocked -> SwarmReleaseReservationsStep`) because `swarm_loop` (`src/durable.rs:6514`) computes `let (dispatchable, not_ready) = partition_ready(&ready);`. `ready` comes from `br ready --json`, which already excludes `Blocked`, future-`Deferred`, and `captain_holds` beads. When an `in_progress` watched bead transitions to `Blocked` or `CaptainHeld`, it is absent from `ready`, `inprog`, AND `not_ready`, so `claim_watch_step_with_known` hits `(None, None)` (`src/durable.rs:4776`) and emits `ClaimWatchAction::ReleaseReservations`, triggering `__close-check` (`no-commit` / `missing`), `__reopen-bead`, `__verify-dispatch`, and `__release-bead-locks`.
  *Fix*: Added `status` inspection in `close_check` returning `CloseCheck::NotClosed { status }`; `ReleaseReservations` preserves locks (`release_locks = false`) and maintains `WaitReason::Blocked`. Regression witness: `verdict::tests::close_check_not_closed_when_bead_status_is_not_closed` and `durable::tests::close_path_not_closed_preserves_locks_and_gates_verifier`.

### 2. `VerifierDispatchedOnlyOnLegalClose`
- **Plain statement**: The independent reviewer pane (`__verify-dispatch`) must only be dispatched when `__close-check` confirms a legal close (`status == "legal"`), never when `__close-check` reports `"no-commit"` or `"missing"` (during `WorkerReceipt` grace ticks or when reopening an unverified close).
- **TLA+ invariant**: `(ledger_state \in {"ClosedNoCommit", "ClosedMissingVerdict"}) => ~verifier_dispatched`.
- **Rust correspondence**: `ClaimWatchAction::ReleaseReservations` handler in `src/durable.rs:6822-7067`.
- **Current status**: **Verified & Passing** on `FlywheelClaimWatch.fixed.cfg`. In unpatched code at `2e4f63bb` it was violated in 3 steps (`WorkerClaim -> WorkerCloseMissingVerdict -> SwarmReleaseReservationsStep`) because `if swarm.mark_verifier_dispatched(&bead_id)` (`src/durable.rs:7042`) executes unconditionally after the `match status` block instead of only inside the `"legal"` arm (`src/durable.rs:6855`).
  *Fix*: Explicit `dispatch_verifier` flag required before calling `mark_verifier_dispatched`. Regression witness: `durable::tests::close_path_not_closed_preserves_locks_and_gates_verifier`.

## Coverage Record

| Code Symbol / Step | File & Lines | TLA+ Action / Operator |
|---|---|---|
| `claim_watch_step_with_known` | `src/durable.rs:4661-4787` | `PollUnclaimed`, `PollNotReadyOpen`, `SwarmReleaseReservationsStep` |
| `partition_ready` | `src/durable.rs:4337-4355, 6514` | `StripVerifyFence`, `PollNotReadyOpen` |
| `ClaimWatchAction::ReleaseReservations` (`"legal"`, `"no-commit"`, `"missing"`) | `src/durable.rs:6822-7037` | `SwarmReleaseReservationsStep` |
| `SwarmState::mark_verifier_dispatched` & `__verify-dispatch` | `src/durable.rs:7042-7067` | `verifier_dispatched` in `SwarmReleaseReservationsStep` |
| `__release-bead-locks` | `src/durable.rs:7068-7080` | `locks_released` in `SwarmReleaseReservationsStep` |

## How to Run the Verification

### 1. Model Check the TLA+ Specification with `/statechart`

Copy `.agents/skills/verify-claim-watch-convergence/models/FlywheelClaimWatch.tla`, `FlywheelClaimWatch.cfg` (or `FlywheelClaimWatch.fixed.cfg` when verifying a candidate fix), and `FlywheelClaimWatch.view.ts` to a scratch directory outside the repo, run the `/statechart` skill on `FlywheelClaimWatch.tla`, and read every result with the `/statechart` skill:

- With `FlywheelClaimWatch.cfg` (`FixPauseAndVerifierGating = FALSE`, matching the unpatched code at `2e4f63bb`), TLC explores 30 states and reports the 3-step `NonTerminalPauseNeverReopensOrDropsLocks` and `VerifierDispatchedOnlyOnLegalClose` counterexamples.
- With `FlywheelClaimWatch.fixed.cfg` (`FixPauseAndVerifierGating = TRUE`), TLC explores all reachable states with 0 invariant violations and `NoDeadlock` holding.

### 2. Run the Rust Unit Tests (`flywheel`)

Always build and test `flywheel` with the nightly toolchain under `mbx`:

```bash
mbx +nightly-aarch64-apple-darwin test --lib durable::tests::claim_watch
```

## Assumptions, Bounds, and Limitations

- **Bounds**: Models 1 watched bead (`bead_id`), `unclaimed_polls \in 0..2`, and `grace_ticks \in 0..1` (`< 2`).
- **Abstractions**: `close_nudged` and `close_polls` on still-`InProgress` beads with a landed commit are omitted from the state vector because they remain in `Watching InProgress` until the bead leaves `inprog`.
- **What a passing check does NOT prove**: It does not verify subprocess CLI flag serialization for `__close-check` or `__reopen-bead`, or pane multiplexer transport (`rex` / `hc`).

## When to Update This Skill

Update `models/FlywheelClaimWatch.tla` and the coverage table above whenever:
- `claim_watch_step_with_known` (`src/durable.rs:4661-4787`) adds or changes branches or parameters.
- `swarm_loop` (`src/durable.rs:6514, 6733-6742`) changes how `not_ready` or non-terminal paused beads are passed to `claim_watch_step_with_known`.
- The `ClaimWatchAction::ReleaseReservations` handler (`src/durable.rs:6822-7081`) changes how `"legal"`, `"no-commit"`, `"missing"`, `mark_verifier_dispatched`, or `release_locks` are gated.

## Not Covered & Related Skills

- **Not covered**: Elm durable workflow journal (`effect-kernel`), `verdict.rs` signature verification, or pane respawn backoff (`RespawnState`).
- **Related skills**: `effect-kernel` (`.agents/skills/effect-kernel/SKILL.md`), `/statechart`, `/maintain-formal-verification-skill`.
