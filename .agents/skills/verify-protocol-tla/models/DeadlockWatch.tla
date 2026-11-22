---- MODULE DeadlockWatch ----
\* bd-v4dcv: the deadlock watch pages the captain once per wait-for cycle
\* and once per overdue collab wait.
\*
\* Rust correspondence (flywheel):
\*   the wait-for graph (lease overlaps, collab waits)  src/deadlock.rs
\*   the once-only reported set                          src/cmd/deadlock.rs
\*   the watch message log                               src/elm/deadlock_watch_statechart.rs
\*
\* Boundary: a fixed fixture graph — one 2-cycle (A holds B's file and B
\* holds A's), one overdue collab wait (C waits on D), one young wait
\* (E waits on F, still under the threshold). Time is a bounded tick; the
\* threshold crossing is a fixed step, not a clock.
EXTENDS Naturals, TLC, FiniteSets, Sequences

CONSTANTS Bug
\* Bug \in {"none", "skip", "false", "double"}: each non-"none" value
\* removes exactly one fence.

\* The fixture graph: A<->B is the cycle, C->D is already overdue,
\* E->F is still young.
TrueFindings == {"cycle:A>B", "wait:C>D"}
ThresholdTick == 2
MaxTick == 3

VARIABLES alerted, alerts, missed, tick

vars == <<alerted, alerts, missed, tick>>

Init ==
    /\ alerted = {}
    /\ alerts = <<>>
    /\ missed = {}
    /\ tick = 0

\* One watch sweep: alert everything due that is not yet alerted, oldest
\* finding first so the alert order is deterministic. The skip bug files
\* the findings as missed instead.
Sweep ==
    /\ tick < MaxTick
    /\ tick' = tick + 1
    /\ LET due == TrueFindings \ alerted
           ordered == <<>>
           withCycle == IF "cycle:A>B" \in due THEN Append(ordered, "cycle:A>B") ELSE ordered
           withBoth == IF "wait:C>D" \in due THEN Append(withCycle, "wait:C>D") ELSE withCycle
       IN IF Bug = "skip"
          THEN /\ missed' = missed \cup due
               /\ UNCHANGED <<alerted, alerts>>
          ELSE /\ alerted' = alerted \cup due
               /\ alerts' = alerts \o withBoth
               /\ UNCHANGED missed

\* Negative fixture only: alert a cycle that does not exist.
FalseAlert ==
    /\ Bug = "false"
    /\ alerted' = alerted \cup {"cycle:X>Y"}
    /\ alerts' = Append(alerts, "cycle:X>Y")
    /\ UNCHANGED <<missed, tick>>

\* Negative fixture only: page the same finding twice.
DupAlert ==
    /\ Bug = "double"
    /\ alerted /= {}
    /\ alerts' = Append(alerts, "cycle:A>B")
    /\ UNCHANGED <<alerted, missed, tick>>

\* The bounds are the end of the model, not a defect.
Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Sweep
    \/ FalseAlert
    \/ DupAlert
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ alerted \subseteq (TrueFindings \cup {"cycle:X>Y"})
    /\ \A i \in 1..Len(alerts) : alerts[i] \in (TrueFindings \cup {"cycle:X>Y"})
    /\ missed \subseteq TrueFindings
    /\ tick \in 0..MaxTick

\* Every real cycle is alerted: a sweep that drops findings leaves them in
\* `missed` instead.
CycleAlerted == missed = {}

\* No page names a cycle (or wait) that is not a real finding.
NoFalseCycle == alerted \subseteq TrueFindings

\* No finding is paged twice.
AlertOnce == Len(alerts) = Cardinality({alerts[i] : i \in 1..Len(alerts)})

====
