---- MODULE BuildBreak ----
\* Build-break alert (bd-6xu6r). One broken file mails its holder once; a
\* repeat of the same break emits nothing, and the caller is never mailed
\* about its own file. Rust correspondence: src/cmd/build_break_statechart.rs
\* (pure reducer) and src/cmd/blocked_by.rs (journal + mail effect).
\*
\* Bug \in {"none", "double-alert", "self-alert"}: negative fixtures.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Pins, Keys, Caller, Bug

VARIABLES alerted, mailed, counts

vars == <<alerted, mailed, counts>>

TypeOK ==
    /\ alerted \subseteq Keys
    /\ \forall m \in mailed : m[1] \in Keys /\ m[2] \in Pins
    /\ \forall k \in Keys : counts[k] \in Nat

\* One alert per break key.
OneAlertPerBreak ==
    \forall k \in Keys : counts[k] <= 1

\* The caller is never the recipient of an alert.
NeverSelfAlert ==
    \forall m \in mailed : m[2] /= Caller

Init ==
    /\ alerted = {}
    /\ mailed = {}
    /\ counts = [k \in Keys |-> 0]

Broken(k, h) ==
    LET fresh == k \notin alerted
        foreign == h /= Caller
        send == CASE Bug = "double-alert" -> foreign
                 [] Bug = "self-alert"    -> fresh
                 [] OTHER                 -> fresh /\ foreign
    IN /\ alerted' = alerted \cup {k}
       /\ IF send THEN /\ mailed' = mailed \cup {<<k, h>>}
                        /\ counts' = [counts EXCEPT ![k] = @ + 1]
                  ELSE /\ mailed' = mailed
                        /\ counts' = counts

Next == \E k \in Keys, h \in Pins : Broken(k, h)

Spec == Init /\ [][Next]_vars
====
