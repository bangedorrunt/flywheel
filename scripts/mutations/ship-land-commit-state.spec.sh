# Sourced by scripts/verify-mutations.sh.
# bd-kz3sc: the tracker-commit step's done-state must come from HEAD, not from
# the old staging flag. Trusting the flag skips forever after a failed commit.

CARGO_ARGS=(-p flywheel --test ship_land)
TEST_FILTER=(a_failed_tracker_commit_leaves_the_step_pending_and_a_resume_retries_it)

mutation "L1  tracker done-state trusts the staging flag (always done)" \
    file=src/cmd/ship.rs \
    expect=a_failed_tracker_commit_leaves_the_step_pending_and_a_resume_retries_it \
    find='    if bead_rows_committed(&dir, &plan.bead)? {' \
    replace='    if true {'
