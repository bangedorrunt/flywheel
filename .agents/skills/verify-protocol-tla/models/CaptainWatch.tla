---- MODULE CaptainWatch ----
\* Captain watch event fan-out (bd-7ky1v).
\* Rust correspondence:
\*   event classify / exit decisions   src/cmd/captain.rs (cmd_captain_watch)
\*   pure lifecycle                    flywheel/src/elm/captain_watch_statechart.rs
\* Boundary: the harness monitor. Stream mode emits one line per event and
\* never exits; the one-shot mode still exits on the first event.
EXTENDS Naturals, TLC, FiniteSets

CONSTANTS Bug, Events, Stream
\* Bug \in {"none", "drop", "exit-on-mail", "oneshot-emits"}

VARIABLES emitted, lines, exits, exitedOnMail

vars == <<emitted, lines, exits, exitedOnMail>>

Init ==
    /\ emitted = {}
    /\ lines = 0
    /\ exits = 0
    /\ exitedOnMail = FALSE

Mail(e) ==
    /\ e \in Events
    /\ e \notin emitted
    /\ IF Stream
       THEN /\ emitted' = emitted \cup {e}
            /\ lines' = IF Bug = "drop" THEN lines ELSE lines + 1
            /\ exits' = exits
       ELSE /\ exits = 0
            /\ emitted' = emitted
            /\ lines' = IF Bug = "oneshot-emits" THEN lines + 1 ELSE lines
            /\ exits' = exits + 1
    /\ UNCHANGED exitedOnMail

Stall(e) ==
    /\ e \in Events
    /\ e \notin emitted
    /\ IF Stream
       THEN /\ emitted' = emitted \cup {e}
            /\ lines' = IF Bug = "drop" THEN lines ELSE lines + 1
            /\ exits' = exits
       ELSE /\ exits = 0
            /\ emitted' = emitted
            /\ lines' = IF Bug = "oneshot-emits" THEN lines + 1 ELSE lines
            /\ exits' = exits + 1
    /\ UNCHANGED exitedOnMail

Preflight(e) ==
    /\ e \in Events
    /\ e \notin emitted
    /\ IF Stream
       THEN /\ emitted' = emitted \cup {e}
            /\ lines' = IF Bug = "drop" THEN lines ELSE lines + 1
            /\ exits' = exits
       ELSE /\ exits = 0
            /\ emitted' = emitted
            /\ lines' = IF Bug = "oneshot-emits" THEN lines + 1 ELSE lines
            /\ exits' = exits + 1
    /\ UNCHANGED exitedOnMail

ProofDone(e) ==
    /\ e \in Events
    /\ e \notin emitted
    /\ IF Stream
       THEN /\ emitted' = emitted \cup {e}
            /\ lines' = IF Bug = "drop" THEN lines ELSE lines + 1
            /\ exits' = exits
       ELSE /\ exits = 0
            /\ emitted' = emitted
            /\ lines' = IF Bug = "oneshot-emits" THEN lines + 1 ELSE lines
            /\ exits' = exits + 1
    /\ UNCHANGED exitedOnMail

\* Negative: in stream mode a mail exits the watch.
MailExit ==
    /\ Stream
    /\ Bug = "exit-on-mail"
    /\ exitedOnMail' = TRUE
    /\ UNCHANGED <<emitted, lines, exits>>

Next ==
    \/ \E e \in Events: Mail(e) \/ Stall(e) \/ Preflight(e) \/ ProofDone(e)
    \/ MailExit

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ emitted \subseteq Events
    /\ lines \in 0..Cardinality(Events)
    /\ exits \in 0..Cardinality(Events)
    /\ exitedOnMail \in BOOLEAN

EveryEventEmitted == Stream => lines = Cardinality(emitted)

NoExitOnMail == ~(Stream /\ exitedOnMail)

OneShotUnchanged == ~Stream => lines = 0

====
