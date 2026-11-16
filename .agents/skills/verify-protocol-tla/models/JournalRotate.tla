---- MODULE JournalRotate ----
\* One journal, two writers and a rotator (bd-ifqqx).
\* Rust correspondence (flywheel):
\*   append    crates/doorbell-rt/src/store.rs  append_journal_seq (whole line,
\*             one write, under the journal flock)
\*   rotate    store.rs  rotate (rewrites the active file; same flock)
\* Boundary: this is the lock protocol between an append and a rotation, not
\* the row's byte-level integrity (that is JournalAppend).
EXTENDS Naturals, Sequences, TLC

CONSTANTS Bug, MaxRows
\* Bug \in {"none", "unlocked"}: negative fixture. "none" is the protocol as
\* specified; "unlocked" lets a rotation start while a writer is mid-append,
\* so the write lands in the renamed inode and the row is lost.

VARIABLES active, gen, pending, rotating, lost

vars == <<active, gen, pending, rotating, lost>>

Init ==
    /\ active = <<>>
    /\ gen = <<>>
    /\ pending = FALSE
    /\ rotating = FALSE
    /\ lost = FALSE

\* A writer starts one append. Only one append is in flight at a time.
AppendStart ==
    /\ ~pending
    /\ ~rotating
    /\ pending' = TRUE
    /\ UNCHANGED <<active, gen, rotating, lost>>

\* The writer finishes. With the defect a rotation may have moved the file
\* underneath it: the row is gone.
AppendFinish(n) ==
    /\ pending
    /\ Len(active) + Len(gen) < MaxRows
    /\ pending' = FALSE
    /\ IF rotating
       THEN /\ lost' = TRUE
            /\ UNCHANGED <<active, gen>>
       ELSE /\ active' = Append(active, n)
            /\ UNCHANGED <<gen, lost>>
    /\ UNCHANGED rotating

\* Rotation takes the lock. Under the defect it takes it anyway.
RotateStart ==
    /\ ~rotating
    /\ (Bug = "unlocked" \/ ~pending)
    /\ rotating' = TRUE
    /\ UNCHANGED <<active, gen, pending, lost>>

\* Rotation rewrites: the active rows move to a generation, active empties.
RotateFinish ==
    /\ rotating
    /\ rotating' = FALSE
    /\ gen' = gen \o active
    /\ active' = <<>>
    /\ UNCHANGED <<pending, lost>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ AppendStart
    \/ \E n \in 1..MaxRows : AppendFinish(n)
    \/ RotateStart
    \/ RotateFinish
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in BOOLEAN
    /\ rotating \in BOOLEAN
    /\ lost \in BOOLEAN

\* The property the lock buys: no append is lost across a rotation.
NoLostRow ==
    lost = FALSE

====
