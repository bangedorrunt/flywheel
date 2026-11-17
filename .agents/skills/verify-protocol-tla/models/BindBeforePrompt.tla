---- MODULE BindBeforePrompt ----
\* Binding BEFORE the prompt (ADR-0022 D8, bd-6setu.2; model bd-6setu.19).
\*
\* Rust correspondence (flywheel):
\*   bind then prompt   src/pane.rs  pane_dispatch (the binding row is written
\*                      BEFORE the pane prompt, keyed (run, node, step))
\*   the row itself     src/dispatch_bindings.rs  append / authors_for
\*   replay reads rows  src/pane.rs  the author lookup for a review step
\*
\* Boundary: one pin, one step. The prompt is a single event; what happens
\* inside the agent's turn is out of scope. The claim being modelled is narrow
\* and load-bearing: a prompt that is visible to the world while its binding row
\* does not exist is a dispatch nobody can attribute, and a replay would read
\* an authorless prompt.
EXTENDS Naturals, TLC

CONSTANTS Bug
\* Bug \in {"none", "prompt-first"}: negative fixture. "none" writes the row,
\* then prompts. "prompt-first" prompts while the row is still absent, which is
\* the ordering the code must never take.

VARIABLES row, prompted, crashed

vars == <<row, prompted, crashed>>

\* The row's lifecycle: absent -> prepared (journaled, before any prompt).
Init ==
    /\ row = "absent"
    /\ prompted = FALSE
    /\ crashed = FALSE

Bind ==
    /\ row = "absent"
    /\ row' = "prepared"
    /\ UNCHANGED <<prompted, crashed>>

\* The prompt may only land once the row exists. The negative fixture is the
\* one mutation of this guard.
Prompt ==
    /\ prompted = FALSE
    /\ (Bug = "prompt-first" \/ row /= "absent")
    /\ prompted' = TRUE
    /\ UNCHANGED <<row, crashed>>

\* A crash anywhere leaves the disk as it is; the next process reads it.
Crash ==
    /\ crashed = FALSE
    /\ crashed' = TRUE
    /\ UNCHANGED <<row, prompted>>

Restart ==
    /\ crashed = TRUE
    /\ crashed' = FALSE
    /\ UNCHANGED <<row, prompted>>

Next ==
    \/ Bind
    \/ Prompt
    \/ Crash
    \/ Restart

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ row \in {"absent", "prepared"}
    /\ prompted \in BOOLEAN
    /\ crashed \in BOOLEAN

\* D8: a visible prompt always has a binding row — across a crash too, because
\* the row is on disk before the prompt is written anywhere.
PromptedImpliesBound == prompted => row /= "absent"

====
