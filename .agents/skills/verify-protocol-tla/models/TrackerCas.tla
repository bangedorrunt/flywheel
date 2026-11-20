---- MODULE TrackerCas ----
\* N landers CAS-update-ref HEAD with their own closed tracker row (bd-kz3sc
\* rework 2). This is the private-index CAS in src/cmd/commit.rs (bd-aberm), also
\* used by ship land's tracker commit.
\*
\* Rust correspondence (flywheel):
\*   read HEAD tree + splice this bead's rows  src/cmd/ship.rs  tracker_blobs_for_bead
\*   private-index fence + CAS retry loop      src/cmd/commit.rs  commit_tree / commit_blobs
\*   update-ref HEAD <new> <parent>             src/cmd/commit.rs  commit_tree
\*
\* Boundary: the journal/gate ceremony, the JSON row encoding, pane transport,
\* and the exact git plumbing are out of scope. Only the read-splice-CAS-retry
\* loop is modelled: another lander moving HEAD between our tree build and our
\* update-ref must not lose its row when we retry.
EXTENDS Naturals, TLC

CONSTANTS Bug, MaxRetries
\* Bug \in {"none", "stale-splice"}. "none" re-reads HEAD and re-splices on every
\* CAS retry; "stale-splice" reuses the bytes spliced at the FIRST read (the
\* regression: retry writes the pre-race file over the moved parent).

Landers == {"l1", "l2"}
Rows == {"r1", "r2"}
own(a) == IF a = "l1" THEN "r1" ELSE "r2"
Phases == {"init", "ready", "retry", "done", "exhausted"}

VARIABLES headRows, ver, phase, readVer, spliceRows, attempts, done

vars == <<headRows, ver, phase, readVer, spliceRows, attempts, done>>

Init ==
    /\ headRows = {}
    /\ ver = 0
    /\ phase = [a \in Landers |-> "init"]
    /\ readVer = [a \in Landers |-> 0]
    /\ spliceRows = [a \in Landers |-> {}]
    /\ attempts = [a \in Landers |-> 0]
    /\ done = [a \in Landers |-> FALSE]

\* Read HEAD's tracker tree, splice in this lander's closed row, and remember
\* the version for the CAS. On a retry the correct protocol re-reads the MOVED
\* head; "stale-splice" keeps the first splice.
Refresh(a) ==
    /\ phase[a] \in {"init", "retry"}
    /\ readVer' = [readVer EXCEPT ![a] = ver]
    /\ spliceRows' = [spliceRows EXCEPT ![a] =
          IF Bug = "stale-splice" /\ phase[a] = "retry" THEN spliceRows[a]
          ELSE headRows \cup {own(a)}]
    /\ phase' = [phase EXCEPT ![a] = "ready"]
    /\ UNCHANGED <<headRows, ver, attempts, done>>

\* CAS succeeds: HEAD was still the version we read.
CasOk(a) ==
    /\ phase[a] = "ready"
    /\ ver = readVer[a]
    /\ attempts[a] <= MaxRetries
    /\ headRows' = spliceRows[a]
    /\ ver' = ver + 1
    /\ done' = [done EXCEPT ![a] = TRUE]
    /\ phase' = [phase EXCEPT ![a] = "done"]
    /\ UNCHANGED <<readVer, spliceRows, attempts>>

\* CAS fails: another lander moved HEAD. Retry after a re-read (bounded).
CasFail(a) ==
    /\ phase[a] = "ready"
    /\ ver /= readVer[a]
    /\ attempts[a] < MaxRetries
    /\ attempts' = [attempts EXCEPT ![a] = attempts[a] + 1]
    /\ phase' = [phase EXCEPT ![a] = "retry"]
    /\ UNCHANGED <<headRows, ver, readVer, spliceRows, done>>

\* Retry budget spent: the lander returns an error, not ok.
Exhaust(a) ==
    /\ phase[a] = "ready"
    /\ ver /= readVer[a]
    /\ attempts[a] = MaxRetries
    /\ phase' = [phase EXCEPT ![a] = "exhausted"]
    /\ UNCHANGED <<headRows, ver, readVer, spliceRows, attempts, done>>

Idle == UNCHANGED vars

Step(a) == Refresh(a) \/ CasOk(a) \/ CasFail(a) \/ Exhaust(a)
Next == (\E a \in Landers : Step(a)) \/ Idle

Spec == Init /\ [][Next]_vars /\ \A a \in Landers : WF_vars(Step(a))

TypeOK ==
    /\ headRows \subseteq Rows
    /\ ver \in Nat
    /\ \A a \in Landers :
         /\ phase[a] \in Phases
         /\ readVer[a] \in Nat
         /\ spliceRows[a] \subseteq Rows
         /\ attempts[a] \in 0..MaxRetries
         /\ done[a] \in BOOLEAN

\* A lander that returned ok has its closed row in the final HEAD. The stale
\* splice overwrites the moved parent and loses the other lander's row.
NoClosedRowLost == \A a \in Landers : done[a] => own(a) \in headRows

\* Retries are bounded, so the protocol terminates rather than spinning.
BoundedRetries == \A a \in Landers : attempts[a] <= MaxRetries

Termination == <>( \A a \in Landers : phase[a] \in {"done", "exhausted"} )

====
