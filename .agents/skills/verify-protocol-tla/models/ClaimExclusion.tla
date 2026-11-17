---- MODULE ClaimExclusion ----
\* A pin that did the work may not claim its review (bd-6setu.6; model bd-6setu.19).
\*
\* Rust correspondence (flywheel + toron):
\*   the review job carries its step's reads as claim exclusions
\*                            src/workflow_lowering.rs  stamp_claim_exclusions
\*   the claim refuses a pin that claimed one of those nodes
\*                            toron  crates/toron-core  job claim guard
\*   the binder's own author rule (V10)  src/seat_binder.rs  is_author
\*
\* Boundary: one node, one run. Lease/fencing is JobLease.tla's; this model is
\* only the authorship rule, which is the guard against self-review.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "allow-author"}: negative fixture. "none" refuses the claim
\* when the pin already claimed the node it would review.

Pins == {"p1", "p2"}

VARIABLES authors, claimant

vars == <<authors, claimant>>

Init ==
    /\ authors = {}
    /\ claimant = "none"

\* The work node is claimed by whoever does it. One claimer per node.
ClaimWork(p) ==
    /\ p \notin authors
    /\ claimant = "none"
    /\ authors' = authors \cup {p}
    /\ UNCHANGED claimant

\* The review job may not go to a pin that claimed the work it would review.
ClaimReview(p) ==
    /\ claimant = "none"
    /\ (Bug = "allow-author" \/ p \notin authors)
    /\ claimant' = p
    /\ UNCHANGED authors

Next ==
    \/ \E p \in Pins : ClaimWork(p)
    \/ \E p \in Pins : ClaimReview(p)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ authors \subseteq Pins
    /\ claimant \in Pins \cup {"none"}

\* bd-6setu.6: the review never lands on an author of the work.
NoSelfReview == claimant = "none" \/ claimant \notin authors

====
