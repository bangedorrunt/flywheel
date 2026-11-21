---- MODULE ResultMail ----
\* Result-mail fan-out (bd-n21ml).
\* Rust correspondence:
\*   proof red row / owner dedupe   src/cmd/clean_proof.rs (mail_red_owners)
\*   landing / assignee dedupe      src/cmd/ship.rs (mail_landed_to_assignee)
\*   pure lifecycle                 flywheel/src/elm/result_mail_statechart.rs
\* Boundary: the mail transport. One red row and one landing each notify their
\* owner once, even when the same event is replayed; a green proof sends no
\* failure mail.
EXTENDS Naturals, TLC

CONSTANTS Bug, Owners
\* Bug \in {"none", "dup-row", "replay-land", "green-mail"}

VARIABLES rowMails, landMails, rowOwner, landOwner, green, done

vars == <<rowMails, landMails, rowOwner, landOwner, green, done>>

Init ==
    /\ rowMails = 0
    /\ landMails = 0
    /\ rowOwner = ""
    /\ landOwner = ""
    /\ green = FALSE
    /\ done = FALSE

RedRow(o) ==
    /\ ~done
    /\ o \in Owners
    /\ rowMails = 0
    /\ rowMails' = 1
    /\ rowOwner' = o
    /\ UNCHANGED <<landMails, landOwner, green, done>>

Land(a) ==
    /\ ~done
    /\ a \in Owners
    /\ landMails = 0
    /\ landMails' = 1
    /\ landOwner' = a
    /\ UNCHANGED <<rowMails, rowOwner, green, done>>

\* A replay of an already-notified landing is a no-op.
Replay ==
    /\ landMails = 1
    /\ UNCHANGED vars

ProofEnd == UNCHANGED vars

Green ==
    /\ ~done
    /\ rowMails = 0
    /\ landMails = 0
    /\ green' = TRUE
    /\ done' = TRUE
    /\ UNCHANGED <<rowMails, landMails, rowOwner, landOwner>>

\* Negative: an already-notified row is mailed a second time.
DupRow(o) ==
    /\ ~done
    /\ Bug = "dup-row"
    /\ o \in Owners
    /\ rowMails >= 1
    /\ rowMails' = rowMails + 1
    /\ rowOwner' = o
    /\ UNCHANGED <<landMails, landOwner, green, done>>

\* Negative: replaying a landing mails its assignee a second time.
ReplayLand(a) ==
    /\ Bug = "replay-land"
    /\ landMails = 1
    /\ a \in Owners
    /\ landMails' = 2
    /\ landOwner' = a
    /\ UNCHANGED <<rowMails, rowOwner, green, done>>

\* Negative: a green proof emits a failure mail.
GreenMail(o) ==
    /\ ~done
    /\ Bug = "green-mail"
    /\ o \in Owners
    /\ green' = TRUE
    /\ done' = TRUE
    /\ rowMails' = 1
    /\ rowOwner' = o
    /\ UNCHANGED <<landMails, landOwner>>

Next ==
    \/ \E o \in Owners: RedRow(o) \/ DupRow(o) \/ GreenMail(o)
    \/ \E a \in Owners: Land(a) \/ ReplayLand(a)
    \/ Replay
    \/ ProofEnd
    \/ Green

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ rowMails \in 0..2
    /\ landMails \in 0..2
    /\ rowOwner \in Owners \cup {""}
    /\ landOwner \in Owners \cup {""}
    /\ green \in BOOLEAN
    /\ done \in BOOLEAN

OwnerNotifiedOnce == rowMails <= 1

LandNotifiedOnce == landMails <= 1

NoMailOnGreen == green => rowMails = 0

====
