#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.5: stall detection reads activity, not wall time. A long build or a
# held proof slot is work; a busy pane repeating one tail is a loop.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(cmd::captain::tests)

mutation "A1  a held proof slot no longer counts as work" \
    file=src/cmd/captain.rs \
    expect=a_long_build_holding_the_proof_slot_is_not_a_stall \
    find='    if p.proof_slot_held {
        return StallVerdict::Working;
    }
' \
    replace='    let _ = p.proof_slot_held;'

mutation "A2  a moving pane no longer counts as activity" \
    file=src/cmd/captain.rs \
    expect=an_unchanged_screen_is_a_stall_and_a_moving_one_is_not \
    find='    if p.activity_age_secs.is_some_and(|age| age < p.stall_secs) {
        return StallVerdict::Working;
    }
' \
    replace='    let _ = p.activity_age_secs;'

mutation "A3  a pane repeating one tail is no longer a loop" \
    file=src/cmd/captain.rs \
    expect=a_busy_pane_repeating_one_tail_is_looping \
    find='    if p.loop_repeats >= LOOP_REPEATS {
        return StallVerdict::Looping;
    }
' \
    replace='    let _ = p.loop_repeats;'

mutation "A4  two identical tails are called a loop" \
    file=src/cmd/captain.rs \
    expect=a_busy_pane_repeating_one_tail_is_looping \
    find='pub(crate) const LOOP_REPEATS: u32 = 3;' \
    replace='pub(crate) const LOOP_REPEATS: u32 = 2;'

mutation "A5  a first sighting of a tail is not a delta" \
    file=src/cmd/captain.rs \
    expect=activity_watch_seeds_the_clock_and_counts_only_busy_tails \
    find='            if !same {
                delta = true;
            }' \
    replace='            if !same {}'

mutation "A6  an idle pane's repeated tail counts as a loop" \
    file=src/cmd/captain.rs \
    expect=activity_watch_seeds_the_clock_and_counts_only_busy_tails \
    find='                .filter(|pane| busy.contains(*pane))' \
    replace='                .filter(|_pane| true)'

mutation "A7  a held flock is read as a free slot" \
    file=src/cmd/captain.rs \
    expect=proof_slot_probe_reads_the_flock_not_the_holder_file \
    find='        Err(std::fs::TryLockError::WouldBlock) => true,' \
    replace='        Err(std::fs::TryLockError::WouldBlock) => false,'
