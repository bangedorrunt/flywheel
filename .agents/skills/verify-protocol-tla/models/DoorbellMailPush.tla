---- MODULE DoorbellMailPush ----
\* Mail-delivered push and inbox poll are two producers of one wake
\* (bd-0uzrq.18). Rust correspondence (flywheel):
\*   watch push -> Prompt/audit           crates/doorbell-core  tick(WatchNdjson)
\*   inbox poll -> Prompt/audit           crates/doorbell-core  tick(InboxRows)
\*   rung dedup by message identity       crates/doorbell-core  route_or_queue
\* (a) the same message delivered by push and poll, in either order, rings once.
\* (b) a second message on the same thread is a distinct wake and must be
\*     accounted/rung, never swallowed by thread-keyed rung dedup.
EXTENDS Naturals, TLC

CONSTANTS Messages, Bug
\* Bug \in {"none", "rung-by-thread"}:
\*   "none"           rung identity is the message id
\*   "rung-by-thread" rung identity collapses to thread; a second same-thread
\*                    message is misread as already rung and dropped

SameThread(x, y) == TRUE

VARIABLES arrived, queued, rung, audited, idle, rings

vars == <<arrived, queued, rung, audited, idle, rings>>

Init ==
    /\ arrived = {}
    /\ queued = {}
    /\ rung = {}
    /\ audited = {}
    /\ idle = TRUE
    /\ rings = [m \in Messages |-> 0]

SeenByRung(m) ==
    IF Bug = "rung-by-thread"
    THEN \E x \in rung : SameThread(x, m)
    ELSE m \in rung

\* One delivery of message m (push or poll). The same message may be delivered
\* again by the other producer in either order.
Deliver(m) ==
    /\ m \in Messages
    /\ arrived' = arrived \cup {m}
    /\ IF SeenByRung(m)
       THEN IF Bug = "rung-by-thread" /\ m \notin rung
            THEN UNCHANGED <<queued, rung, audited, idle, rings>>
            ELSE /\ audited' = audited \cup {m}
                 /\ UNCHANGED <<queued, rung, idle, rings>>
       ELSE IF idle
            THEN /\ rung' = rung \cup {m}
                 /\ rings' = [rings EXCEPT ![m] = @ + 1]
                 /\ UNCHANGED <<queued, audited, idle>>
            ELSE /\ queued' = queued \cup {m}
                 /\ UNCHANGED <<rung, audited, idle, rings>>

BecomeBusy ==
    /\ idle
    /\ idle' = FALSE
    /\ UNCHANGED <<arrived, queued, rung, audited, rings>>

Drain ==
    /\ ~idle
    /\ idle' = TRUE
    /\ rung' = rung \cup queued
    /\ rings' = [m \in Messages |-> IF m \in queued THEN rings[m] + 1 ELSE rings[m]]
    /\ queued' = {}
    /\ UNCHANGED <<arrived, audited>>

Idle == /\ UNCHANGED vars

Next ==
    \/ \E m \in Messages : Deliver(m)
    \/ BecomeBusy
    \/ Drain
    \/ Idle

Spec == Init /\ [][Next]_vars /\ WF_vars(Drain)

TypeOK ==
    /\ arrived \subseteq Messages
    /\ queued \subseteq Messages
    /\ rung \subseteq Messages
    /\ audited \subseteq Messages
    /\ idle \in BOOLEAN
    /\ rings \in [Messages -> Nat]

AtMostOnceRung == \A m \in Messages : rings[m] <= 1
EveryWakeAccounted == arrived \subseteq (rung \cup audited \cup queued)

====
