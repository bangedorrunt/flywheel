---- MODULE PeerContention ----
\* Two peers, one path, one tick (bd-0uzrq.7). The contention half of peer
\* delivery (P2pDelivery models the directive reaching a busy target).
\* Rust correspondence (flywheel):
\*   advisory arbitration   src/judgment.rs  arbitrate_p2p_conflict
\*   deterministic winner   the lowest pin wins a same-tick claim, and the
\*                          loser is told who won
\*   lease auto-release     toron's lease expiry releases a dead holder
\* Boundary: the transport and the reservation ledger's fencing epochs. This
\* model is about WHO holds the path and whether the requester is ever told.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Bug, Pins
\* Bug \in {"none", "no-tiebreak"}: negative fixture. "no-tiebreak" grants the
\* path to every same-tick claimant, so AtMostOneHolder fails.

VARIABLES holders, claims, req, told, asked

vars == <<holders, claims, req, told, asked>>

Init ==
    /\ holders = {}
    /\ claims = {}
    /\ req = "A"            \* the lower pin asks; ties go to the lower pin
    /\ told = FALSE
    /\ asked = FALSE

Claim(p) ==
    /\ p \in Pins
    /\ p \notin claims
    /\ claims' = claims \cup {p}
    /\ asked' = TRUE
    /\ UNCHANGED <<holders, req, told>>

\* The tick settles: one claimant wins (the lowest pin). With the defect the
\* path is granted to every claimant. The loser is told who won.
Settle ==
    /\ claims /= {}
    /\ holders = {}
    /\ holders' =
        IF Bug = "no-tiebreak" /\ Cardinality(claims) > 1
        THEN claims
        ELSE (IF "A" \in claims THEN {"A"} ELSE claims)
    /\ claims' = {}
    /\ told' = (told \/ (Cardinality(claims) > 1 /\ "A" /= req))
    /\ UNCHANGED <<req, asked>>

\* The holder's pane dies: the lease auto-releases, and the requester is told
\* the path is free again.
HolderDies ==
    /\ holders /= {}
    /\ holders' = {}
    /\ told' = TRUE
    /\ UNCHANGED <<claims, req, asked>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ \E p \in Pins : Claim(p)
    \/ Settle
    \/ HolderDies
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ holders \subseteq Pins
    /\ claims \subseteq Pins
    /\ req \in Pins
    /\ told \in BOOLEAN
    /\ asked \in BOOLEAN

\* At most one peer holds the path, however many claimed it in one tick.
AtMostOneHolder ==
    Cardinality(holders) <= 1

\* Once the claims are settled, either somebody holds the path or the requester
\* has been told: nobody is left waiting forever.
EveryRequesterSettled ==
    (asked /\ claims = {}) => (holders /= {} \/ told)

====
