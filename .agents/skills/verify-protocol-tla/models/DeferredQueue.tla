---- MODULE DeferredQueue ----
\* A pin's wake queue, as a set of message identities (bd-0uzrq.8/.x3pqw/.1).
\* Rust correspondence (flywheel):
\*   one entry per wake class, merge journaled  crates/doorbell-core  queue_line
\*   the same message from two producers       crates/doorbell-core  wake_key / Queued::Duplicate
\*   one ring naming every class it carries    crates/doorbell-core  composed_ring
\*   newest line of a class wins (supersede)   crates/doorbell-core  queue_line
\*   a held prompt is queued, never dropped    src/workspace.rs  hold_filtered
\* Boundary: the pane transport. This model is the QUEUE: which messages are
\* pending, which have rung, which were journaled instead of rung.
EXTENDS Naturals, TLC

CONSTANTS Messages, Bug
\* Messages \subseteq {"brief", "peer"}: two wakes of different classes.
\* Bug \in {"none", "ring-twice", "strand"}:
\*   "none"         the protocol as specified
\*   "ring-twice"   the ring does not consume the queue, so a message rung once
\*                  can ring again (AtMostOnceRung fails)
\*   "strand"       an arrival is acked as audited without ever being queued, so
\*                  it is neither rung nor journaled (EveryWakeAccounted fails)

VARIABLES arrived, queued, rung, audited

vars == <<arrived, queued, rung, audited>>

Init ==
    /\ arrived = {}
    /\ queued = {}
    /\ rung = {}
    /\ audited = {}

\* A producer delivers one message. Distinct messages land in the queue once
\* each (a set: the same identity from two producers is one entry).
Arrive(m) ==
    /\ m \in Messages
    /\ m \notin arrived
    /\ IF Bug = "strand"
       THEN /\ arrived' = arrived \cup {m}
            /\ UNCHANGED <<queued, rung, audited>>
       ELSE /\ arrived' = arrived \cup {m}
            /\ queued' = queued \cup {m}
            /\ UNCHANGED <<rung, audited>>

\* The same message arrives again through a second producer: no second entry,
\* no second ring. Modelled as a step that changes nothing observable.
Redeliver(m) ==
    /\ m \in arrived
    /\ UNCHANGED vars

\* The pane reaches a prompt: ONE ring drains the queue, naming every class.
\* "ring-twice" leaves the queue in place, which is the double ring.
Ring ==
    /\ queued # {}
    /\ rung' = rung \cup queued
    /\ queued' = IF Bug = "ring-twice" THEN queued ELSE {}
    /\ UNCHANGED <<arrived, audited>>

\* A wake that must not ring (a machine notice, a superseded brief) is journaled
\* instead: visible, never rung.
Audit(m) ==
    /\ m \in arrived
    /\ m \notin rung
    /\ audited' = audited \cup {m}
    /\ UNCHANGED <<arrived, queued, rung>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ \E m \in Messages : Arrive(m)
    \/ \E m \in Messages : Redeliver(m)
    \/ Ring
    \/ \E m \in Messages : Audit(m)
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(Ring)

TypeOK ==
    /\ arrived \subseteq Messages
    /\ queued \subseteq Messages
    /\ rung \subseteq Messages
    /\ audited \subseteq Messages

\* bd-x3pqw: each message is rung AT MOST ONCE. A message that has rung is not
\* in the queue — the ring consumed it, and no producer can put it back.
AtMostOnceRung == \A m \in rung : m \notin queued

\* bd-0uzrq.8/.1: nothing vanishes. Every arrived message is queued, rung or
\* journaled — a captain brief behind a peer wake is exactly the case this
\* forbids. "Rung or audited" is AccountedEventually, below: pending is a legal
\* state, being absent is not.
EveryWakeAccounted == arrived \subseteq (rung \cup audited \cup queued)

\* Liveness: on a fair run, every message that arrived is eventually rung or
\* journaled. TLC needs Messages finite for this.
AccountedEventually == <>(\A m \in arrived : m \in rung \/ m \in audited)

====
