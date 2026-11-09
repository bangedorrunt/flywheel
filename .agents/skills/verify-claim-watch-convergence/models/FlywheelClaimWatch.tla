---- MODULE FlywheelClaimWatch ----
EXTENDS Naturals, TLC

CONSTANTS FixPauseAndVerifierGating

VARIABLES
    ledger_state,
    watch_active,
    unclaimed_polls,
    grace_ticks,
    has_worker_receipt,
    verifier_dispatched,
    locks_released,
    reopened_by_loop

vars == <<
    ledger_state,
    watch_active,
    unclaimed_polls,
    grace_ticks,
    has_worker_receipt,
    verifier_dispatched,
    locks_released,
    reopened_by_loop
>>

Init ==
    /\ ledger_state = "Ready"
    /\ watch_active = TRUE
    /\ unclaimed_polls = 0
    /\ grace_ticks = 0
    /\ has_worker_receipt = FALSE
    /\ verifier_dispatched = FALSE
    /\ locks_released = FALSE
    /\ reopened_by_loop = FALSE

WorkerClaim ==
    /\ watch_active
    /\ ledger_state = "Ready"
    /\ ledger_state' = "InProgress"
    /\ unclaimed_polls' = 0
    /\ UNCHANGED <<watch_active, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

PollUnclaimed ==
    /\ watch_active
    /\ ledger_state = "Ready"
    /\ unclaimed_polls < 2
    /\ unclaimed_polls' = unclaimed_polls + 1
    /\ watch_active' = IF unclaimed_polls + 1 >= 2 THEN FALSE ELSE TRUE
    /\ UNCHANGED <<ledger_state, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

StripVerifyFence ==
    /\ watch_active
    /\ ledger_state = "Ready"
    /\ ledger_state' = "NotReadyOpen"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

PollNotReadyOpen ==
    /\ watch_active
    /\ ledger_state = "NotReadyOpen"
    /\ watch_active' = FALSE
    /\ UNCHANGED <<ledger_state, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

PauseAsBlocked ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ledger_state' = "Blocked"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

PauseByCaptainHold ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ledger_state' = "CaptainHeld"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

WorkerEmitReceipt ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ~has_worker_receipt
    /\ has_worker_receipt' = TRUE
    /\ UNCHANGED <<ledger_state, watch_active, unclaimed_polls, grace_ticks, verifier_dispatched, locks_released, reopened_by_loop>>

WorkerCloseLegal ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ledger_state' = "ClosedLegal"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

WorkerCloseMissingVerdict ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ledger_state' = "ClosedMissingVerdict"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

WorkerCloseNoCommit ==
    /\ watch_active
    /\ ledger_state = "InProgress"
    /\ ledger_state' = "ClosedNoCommit"
    /\ UNCHANGED <<watch_active, unclaimed_polls, grace_ticks, has_worker_receipt, verifier_dispatched, locks_released, reopened_by_loop>>

SwarmReleaseReservationsStep ==
    /\ watch_active
    /\ ~(ledger_state \in {"Ready", "InProgress", "NotReadyOpen"})
    /\ IF FixPauseAndVerifierGating /\ (ledger_state \in {"Blocked", "CaptainHeld"}) THEN
           /\ watch_active' = FALSE
           /\ grace_ticks' = grace_ticks
           /\ reopened_by_loop' = FALSE
           /\ verifier_dispatched' = FALSE
           /\ locks_released' = FALSE
       ELSE IF ledger_state = "ClosedLegal" THEN
           /\ watch_active' = FALSE
           /\ grace_ticks' = grace_ticks
           /\ reopened_by_loop' = FALSE
           /\ verifier_dispatched' = TRUE
           /\ locks_released' = TRUE
       ELSE IF ledger_state = "ClosedMissingVerdict" /\ has_worker_receipt /\ grace_ticks < 1 THEN
           /\ watch_active' = TRUE
           /\ grace_ticks' = grace_ticks + 1
           /\ reopened_by_loop' = FALSE
           /\ verifier_dispatched' = IF FixPauseAndVerifierGating THEN FALSE ELSE TRUE
           /\ locks_released' = FALSE
       ELSE
           /\ watch_active' = FALSE
           /\ grace_ticks' = grace_ticks
           /\ reopened_by_loop' = TRUE
           /\ verifier_dispatched' = IF FixPauseAndVerifierGating THEN FALSE ELSE TRUE
           /\ locks_released' = TRUE
    /\ UNCHANGED <<ledger_state, unclaimed_polls, has_worker_receipt>>

Done ==
    /\ ~watch_active
    /\ UNCHANGED vars

Next ==
    \/ WorkerClaim
    \/ PollUnclaimed
    \/ StripVerifyFence
    \/ PollNotReadyOpen
    \/ PauseAsBlocked
    \/ PauseByCaptainHold
    \/ WorkerEmitReceipt
    \/ WorkerCloseLegal
    \/ WorkerCloseMissingVerdict
    \/ WorkerCloseNoCommit
    \/ SwarmReleaseReservationsStep
    \/ Done

Spec == Init /\ [][Next]_vars

NonTerminalPauseNeverReopensOrDropsLocks ==
    (ledger_state \in {"Blocked", "CaptainHeld"})
        => (~reopened_by_loop /\ ~locks_released)

VerifierDispatchedOnlyOnLegalClose ==
    (ledger_state \in {"ClosedNoCommit", "ClosedMissingVerdict"})
        => ~verifier_dispatched

====
