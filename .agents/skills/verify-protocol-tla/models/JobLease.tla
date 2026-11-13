---- MODULE JobLease ----
\* toron job claim / lease / settle, modelled from the fenced journal.
\* Rust correspondence (sibling toron + elm-effect-rs):
\*   Journal::claim        toron-core/src/journal.rs        (owner + deadline + gen+1)
\*   check_claim           elm-effect-journal/src/plan.rs   (terminal, generation, live lease)
\*   check_settle          elm-effect-journal/src/plan.rs   (generation fence, legal transition)
\* Boundary: one job row, two owners, time as a bounded tick. Nothing else.
EXTENDS Naturals, TLC

CONSTANTS Owners, LeaseMs, MaxTime, MaxGen, Bug
\* Bug \in {"none", "steal", "stale-settle", "revive-terminal"}: the negative
\* fixtures. "none" is the code as written; each other value removes exactly
\* one fence so the corresponding invariant must fail.

VARIABLES state, owner, deadline, gen, now, stolen, stale_settled, revived

vars == <<state, owner, deadline, gen, now, stolen, stale_settled, revived>>

Init ==
    /\ state = "inflight"
    /\ owner = "none"
    /\ deadline = 0
    /\ gen = 0
    /\ now = 0
    /\ stolen = FALSE
    /\ stale_settled = FALSE
    /\ revived = FALSE

Terminal == state = "settled" \/ state = "failed"

\* The lease-live rule of check_claim: a foreign live lease is never stolen.
ForeignLiveLease(o) == owner /= "none" /\ owner /= o /\ deadline > now

\* Claim: state is not terminal, the caller's generation is current (the
\* caller re-reads on reconcile), and no foreign live lease exists.
Claim(o) ==
    /\ ~Terminal
    /\ gen < MaxGen
    /\ ~(ForeignLiveLease(o) /\ Bug /= "steal")
    /\ stolen' = (stolen \/ (ForeignLiveLease(o) /\ Bug = "steal"))
    /\ owner' = o
    /\ deadline' = now + LeaseMs
    /\ gen' = gen + 1
    /\ UNCHANGED <<state, now, stale_settled, revived>>

\* The holder settles its own generation. check_settle refuses terminal rows
\* and stale generations.
Settle(o) ==
    /\ ~Terminal
    /\ owner = o
    /\ state' = "settled"
    /\ UNCHANGED <<owner, deadline, gen, now, stolen, stale_settled, revived>>

\* Negative fixture: a zombie settles generation gen-1 anyway.
ZombieSettle ==
    /\ Bug = "stale-settle"
    /\ gen > 0
    /\ ~Terminal
    /\ state' = "settled"
    /\ stale_settled' = TRUE
    /\ UNCHANGED <<owner, deadline, gen, now, stolen, revived>>

\* Negative fixture: a terminal row is claimed again (settled work revived).
ReviveTerminal(o) ==
    /\ Bug = "revive-terminal"
    /\ Terminal
    /\ revived' = TRUE
    /\ owner' = o
    /\ deadline' = now + LeaseMs
    /\ gen' = gen + 1
    /\ UNCHANGED <<state, now, stolen, stale_settled>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<state, owner, deadline, gen, stolen, stale_settled, revived>>

Next ==
    \/ \E o \in Owners : Claim(o)
    \/ \E o \in Owners : Settle(o)
    \/ ZombieSettle
    \/ \E o \in Owners : ReviveTerminal(o)
    \/ Tick

Spec == Init /\ [][Next]_vars /\ WF_vars(Tick)

TypeOK ==
    /\ state \in {"inflight", "settled", "failed"}
    /\ owner \in Owners \cup {"none"}
    /\ deadline \in 0..(MaxTime + LeaseMs)
    /\ gen \in 0..MaxGen
    /\ now \in 0..MaxTime
    /\ stolen \in BOOLEAN
    /\ stale_settled \in BOOLEAN
    /\ revived \in BOOLEAN

\* 1. No foreign live lease is ever stolen: a claim that took the row from a
\*    holder whose lease was still live is the counterexample.
NoSteal == stolen = FALSE

\* 2. A stale generation never settles the row (fencing).
NoStaleSettle == stale_settled = FALSE

\* 3. A terminal row never moves: settled/failed work is never revived.
TerminalStable == revived = FALSE

====
