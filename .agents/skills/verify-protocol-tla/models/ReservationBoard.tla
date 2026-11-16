---- MODULE ReservationBoard ----
\* The reservation board read (bd-0uzrq.12). Two acquires contend for ONE
\* exclusive path in the same tick; each decides from the board it read.
\*
\* Measured before: the board read returned Ok(0) after 10.0s while a peer held
\* the path, so the acquire built an EMPTY board and granted. "Nothing is held"
\* and "I could not read what is held" must never be the same answer to the
\* question a reservation asks.
\*
\* Rust correspondence (toron @51e27bde):
\*   strict board read    collect_replay_to_eose   (a truncated history errors)
\*   refusal, not grant   try_kernel_model_for     (acquire/conflict refuse)
\*   per-coordinate lock  acquire_coordinate_lock  (the decide sees the board)
\* Tests: toron-core a_replay_that_cannot_read_refuses_the_acquire, and
\* same_tick_double_acquire_grants_exactly_one (the lock's own fence).
\*
\* Boundary: the bus transport and the fencing epochs. This model is about what
\* a decision is allowed to conclude from the view it holds.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Pins, Bug
\* Bug \in {"none", "truncated-grants", "no-refresh"}: negative fixtures.
\*   truncated-grants  a truncated read is treated as an empty board
\*   no-refresh        a stale complete view grants without re-checking the board

VARIABLES held, views, read, decided

vars == <<held, views, read, decided>>

View == [status : {"complete", "truncated"}, seen : SUBSET Pins]

Init ==
    /\ held = {}
    /\ views = [p \in Pins |-> [status |-> "truncated", seen |-> {}]]
    /\ read = {}
    /\ decided = {}

\* The board comes back one of two ways: the whole history, or a truncated one.
Read(p) ==
    /\ p \notin read
    /\ \/ views' = [views EXCEPT ![p] = [status |-> "complete", seen |-> held]]
       \/ views' = [views EXCEPT ![p] = [status |-> "truncated", seen |-> {}]]
    /\ read' = read \cup {p}
    /\ UNCHANGED <<held, decided>>

\* May this view grant the path to p?
\* The decision sees the CURRENT board (`held = {}`): that is the per-coordinate
\* lock, so a same-tick pair cannot both grant from a view taken before the other.
Grant(v, p) ==
    CASE Bug = "truncated-grants" -> v.seen = {}
      [] Bug = "no-refresh"       -> v.status = "complete" /\ v.seen = {}
      [] OTHER                    -> v.status = "complete" /\ v.seen = {} /\ held = {}

Decide(p) ==
    /\ p \in read
    /\ p \notin decided
    /\ LET v == views[p] IN
         IF Grant(v, p) THEN held' = held \cup {p} ELSE held' = held
    /\ decided' = decided \cup {p}
    /\ UNCHANGED <<views, read>>

Stutter ==
    /\ UNCHANGED vars

Next ==
    \/ \E p \in Pins : Read(p)
    \/ \E p \in Pins : Decide(p)
    \/ Stutter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ held \subseteq Pins
    /\ views \in [Pins -> View]
    /\ read \subseteq Pins
    /\ decided \subseteq Pins

\* The board is the one thing a reservation exists to answer: a grant is only
\* ever made from a COMPLETE read that saw the path free.
NoGrantWithoutCompleteBoard ==
    \A p \in held : views[p].status = "complete" /\ views[p].seen = {}

\* However many acquire in one tick, one path has one holder.
AtMostOneHolder ==
    Cardinality(held) <= 1

====
