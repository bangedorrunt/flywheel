---- MODULE JournalAppend ----
\* Two writers, one doorbell journal (bd-bj9xk-adjacent, model bd-zutvt).
\* Rust correspondence (flywheel):
\*   one locked write    crates/doorbell-rt/src/store.rs  append_journal
\*                       (line built whole, written under an exclusive
\*                       cross-process lock; rotation shares it)
\* Boundary: rotation itself is out of scope; this is the row integrity.
EXTENDS Naturals, Sequences, TLC

CONSTANTS Bug, MaxRows
\* Bug \in {"none", "interleave"}: negative fixture. "none" is the protocol as
\* specified; "interleave" lets a second writer start while the first line is
\* still open, which leaves a torn row forever.

VARIABLES lines, pending

vars == <<lines, pending>>

\* A line is a record: state is "open" (payload written, newline pending) or
\* "closed" (whole row on disk).
Init ==
    /\ lines = <<>>
    /\ pending = FALSE

\* A writer starts a row. With the defect, a writer may start while another
\* row is still open.
Begin ==
    /\ Len(lines) < MaxRows
    /\ (Bug = "interleave" \/ ~pending)
    /\ pending' = TRUE
    /\ lines' = Append(lines, [state |-> "open", who |-> IF pending THEN "w2" ELSE "w1"])

\* The same writer finishes its row: only the LAST line can be closed, so a row
\* that was overtaken stays open forever.
End ==
    /\ pending
    /\ pending' = FALSE
    /\ lines' = Append(SubSeq(lines, 1, Len(lines) - 1),
                       [state |-> "closed", who |-> (lines[Len(lines)]).who])

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Begin
    \/ End
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in BOOLEAN
    /\ \A i \in 1..Len(lines) : (lines[i]).state \in {"open", "closed"}

\* At most the LAST row may be open; every earlier row is whole. A writer that
\* starts while another row is open leaves that row torn forever.
NoTornRow ==
    \A i \in 1..Len(lines) : i < Len(lines) => (lines[i]).state = "closed"

====
