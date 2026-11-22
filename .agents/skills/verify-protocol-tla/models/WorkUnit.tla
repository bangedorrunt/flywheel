---- MODULE WorkUnit ----
\* bd-ihiqo.3: one bead-optional work unit closes exactly one way, never
\* without a green verify, and a bead is created only on handoff.
\*
\* Rust correspondence (flywheel):
\*   the work-unit fold (target + verify, close routing)
\*                            src/elm/work_unit_statechart.rs
\*   the bead/ad-hoc close verbs (ship land vs commit + journal)
\*                            (bead close path: src/cmd/ship.rs; ad-hoc close: commit)
\*
\* Boundary: one unit, one verify flag, one handoff. Lease fencing,
\* journal settlement and the commit CAS are other models' jobs; this one
\* is only the close discipline: which way, on what evidence, and when a
\* bead may be minted.

EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "close-twice", "close-anyway", "bead-always"}: each
\* non-"none" value removes exactly one fence.

VARIABLES kind, verified, handedOff, beadCreated, closed, closeCount

vars == <<kind, verified, handedOff, beadCreated, closed, closeCount>>

Init ==
    /\ kind = "none"
    /\ verified = FALSE
    /\ handedOff = FALSE
    /\ beadCreated = FALSE
    /\ closed = ""
    /\ closeCount = 0

BindBead ==
    /\ kind = "none"
    /\ kind' = "bead"
    /\ UNCHANGED <<verified, handedOff, beadCreated, closed, closeCount>>

BindAdHoc ==
    /\ kind = "none"
    /\ kind' = "adhoc"
    /\ beadCreated' = IF Bug = "bead-always" THEN TRUE ELSE beadCreated
    /\ UNCHANGED <<verified, handedOff, closed, closeCount>>

Verify ==
    /\ verified' = TRUE
    /\ UNCHANGED <<kind, handedOff, beadCreated, closed, closeCount>>

Handoff ==
    /\ handedOff' = TRUE
    /\ beadCreated' = IF kind = "adhoc" THEN TRUE ELSE beadCreated
    /\ UNCHANGED <<kind, verified, closed, closeCount>>

CloseBead ==
    /\ kind = "bead"
    /\ (closed = "" \/ Bug = "close-twice")
    /\ verified \/ Bug = "close-anyway"
    /\ closed' = "ship"
    /\ closeCount' = IF Bug = "close-twice" THEN closeCount + 1 ELSE 1
    /\ UNCHANGED <<kind, verified, handedOff, beadCreated>>

CloseAdHoc ==
    /\ kind = "adhoc"
    /\ (closed = "" \/ Bug = "close-twice")
    /\ verified \/ Bug = "close-anyway"
    /\ closed' = "commit"
    /\ closeCount' = IF Bug = "close-twice" THEN closeCount + 1 ELSE 1
    /\ UNCHANGED <<kind, verified, handedOff, beadCreated>>

\* The bounds are the end of the model, not a defect.
Idle ==
    /\ UNCHANGED vars

Next ==
    \/ BindBead
    \/ BindAdHoc
    \/ Verify
    \/ Handoff
    \/ CloseBead
    \/ CloseAdHoc
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ kind \in {"none", "bead", "adhoc"}
    /\ verified \in BOOLEAN
    /\ handedOff \in BOOLEAN
    /\ beadCreated \in BOOLEAN
    /\ closed \in {"", "ship", "commit"}
    /\ closeCount \in 0..3

\* Each unit closes exactly one way.
OneCloseRoute == closeCount <= 1

\* No close lands without a green verify behind it.
NoCloseWithoutGreenVerify == closed = "" \/ verified

\* A bead is minted only when the unit changes hands.
BeadOnlyOnHandoff == ~beadCreated \/ handedOff

====
