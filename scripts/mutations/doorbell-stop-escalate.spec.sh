# Sourced by scripts/verify-mutations.sh.
# bd-15tbq: stop escalates TERM -> KILL when the target ignores TERM. Drop the
# escalation signal and a hung daemon is reported stopped while it still runs.

CARGO_ARGS=(-p doorbell-cli)
TEST_FILTER=(stop_escalates_to_sigkill_when_sigterm_is_ignored)

mutation "E1  stop never escalates to SIGKILL" \
    file=crates/doorbell-cli/src/lifecycle.rs \
    expect=stop_escalates_to_sigkill_when_sigterm_is_ignored \
    find='        world.signal(pid, Sig::Kill);' \
    replace='        // mutation: no escalation'
