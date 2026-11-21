---- MODULE LeaseWaitlist ----
\* bd-u1n91: a blocked acquire queues and a freed file hands over on its own.
\*
\* Rust correspondence (toron + flywheel):
\*   reserve acquire --queue    enqueue on hard conflict + one typed request-release
\*   kernel_release_lease       hand the path to the head waiter after ANY release
\*   flywheel commit (default) release committed leases; --keep-lease skips it
\*
\* Boundary: one path, two pins, at most two queued waiters. The bus, TTL
\* clock and mail transport are abstracted: a release names its source, and a
\* request-release counts as delivered when it is sent.

EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Bug
\* Bug \in {"none", "double-holder", "lifo", "drop-waiter", "no-ttl"}:
\* each non-"none" value removes exactly one fence.

Pins == {"A", "B", "C"}
Sources == {"commit", "ship", "release", "ttl", "retire"}
MaxQ == 2
Idx == 1..MaxQ

VARIABLES holder, arr, arrLen, srv, srvLen, arrived, notified, badRelease

vars == <<holder, arr, arrLen, srv, srvLen, arrived, notified, badRelease>>

RangeArr == {arr[i] : i \in 1..arrLen}
RangeSrv == {srv[i] : i \in 1..srvLen}
Queued == RangeArr \ RangeSrv

Init ==
    /\ holder = {}
    /\ arr = [i \in Idx |-> "none"]
    /\ arrLen = 0
    /\ srv = [i \in Idx |-> "none"]
    /\ srvLen = 0
    /\ arrived = {}
    /\ notified = {}
    /\ badRelease = {}

\* Queue(p): a hard conflict with one typed request-release.
Queue(p) ==
    /\ p \in Pins
    /\ holder /= {}
    /\ p \notin holder
    /\ p \notin RangeArr
    /\ arrLen < MaxQ
    /\ arr' = [arr EXCEPT ![arrLen + 1] = p]
    /\ arrLen' = arrLen + 1
    /\ arrived' = arrived \cup {p}
    /\ notified' = notified \cup {p}
    /\ UNCHANGED <<holder, srv, srvLen, badRelease>>

\* Release(src): every source funnels through the one handoff helper.
Release(src) ==
    /\ src \in Sources
    /\ holder /= {}
    /\ LET head == IF arrLen > srvLen THEN arr[srvLen + 1] ELSE "none"
           tail == IF Bug = "lifo" /\ arrLen > srvLen THEN arr[arrLen] ELSE head
           skipped == Bug = "no-ttl" /\ src = "ttl"
           grant == IF skipped \/ tail = "none" THEN {} ELSE {tail}
       IN /\ holder' = CASE Bug = "double-holder" -> holder \cup grant
                       [] OTHER -> grant
          /\ IF Bug = "drop-waiter"
             THEN /\ arrLen' = srvLen
                  /\ UNCHANGED <<srv, srvLen>>
             ELSE IF grant /= {}
                  THEN /\ srv' = [srv EXCEPT ![srvLen + 1] = tail]
                       /\ srvLen' = srvLen + 1
                       /\ arrLen' = arrLen
                  ELSE /\ UNCHANGED <<arrLen, srv, srvLen>>
          /\ badRelease' = IF arrLen > srvLen /\ grant = {}
                           THEN badRelease \cup {src}
                           ELSE badRelease
    /\ UNCHANGED <<arr, arrived, notified>>

\* Acquiring a free path needs no queue.
AcquireFree(p) ==
    /\ p \in Pins
    /\ holder = {}
    /\ holder' = {p}
    /\ UNCHANGED <<arr, arrLen, srv, srvLen, arrived, notified, badRelease>>

Next == \/ \E p \in Pins : Queue(p)
        \/ \E p \in Pins : AcquireFree(p)
        \/ \E s \in Sources : Release(s)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ holder \subseteq Pins
    /\ arr \in [Idx -> Pins \cup {"none"}]
    /\ arrLen \in 0..MaxQ
    /\ srv \in [Idx -> Pins \cup {"none"}]
    /\ srvLen \in 0..arrLen
    /\ arrived \subseteq Pins
    /\ notified \subseteq Pins
    /\ badRelease \subseteq Sources

\* At most one exclusive holder per path.
OneHolder == Cardinality(holder) <= 1

\* Waiters are served in arrival order: served is a prefix of arrivals.
FifoHandoff == \A i \in 1..srvLen : srv[i] = arr[i]

\* Every waiter holds the path, is still queued, or was served.
NoLostWaiter == arrived \subseteq Queued \cup holder \cup RangeSrv

\* A release with waiters always hands over, from every source.
HandoffOnEveryRelease == badRelease = {}

====
