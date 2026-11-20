---- MODULE EventsDedupe ----
\* One ring per mail across wake.sock + toron events (ADR-0039 D8, bd-l5vb2).
\* Rust correspondence (flywheel):
\*   one shared dedupe key (bus event id)   crates/doorbell-core  rung
\*   wake.sock rings, journals the id       src/doorbell_host.rs  wake_outcome_to_brain
\*   events hint rings through the same key crates/doorbell-core  DoorbellMsg::EventsHint
\*   gap becomes ONE digest, never a prompt crates/doorbell-core  DoorbellMsg::EventsGap
\*   single budget gate, many sources       crates/doorbell-rt    Brain::deliver/gated
\* Boundary: the pin's prompt sink. Two producers, one ring per mail.
EXTENDS Naturals, TLC

CONSTANTS Mails, Bug
\* Bug \in {"none", "ring-twice"}:
\*   "none"        the events source checks the shared dedupe set before ringing
\*   "ring-twice"  it ignores the shared set, so a mail reached by both rings twice

VARIABLES arrived, seen, rung, rings, digests

vars == <<arrived, seen, rung, rings, digests>>

Init ==
    /\ arrived = {}
    /\ seen = {}
    /\ rung = {}
    /\ rings = [m \in Mails |-> 0]
    /\ digests = {}

\* The wake.sock path: a mail arrives and rings once; the bus event id is
\* journaled in the shared dedupe set.
WakeSock(m) ==
    /\ m \in Mails
    /\ m \notin arrived
    /\ m \notin digests
    /\ arrived' = arrived \cup {m}
    /\ seen' = seen \cup {m}
    /\ rung' = rung \cup {m}
    /\ rings' = [rings EXCEPT ![m] = @ + 1]
    /\ UNCHANGED digests

\* The toron events path for the same mail. If the id is already in the shared
\* set, the good protocol emits nothing; the broken variant rings it again.
EventsHint(m) ==
    /\ m \in Mails
    /\ m \notin digests
    /\ arrived' = arrived \cup {m}
    /\ IF m \in seen /\ Bug /= "ring-twice"
       THEN UNCHANGED <<seen, rung, rings, digests>>
       ELSE /\ seen' = seen \cup {m}
            /\ rung' = rung \cup {m}
            /\ rings' = [rings EXCEPT ![m] = @ + 1]
            /\ UNCHANGED digests

\* A gap is one digest, not a prompt, and never enters the rung set.
EventsGap(m) ==
    /\ m \in Mails
    /\ digests' = digests \cup {m}
    /\ UNCHANGED <<arrived, seen, rung, rings>>

Next == \E m \in Mails : WakeSock(m) \/ EventsHint(m) \/ EventsGap(m)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ arrived \subseteq Mails
    /\ seen \subseteq Mails
    /\ rung \subseteq Mails
    /\ digests \subseteq Mails

\* The invariant: no mail is rung more than once even when both sources see it.
AtMostOnceRing == \A m \in Mails : rings[m] <= 1

\* A gap never produces a prompt.
GapIsDigest == digests \cap rung = {}

====
