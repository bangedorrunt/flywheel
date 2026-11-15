---- MODULE DeferredQueueLanes ----
\* bd-0uzrq.17: DeferredQueue with TWO DELIVERY LANES.
\*
\* The embedded brain reads one channel today; the .17 fix adds a status-only lane
\* polled `biased` first, so a status can overtake a mail that is still sitting in
\* the other lane. DeferredQueue.tla already interleaves arrival and the idle edge
\* freely, so the INVARIANTS there are order-independent; this model makes the two
\* lanes explicit, so the ordering is a modelled choice instead of an accident, and
\* pins the one order that would actually be wrong: a drain that clears mail it has
\* not seen.
\*
\* Rust correspondence (flywheel):
\*   the mail lane                    crates/doorbell-rt  Brain::run(rx)
\*   the status lane, biased first    crates/doorbell-rt  Brain::run(rx, status_rx)
\*   the idle drain                   crates/doorbell-core queue drain
\*   queued, rung or journaled        crates/doorbell-core route_or_queue
EXTENDS Naturals, Sequences, TLC

CONSTANTS Messages, ActNow, Bug, LaneCap
\* Messages \subseteq {"brief"}: wakes a producer puts on the mail lane.
\* ActNow \subseteq Messages: what may preempt a WORKING pane.
\* Bug \in {"none", "drain-unreceived"}:
\*   "none"              the idle edge drains only what the fold queued
\*   "drain-unreceived"  the idle edge clears the MAIL LANE too, so mail it has
\*                       not received is neither rung nor journaled
\*                       (EveryWakeAccounted fails)

VARIABLES arrived, queued, rung, audited, preempted, working, rings, mail, status

vars == <<arrived, queued, rung, audited, preempted, working, rings, mail, status>>

Init ==
    /\ arrived = {}
    /\ queued = {}
    /\ rung = {}
    /\ audited = {}
    /\ preempted = {}
    /\ working = TRUE
    /\ rings = [m \in Messages |-> 0]
    /\ mail = <<>>
    /\ status = <<>>

TypeOK ==
    /\ arrived \subseteq Messages
    /\ queued \subseteq Messages
    /\ rung \subseteq Messages
    /\ audited \subseteq Messages
    /\ preempted \subseteq Messages
    /\ working \in BOOLEAN
    /\ rings \in [Messages -> Nat]
    /\ mail \in Seq(Messages)
    /\ status \in Seq({"working", "idle"})

Lane == {mail[i] : i \in 1..Len(mail)}

\* A producer puts a wake on the mail lane. Delivery is a separate step, so the
\* lane may be behind (bd-0uzrq.17's live backlog) without being wrong.
Send(m) ==
    /\ m \in Messages
    /\ m \notin arrived
    /\ Len(mail) < LaneCap
    /\ arrived' = arrived \cup {m}
    /\ mail' = Append(mail, m)
    /\ UNCHANGED <<queued, rung, audited, preempted, working, rings, status>>

\* The plane poll puts a status word on the status lane (the .17 producer).
SendStatus(s) ==
    /\ s \in {"working", "idle"}
    /\ Len(status) < LaneCap
    /\ status' = Append(status, s)
    /\ UNCHANGED <<arrived, queued, rung, audited, preempted, working, rings, mail>>

\* The mail lane delivers its head: a working pane queues it unless act-now.
DeliverMail ==
    /\ Len(mail) > 0
    /\ LET m == Head(mail) IN
       /\ mail' = Tail(mail)
       /\ IF working /\ m \in ActNow
          THEN /\ preempted' = preempted \cup {m}
               /\ rung' = rung \cup {m}
               /\ rings' = [rings EXCEPT ![m] = @ + 1]
               /\ UNCHANGED <<arrived, queued, audited, working, status>>
          ELSE /\ queued' = queued \cup {m}
               /\ UNCHANGED <<arrived, rung, audited, preempted, working, rings, status>>

\* The status lane delivers a word. It is polled first, and nothing here forbids
\* it firing while `mail` is non-empty: that is the overtake under test.
DeliverStatus ==
    /\ Len(status) > 0
    /\ LET s == Head(status) IN
       /\ status' = Tail(status)
       /\ IF s = "idle"
          THEN /\ working' = FALSE
               /\ rung' = rung \cup queued
               /\ rings' = [m \in Messages |->
                               IF m \in queued THEN rings[m] + 1 ELSE rings[m]]
               /\ queued' = {}
               /\ mail' = IF Bug = "drain-unreceived" THEN <<>> ELSE mail
               /\ UNCHANGED <<arrived, audited, preempted>>
          ELSE /\ working' = TRUE
               /\ UNCHANGED <<arrived, queued, rung, audited, preempted, rings, mail>>

\* The pane reports a prompt itself (the .10/.14 idle edge): same drain.
ReachPrompt ==
    /\ working
    /\ working' = FALSE
    /\ rung' = rung \cup queued
    /\ rings' = [m \in Messages |->
                    IF m \in queued THEN rings[m] + 1 ELSE rings[m]]
    /\ queued' = {}
    /\ UNCHANGED <<arrived, audited, preempted, mail, status>>

\* A wake that must not ring (a machine notice, a superseded brief).
Audit(m) ==
    /\ m \in arrived
    /\ m \notin rung
    /\ audited' = audited \cup {m}
    /\ UNCHANGED <<arrived, queued, rung, preempted, working, rings, mail, status>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ \E m \in Messages : Send(m)
    \/ \E s \in {"working", "idle"} : SendStatus(s)
    \/ DeliverMail
    \/ DeliverStatus
    \/ ReachPrompt
    \/ \E m \in Messages : Audit(m)
    \/ Idle

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(DeliverMail)
    /\ WF_vars(DeliverStatus)
    /\ WF_vars(ReachPrompt)

\* bd-x3pqw/.13: each message rings AT MOST ONCE, whatever the lane order.
AtMostOnceRung == \A m \in Messages : rings[m] <= 1

\* bd-0uzrq.13: only act-now is ever typed into a WORKING pane.
OnlyActNowPreempts == preempted \subseteq ActNow

\* bd-0uzrq.8/.1: nothing vanishes — mail still in its lane is accounted for.
EveryWakeAccounted == arrived \subseteq (rung \cup audited \cup queued \cup Lane)

\* Liveness: on a fair run every arrived message is eventually rung or journaled.
AccountedEventually == <>(\A m \in arrived : m \in rung \/ m \in audited)

====
