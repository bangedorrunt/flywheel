# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.2 (D4.5): each observe arm's signal must REACH the captain through the
# fold sweep — an arm that compiles but never escalates is the silent drop the
# seat it replaced was there to prevent.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(escalates)

# The fold wiring (bd-jhz8x.2 part 3): each arm's signal must reach the captain.
CARGO_ARGS=(-p doorbell-rt --lib)

mutation "W1  a blocked pane never escalates" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=a_blocked_pane_escalates_to_the_captain_once_per_episode \
    find='            .filter(|(_, age)| *age >= BLOCKED_AFTER_SECS)' \
    replace='            .filter(|_| false)'

mutation "R1  an unanswered release ask never escalates" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=an_unanswered_release_request_escalates_to_the_captain_once \
    find='        escalations.extend(self.release_waits.expired(now));' \
    replace='        escalations.extend(Vec::new());'

mutation "K1  a fork subject is never parsed" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=a_fork_subject_escalates_to_the_captain_once_per_bead \
    find='            if let Some((bead, question)) = doorbell_core::observe::fork_subject(subject) {' \
    replace='            if let Some((bead, question)) = doorbell_core::observe::fork_subject("") {'
