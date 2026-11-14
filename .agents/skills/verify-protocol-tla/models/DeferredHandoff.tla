---- MODULE DeferredHandoff ----
\* One pin, one dispatcher: a refused crew wake handed from the CLI to the
\* doorbell (bd-s0uxh, bd-pbng7).
\* Rust correspondence (flywheel):
\*   CLI append + host drain under one lock   src/doorbell.rs
\*                                            append_deferred_wake / take_deferred_wakes
\*   journal before the handoff is rewritten  src/doorbell.rs  journal_deferred_wake
\*   boot refold of journaled, unrung rows    src/doorbell.rs  unrung_deferrals
\*   the per-pin pending queue                src/doorbell_host.rs  RtHost::tick
\* Boundary: the pane transport and the reducer's queue coalescing. This model
\* is about ONE wake surviving a dispatcher restart, which is the defect the
\* install at 21:44Z exposed: the queue was memory-only.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "memory-only"}: negative fixture. "none" is the protocol
\* as specified; "memory-only" makes a restart drop the pending queue instead
\* of re-deriving it from the journal, so NoLostWake must fail.

VARIABLES file, journaled, taken, queued, rung

vars == <<file, journaled, taken, queued, rung>>

Init ==
    /\ file = TRUE          \* the CLI appended one wake
    /\ journaled = FALSE
    /\ taken = FALSE
    /\ queued = FALSE
    /\ rung = FALSE

\* The host takes the handoff: it journals the wake, then removes the file.
Drain ==
    /\ file
    /\ ~taken
    /\ journaled' = TRUE
    /\ taken' = TRUE
    /\ queued' = TRUE
    /\ file' = FALSE
    /\ UNCHANGED rung

\* The pane is at a prompt: the queued line is typed once.
Ring ==
    /\ queued
    /\ rung' = TRUE
    /\ queued' = FALSE
    /\ UNCHANGED <<file, journaled, taken>>

\* A dispatcher restart. The journal is durable; the queue is not. A correct
\* boot refolds every journaled wake that never rang. "memory-only" does not.
Restart ==
    /\ queued' = IF Bug = "memory-only" THEN FALSE ELSE (journaled /\ ~rung)
    /\ UNCHANGED <<file, journaled, taken, rung>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Drain
    \/ Ring
    \/ Restart
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ file \in BOOLEAN
    /\ journaled \in BOOLEAN
    /\ taken \in BOOLEAN
    /\ queued \in BOOLEAN
    /\ rung \in BOOLEAN

\* A journaled wake that has not rung is always still queued (or re-derived
\* into the queue at boot). The memory-only restart breaks exactly this.
NoLostWake == (journaled /\ ~rung) => queued

====
