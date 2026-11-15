---- MODULE SupervisorRetire ----
\* Retiring one installed supervisor unit (model bd-zutvt, bd-rilvn era).
\* Rust correspondence (flywheel):
\*   retire verb   src/cmd/swarm.rs  retire_with / SeatEffect::Retire
\*   bootout       src/doorbell_supervise.rs  (launchd bootout outcome)
\* Boundary: leases and the journal are out of scope; this is the plist.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "delete-on-failure"}: negative fixture. "none" is the
\* protocol as specified; the defect deletes the plist even when the bootout
\* failed, so the unit can never be bootstrapped again.

VARIABLES unit_loaded, plist, bootout_ran, bootout_ok

vars == <<unit_loaded, plist, bootout_ran, bootout_ok>>

Init ==
    /\ unit_loaded = TRUE
    /\ plist = TRUE
    /\ bootout_ran = FALSE
    /\ bootout_ok = FALSE

\* The unit is booted out of the service manager. A successful bootout unloads
\* it (the manager owns the loaded flag); the plist is not touched here.
Bootout ==
    /\ ~bootout_ran
    /\ bootout_ran' = TRUE
    /\ bootout_ok' \in {TRUE, FALSE}
    /\ unit_loaded' = ~bootout_ok'
    /\ UNCHANGED plist

\* The plist is deleted only once the unit is really gone. The defect deletes it
\* on any outcome.
DeletePlist ==
    /\ plist
    /\ (bootout_ok \/ Bug = "delete-on-failure")
    /\ plist' = FALSE
    /\ UNCHANGED <<unit_loaded, bootout_ran, bootout_ok>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Bootout
    \/ DeletePlist
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ unit_loaded \in BOOLEAN
    /\ plist \in BOOLEAN
    /\ bootout_ran \in BOOLEAN
    /\ bootout_ok \in BOOLEAN

\* The plist is only gone once the unit is gone: deleting it after a failed
\* bootout strands a loaded unit with no way to bootstrap it again.
PlistOnlyWhenGone ==
    plist = FALSE => unit_loaded = FALSE

====
