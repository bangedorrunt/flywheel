# Sourced by scripts/verify-mutations.sh.
# bd-fwt8r: a deferred ring must be dropped once the recipient's cursor has
# passed the mail that queued it. Both mutants restore a spurious ring.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(already_pulled_decision_table a_stale_deferral_is_dropped_and_journaled)

mutation "R1  the cursor comparison is ignored" \
    file=src/doorbell.rs \
    expect=already_pulled_decision_table \
    find='        (Some(current), Some(wm)) => current >= wm,' \
    replace='        (Some(_current), Some(_wm)) => false,'

mutation "R2  a behind cursor is treated as pulled" \
    file=src/doorbell.rs \
    expect=already_pulled_decision_table \
    find='        (Some(current), Some(wm)) => current >= wm,' \
    replace='        (Some(_current), Some(_wm)) => true,'
