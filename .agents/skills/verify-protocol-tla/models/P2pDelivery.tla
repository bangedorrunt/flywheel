---- MODULE P2pDelivery ----
\* One peer directive (request-release / release-response / review-request)
\* aimed at one busy target (bd-ncgyf).
\* Rust correspondence (flywheel):
\*   p2p verbs mail + wake        src/cmd/peer.rs  cmd_peer / deliver_p2p_wake_*
\*   busy refusal -> the ONE handoff
\*                                src/cmd/peer.rs  defer_busy_p2p_wake
\*   p2p mail classifies as a wake
\*                                crates/doorbell-core/src/lib.rs  parse_inbox_item
\*   boot refold of unrung rows   src/doorbell.rs  unrung_deferrals
\* Boundary: the pane transport and the reducer's queue coalescing. The defect
\* this model pins: a busy pane printed "wake deferred" and DROPPED the wake, so
\* the peer never heard the directive.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "drop-on-busy"}: negative fixture. "none" is the protocol
\* as specified; "drop-on-busy" makes a busy refusal drop the directive instead
\* of journaling it, so EveryDirectiveReachesTargetOrIsJournaledPending fails.

VARIABLES sent, busy, refused, journaled, queued, rung, dropped

vars == <<sent, busy, refused, journaled, queued, rung, dropped>>

Init ==
    /\ sent = FALSE
    /\ busy = TRUE           \* the target is mid-turn when the directive lands
    /\ journaled = FALSE
    /\ queued = FALSE
    /\ rung = FALSE
    /\ refused = FALSE
    /\ dropped = FALSE

Send ==
    /\ ~sent
    /\ sent' = TRUE
    /\ UNCHANGED <<busy, refused, journaled, queued, rung, dropped>>

\* The target refuses: a correct sender journals the directive into the one
\* handoff (and the host queues it); the defect drops it.
Refuse ==
    /\ sent
    /\ busy
    /\ ~journaled
    /\ ~dropped
    /\ refused' = TRUE
    /\ dropped' = (Bug = "drop-on-busy")
    /\ journaled' = (Bug /= "drop-on-busy")
    /\ queued' = (Bug /= "drop-on-busy")
    /\ UNCHANGED <<sent, busy, rung>>

\* The target reaches a prompt: the queued directive is typed once.
Ring ==
    /\ queued
    /\ rung' = TRUE
    /\ queued' = FALSE
    /\ UNCHANGED <<sent, busy, refused, journaled, dropped>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Send
    \/ Refuse
    \/ Ring
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ sent \in BOOLEAN
    /\ busy \in BOOLEAN
    /\ journaled \in BOOLEAN
    /\ queued \in BOOLEAN
    /\ rung \in BOOLEAN
    /\ refused \in BOOLEAN
    /\ dropped \in BOOLEAN

\* Every directive either reached the target or is journaled/pending: the
\* busy-drop defect breaks exactly this.
EveryDirectiveReachesTargetOrIsJournaledPending ==
    refused => (rung \/ journaled \/ queued)

====
