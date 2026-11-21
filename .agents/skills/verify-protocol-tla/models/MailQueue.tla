---- MODULE MailQueue ----
\* One send across a toron daemon outage (bd-ys3jp).
\* Rust correspondence:
\*   durable queue, corr dedupe    toron-core/src/queued_send.rs
\*   queue stamp + outage status   toron-cli/src/main.rs (run_robot)
\*   flush under same corr         toron-cli/src/robot.rs (flush_queued_sends)
\*   pure lifecycle                flywheel/src/elm/mail_queue_phasechart.rs
\* Boundary: the transport. The model is about ONE accepted send surviving an
\* outage: it is journaled before the attempt, retried under the SAME corr,
\* and delivered at most once.
EXTENDS Naturals, TLC

CONSTANTS Bug, Corr
\* Bug \in {"none", "double", "memory-only", "recorr"}:
\*   "none"         the protocol as specified;
\*   "double"       a retry delivers the same mail twice (ExactlyOnce);
\*   "memory-only"  a restart forgets the queue (NoLostSend);
\*   "recorr"       a retry mints a fresh corr (SameCorr).

VARIABLES phase, queued, delivered, corr, attempts

vars == <<phase, queued, delivered, corr, attempts>>

Init ==
    /\ phase = "idle"
    /\ queued = FALSE
    /\ delivered = 0
    /\ corr = Corr
    /\ attempts = 0

Send ==
    /\ phase = "idle"
    /\ phase' = "queued"
    /\ queued' = TRUE
    /\ UNCHANGED <<delivered, corr, attempts>>

Down ==
    /\ phase \in {"queued", "flushing"}
    /\ phase' = "queued"
    /\ queued' = TRUE
    /\ UNCHANGED <<delivered, corr, attempts>>

Up ==
    /\ phase = "queued"
    /\ attempts < 2
    /\ phase' = "flushing"
    /\ attempts' = attempts + 1
    /\ UNCHANGED <<queued, delivered, corr>>

Flush ==
    /\ phase = "flushing"
    /\ phase' = "delivered"
    /\ delivered' = delivered + 1
    /\ queued' = FALSE
    /\ corr' = corr
    /\ UNCHANGED attempts

\* Negative: a queued row is dropped without delivery.
Lose ==
    /\ Bug = "memory-only"
    /\ queued
    /\ queued' = FALSE
    /\ delivered' = delivered
    /\ UNCHANGED <<phase, corr, attempts>>

\* Negative: a retry re-mints the corr.
Recorr ==
    /\ Bug = "recorr"
    /\ phase = "flushing"
    /\ corr' = "OTHER"
    /\ UNCHANGED <<phase, queued, delivered, attempts>>

\* Negative: the same send is delivered a second time.
Double ==
    /\ Bug = "double"
    /\ phase = "delivered"
    /\ delivered' = delivered + 1
    /\ UNCHANGED <<phase, queued, corr, attempts>>

Next ==
    \/ Send
    \/ Down
    \/ Up
    \/ Flush
    \/ Double
    \/ Lose
    \/ Recorr

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"idle", "queued", "flushing", "delivered"}
    /\ queued \in BOOLEAN
    /\ delivered \in 0..2
    /\ corr \in {Corr, "OTHER"}
    /\ attempts \in 0..2

ExactlyOnce == delivered <= 1

NoLostSend == queued \/ delivered > 0 \/ phase = "idle"

SameCorr == corr = Corr

====
