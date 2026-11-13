---- MODULE Respawn ----
\* One pin, two panes: the reconciler respawn protocol (ADR-0022 V11).
\* Rust correspondence (flywheel):
\*   taken-name refusal + clear  src/main.rs  start_agent_with_taken_name_clear
\*   respawn order               src/main.rs  envpin_hop_respawn (split -> start
\*                               -> close old) and src/seat_reconcile.rs
\* Boundary: the pin's own liveness and reservations. Backoff, retries and
\* pane transport are out of scope.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Panes, Pin, Bug
\* Bug \in {"none", "claim-before-free", "drop-reservations"}: negative
\* fixtures. "none" is the protocol as specified; each other value removes
\* exactly one rule so the matching invariant must fail.

VARIABLES alive, holder, reservations, claimed, dropped

vars == <<alive, holder, reservations, claimed, dropped>>

Init ==
    /\ alive = {"old"}
    /\ holder = [p \in Panes |-> IF p = "old" THEN Pin ELSE "none"]
    /\ reservations = {Pin}
    /\ claimed = FALSE
    /\ dropped = FALSE

HoldsLivePin(p) == p \in alive /\ holder[p] = Pin

SpawnNew ==
    /\ "new" \notin alive
    /\ alive' = alive \cup {"new"}
    /\ UNCHANGED <<holder, reservations, claimed, dropped>>

\* The retired pane's binding is released before the replacement claims the
\* pin: a name another live pane holds is refused, not shared.
FreeOld ==
    /\ "old" \in alive
    /\ holder["old"] = Pin
    /\ holder' = [holder EXCEPT !["old"] = "none"]
    /\ UNCHANGED <<alive, reservations, claimed, dropped>>

ClaimNew ==
    /\ "new" \in alive
    /\ holder["new"] = "none"
    /\ (Bug = "claim-before-free" \/ \A p \in alive : holder[p] /= Pin)
    /\ holder' = [holder EXCEPT !["new"] = Pin]
    /\ claimed' = TRUE
    /\ UNCHANGED <<alive, reservations, dropped>>

\* The old pane is only closed once it no longer holds the pin, and the
\* pin's reservations survive the respawn.
CloseOld ==
    /\ "old" \in alive
    /\ holder["old"] /= Pin
    /\ alive' = alive \ {"old"}
    /\ dropped' = (dropped \/ (Bug = "drop-reservations"))
    /\ reservations' = IF Bug = "drop-reservations" THEN {} ELSE reservations
    /\ UNCHANGED <<holder, claimed>>

Next ==
    \/ SpawnNew
    \/ FreeOld
    \/ ClaimNew
    \/ CloseOld

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ alive \subseteq Panes
    /\ \A p \in Panes : holder[p] \in {Pin, "none"}
    /\ reservations \subseteq {Pin}
    /\ claimed \in BOOLEAN
    /\ dropped \in BOOLEAN

\* V11 (a): at most one live pane holds the pin at any time.
OneLiveHolder == Cardinality({p \in Panes : HoldsLivePin(p)}) <= 1

\* V11 (b): a respawn never drops the pin's reservations.
ReservationsKept == dropped = FALSE

====
