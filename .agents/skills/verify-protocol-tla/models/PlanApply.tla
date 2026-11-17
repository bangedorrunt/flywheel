---- MODULE PlanApply ----
\* Two appliers, one plan record (ADR-0022 D1, bd-6setu.3/.15; model bd-6setu.19).
\*
\* Rust correspondence (flywheel):
\*   read the record      src/state.rs  load_plan
\*   fold in memory       src/cmd/swarm.rs  fold_plan (+ pending events)
\*   append the batch     src/state.rs  append_plan_events (ONE write, after
\*                        every gate, right before the effects)
\*
\* Boundary: the seat effects are out of scope; so is the fold's arithmetic. The
\* claim being modelled is that a batch append cannot destroy a batch another
\* applier landed while this one was folding - the read-modify-write window is
\* real (two operators, one project) and the only guard today is the atomic
\* write, which is an implementation property, not a stated protocol.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "clobber"}: negative fixture. "none" appends a batch to the
\* record it finds (union). "clobber" writes the batch over the snapshot it
\* folded from, losing whatever landed in between.

Appliers == {"a", "b"}

own(a) == IF a = "a" THEN "ea" ELSE "eb"

VARIABLES record, snapshot, read_done, appended

vars == <<record, snapshot, read_done, appended>>

Init ==
    /\ record = {}
    /\ snapshot = [a \in Appliers |-> {}]
    /\ read_done = [a \in Appliers |-> FALSE]
    /\ appended = [a \in Appliers |-> FALSE]

\* Read: take the record as it stands, to fold the batch from.
Read(a) ==
    /\ read_done[a] = FALSE
    /\ snapshot' = [snapshot EXCEPT ![a] = record]
    /\ read_done' = [read_done EXCEPT ![a] = TRUE]
    /\ UNCHANGED <<record, appended>>

\* Append: ONE atomic batch write, after the gates.
Append(a) ==
    /\ read_done[a] = TRUE
    /\ appended[a] = FALSE
    /\ record' = IF Bug = "clobber" THEN snapshot[a] \cup {own(a)}
                 ELSE record \cup {own(a)}
    /\ appended' = [appended EXCEPT ![a] = TRUE]
    /\ UNCHANGED <<snapshot, read_done>>

Next ==
    \/ \E a \in Appliers : Read(a)
    \/ \E a \in Appliers : Append(a)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ record \subseteq {"ea", "eb"}
    /\ \A a \in Appliers : snapshot[a] \subseteq {"ea", "eb"}
    /\ \A a \in Appliers : read_done[a] \in BOOLEAN
    /\ \A a \in Appliers : appended[a] \in BOOLEAN

\* D1: once every applier has landed its batch, the record holds every batch.
\* A clobbering append loses one, which is exactly the failure this models.
EveryAppendLands ==
    (\A a \in Appliers : appended[a]) => record = {"ea", "eb"}

====
