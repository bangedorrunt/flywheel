---- MODULE AssignBrief ----
\* bd-vk0do: assigning a bead refuses when another pin holds it, and a
\* newer brief on the same bead withdraws the older one.
\*
\* Rust correspondence (flywheel):
\*   the assign gate (refuse vs --reassign plus withdrawal mail)
\*                            src/main.rs  cmd_assign
\*   the brief supersede and the live-only display
\*                            src/crew_briefs.rs  BriefBook
\*
\* Boundary: one bead, two pins, two brief ids. Leases, peer intents and
\* open briefs are one abstract "holders" set; the mail transport is out
\* of scope (a withdrawal counts as notified the moment it is sent).

EXTENDS Naturals, TLC, FiniteSets

CONSTANTS Bug
\* Bug \in {"none", "double-claim", "no-withdraw", "silent-reassign"}:
\* each non-"none" value removes exactly one fence.

Pins == {"p1", "p2"}
Briefs == {"b1", "b2"}

VARIABLES holders, live, withdrawnPins, notified

vars == <<holders, live, withdrawnPins, notified>>

Init ==
    /\ holders = {}
    /\ live = {}
    /\ withdrawnPins = {}
    /\ notified = {}

\* Grant the bead. A held bead refuses a plain assign (no state change);
\* --reassign replaces the holder and notifies whoever it replaces.
Assign(p, reassign) ==
    /\ p \in Pins
    /\ \/ holders = {}
       \/ reassign
    /\ holders' = {p}
    /\ IF reassign /\ holders /= {}
       THEN /\ withdrawnPins' = withdrawnPins \cup holders
            /\ notified' = IF Bug = "silent-reassign" THEN notified
                           ELSE notified \cup holders
       ELSE UNCHANGED <<withdrawnPins, notified>>
    /\ UNCHANGED live

\* Negative fixture only: a plain assign steals a held bead, so two pins
\* hold it at once.
Steal(p) ==
    /\ Bug = "double-claim"
    /\ p \in Pins
    /\ holders /= {}
    /\ p \notin holders
    /\ holders' = holders \cup {p}
    /\ UNCHANGED <<live, withdrawnPins, notified>>

\* Open a brief for the bead. A second live brief withdraws the first and
\* notifies its pin; the negative fixture leaves both live.
Brief(b) ==
    /\ b \in Briefs
    /\ holders /= {}
    /\ b \notin live
    /\ IF live /= {}
       THEN IF Bug = "no-withdraw"
            THEN /\ live' = live \cup {b}
                 /\ UNCHANGED <<withdrawnPins, notified>>
            ELSE /\ live' = {b}
                 /\ withdrawnPins' = withdrawnPins \cup holders
                 /\ notified' = notified \cup holders
       ELSE /\ live' = {b}
            /\ UNCHANGED <<withdrawnPins, notified>>
    /\ UNCHANGED holders

\* Close a brief. Closing is not withdrawing: no notice is owed.
Done(b) ==
    /\ b \in live
    /\ live' = live \ {b}
    /\ UNCHANGED <<holders, withdrawnPins, notified>>

Next ==
    \/ \E p \in Pins : \E r \in BOOLEAN : Assign(p, r)
    \/ \E p \in Pins : Steal(p)
    \/ \E b \in Briefs : Brief(b)
    \/ \E b \in Briefs : Done(b)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ holders \subseteq Pins
    /\ live \subseteq Briefs
    /\ withdrawnPins \subseteq Pins
    /\ notified \subseteq Pins

\* The bead never has two holders at once.
OneClaimant == Cardinality(holders) <= 1

\* The bead never shows two live briefs at once.
OneLiveBrief == Cardinality(live) <= 1

\* Every withdrawn pin was notified.
WithdrawalDelivered == withdrawnPins \subseteq notified

====
