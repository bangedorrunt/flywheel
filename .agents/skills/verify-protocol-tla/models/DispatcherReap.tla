---- MODULE DispatcherReap ----
\* One project, several dispatcher doors: which ones may a status sweep reap?
\* (bd-bj9xk, model bd-zutvt)
\* Rust correspondence (flywheel):
\*   plane gate     src/cmd/doorbell.rs  reap_gone_with  (rex-primary only,
\*                  never a fallback list, never an empty list, own session
\*                  present in the list)
\*   door decision  src/cmd/doorbell.rs  dispatcher_gone
\* Boundary: the stop itself (a failed stop keeps the record). This model is
\* about the DECISION to call a door gone.
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Bug
\* Bug \in {"none", "fallback-reap", "reap-live-session", "label-dropped"}:
\* negative fixtures. "none" is the protocol as specified.

VARIABLES plane, shape, live, reaped

vars == <<plane, shape, live, reaped>>

Door(kind, id) == [kind |-> kind, id |-> id]

AllDoors ==
    {Door("label", "flywheel-p"), Door("session", "own"), Door("session", "gone")}

LabelOnly ==
    {Door("label", "flywheel-p")}

Doors == IF shape = "label-only" THEN LabelOnly ELSE AllDoors

Init ==
    /\ plane = "unresolved"
    /\ shape = "all"
    /\ live = {"own", "gone"}          \* the plane's live session ids
    /\ reaped = {}

\* The plane answers, and the project's door set is whatever it is.
Probe ==
    /\ plane = "unresolved"
    /\ plane' \in {"rex", "fallback", "hc"}
    /\ shape' \in {"all", "label-only"}
    /\ UNCHANGED <<live, reaped>>

LiveSessionDoors ==
    {d \in Doors : d.kind = "session" /\ d.id \in live}

\* A session door is gone when its id is not live. A label door is an alias
\* only while a live session door carries the project; a label that is the only
\* door is kept.
GoneDoors ==
    {d \in Doors :
        \/ /\ d.kind = "session"
           /\ (Bug = "reap-live-session" \/ d.id \notin live)
        \/ /\ d.kind = "label"
           /\ (Bug = "label-dropped" \/ LiveSessionDoors /= {})
    }

Reap ==
    /\ (plane = "rex" \/ Bug = "fallback-reap")
    /\ live /= {}
    /\ "own" \in live
    /\ reaped' = reaped \cup GoneDoors
    /\ UNCHANGED <<plane, shape, live>>

Idle ==
    /\ UNCHANGED vars

Next ==
    \/ Probe
    \/ Reap
    \/ Idle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ plane \in {"unresolved", "rex", "fallback", "hc"}
    /\ shape \in {"all", "label-only"}
    /\ live \subseteq {"own", "gone"}
    /\ reaped \subseteq AllDoors \cup LabelOnly

\* Only a rex-primary list may reap anything.
NeverReapOnFallback ==
    (plane /= "rex") => (reaped = {})

\* A live session door is never reaped.
NeverReapLiveSession ==
    \A d \in reaped : ~(d.kind = "session" /\ d.id \in live)

\* A label that is the only door (no live session alias) is kept.
LabelOnlyDoorKept ==
    (LiveSessionDoors = {}) => (\A d \in reaped : d.kind /= "label")

====
