---- MODULE DeferredQueue ----
\* A pin's wake queue, as a set of message identities (bd-0uzrq.8/.x3pqw/.1/.13).
\* Rust correspondence (flywheel):
\*   one entry per wake class, merge journaled  crates/doorbell-core  queue_line
\*   the same message from two producers       crates/doorbell-core  wake_key / Queued::Duplicate
\*   one ring naming every class it carries    crates/doorbell-core  composed_ring
\*   newest line of a class wins (supersede)   crates/doorbell-core  queue_line
\*   a held prompt is queued, never dropped    src/workspace.rs  hold_filtered
\*   preempt only on act-now                   crates/doorbell-core  is_act_now_line
\*   a preempt is recorded as rung             crates/doorbell-core  route_or_queue
\* Boundary: the pane transport. This model is the QUEUE: which messages are
\* pending, which have rung, which were journaled instead of rung.
EXTENDS Naturals, TLC

CONSTANTS Messages, ActNow, Bug
\* Messages \subseteq {"brief", "peer"}: two wakes of different classes.
\* ActNow \subseteq Messages: the messages allowed to preempt a WORKING pane
\* (importance urgent / captain steer). A peer request is never in it.
\* Bug \in {"none", "ring-twice", "strand", "preempt-any", "preempt-unrecorded"}:
\*   "none"                the protocol as specified
\*   "ring-twice"          the ring does not consume the queue, so a message rung
\*                         once can ring again (AtMostOnceRung fails)
\*   "strand"              an arrival is acked as audited without ever being
\*                         queued, so it is neither rung nor journaled
\*                         (EveryWakeAccounted fails)
\*   "preempt-any"         any arrival preempts the working pane, so a peer
\*                         request is typed where the harness swallows it
\*                         (OnlyActNowPreempts fails)
\*   "preempt-unrecorded"  a preempt does not enter rung, so the second
\*                         producer's copy queues it and the drain rings the
\*                         same message again (AtMostOnceRung fails)

VARIABLES arrived, queued, rung, audited, working, preempted, rings

vars == <<arrived, queued, rung, audited, working, preempted, rings>>

Init ==
    /\ arrived = {}
    /\ queued = {}
    /\ rung = {}
    /\ audited = {}
    /\ working = TRUE          \* the wake lands while the pin is mid-turn
    /\ preempted = {}
    /\ rings = [m \in Messages |-> 0]

\* A producer delivers one message and the routing decision is made AT ARRIVAL
\* (`route_or_queue`): a working pane queues it, unless the line is act-now, in
\* which case it preempts — and a preempt records itself in rung.
Arrive(m) ==
    /\ m \in Messages
    /\ m \notin arrived
    /\ arrived' = arrived \cup {m}
    /\ IF Bug = "strand"
       THEN UNCHANGED <<queued, rung, audited, working, preempted, rings>>
       ELSE IF working /\ (m \in ActNow \/ Bug = "preempt-any")
            THEN /\ preempted' = preempted \cup {m}
                 /\ rings' = [rings EXCEPT ![m] = @ + 1]
                 /\ rung' = IF Bug = "preempt-unrecorded"
                           THEN rung ELSE rung \cup {m}
                 /\ UNCHANGED <<queued, audited, working>>
            ELSE /\ queued' = queued \cup {m}
                 /\ UNCHANGED <<rung, audited, working, preempted, rings>>

\* The IDLE EDGE re-reads the inbox, so an arrival can repeat. A repeat is a
\* step that changes nothing observable: no second entry, no second ring.
RereadByTheIdleEdge(m) ==
    /\ m \in arrived
    /\ UNCHANGED vars

\* A second producer carries the SAME message (the CLI's own refused copy).
\* A message already rung is refused HERE — that refusal is what keeps
\* AtMostOnceRung true across both producers.
Resend(m) ==
    /\ m \in arrived
    /\ m \notin rung
    /\ m \notin queued
    /\ queued' = queued \cup {m}
    /\ UNCHANGED <<arrived, rung, audited, working, preempted, rings>>

\* The pane reaches a prompt and stays there: the turn is over.
ReachPrompt ==
    /\ working
    /\ working' = FALSE
    /\ UNCHANGED <<arrived, queued, rung, audited, preempted, rings>>

\* ONE ring drains the queue, naming every class. "ring-twice" leaves the queue
\* in place, which is the double ring.
Ring ==
    /\ ~working
    /\ queued # {}
    /\ rung' = rung \cup queued
    /\ rings' = [m \in Messages |->
                    IF m \in queued THEN rings[m] + 1 ELSE rings[m]]
    /\ queued' = IF Bug = "ring-twice" THEN queued ELSE {}
    /\ UNCHANGED <<arrived, audited, working, preempted>>

\* A wake that must not ring (a machine notice, a superseded brief) is journaled
\* instead: visible, never rung.
Audit(m) ==
    /\ m \in arrived
    /\ m \notin rung
    /\ audited' = audited \cup {m}
    /\ UNCHANGED <<arrived, queued, rung, working, preempted, rings>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ \E m \in Messages : Arrive(m)
    \/ \E m \in Messages : RereadByTheIdleEdge(m)
    \/ \E m \in Messages : Resend(m)
    \/ ReachPrompt
    \/ Ring
    \/ \E m \in Messages : Audit(m)
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(ReachPrompt) /\ WF_vars(Ring)

TypeOK ==
    /\ arrived \subseteq Messages
    /\ queued \subseteq Messages
    /\ rung \subseteq Messages
    /\ audited \subseteq Messages
    /\ working \in BOOLEAN
    /\ preempted \subseteq Messages
    /\ rings \in [Messages -> Nat]

\* bd-x3pqw/.13: each message is rung AT MOST ONCE, by either producer — the
\* preempt arm and the drain share the count, so a preempt that forgets to
\* record itself is caught by the queue accepting the second copy.
AtMostOnceRung == \A m \in Messages : rings[m] <= 1

\* bd-0uzrq.13: nothing but an explicit act-now is ever typed into a WORKING
\* pane. A peer request waits for the prompt.
OnlyActNowPreempts == preempted \subseteq ActNow

\* bd-0uzrq.8/.1: nothing vanishes. Every arrived message is queued, rung or
\* journaled — a captain brief behind a peer wake is exactly the case this
\* forbids. "Rung or audited" is AccountedEventually, below: pending is a legal
\* state, being absent is not.
EveryWakeAccounted == arrived \subseteq (rung \cup audited \cup queued)

\* Liveness: on a fair run, every message that arrived is eventually rung or
\* journaled. TLC needs Messages finite for this.
AccountedEventually == <>(\A m \in arrived : m \in rung \/ m \in audited)

====
