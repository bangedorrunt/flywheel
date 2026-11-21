# Sourced by scripts/verify-mutations.sh.
# bd-mdpfr: uninstall must never touch a foreign (rex) hook entry.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(install_is_idempotent_and_foreign_entries_survive trampoline_is_a_noop_outside_a_seat)

mutation "I1  uninstall drops every entry, including foreign rex hooks" \
    file=src/integration.rs \
    expect=install_is_idempotent_and_foreign_entries_survive \
    find='                !cmd.is_some_and(|c| c.contains(&format!(" --agent {agent} ")))' \
    replace='                true'

mutation "I2  the trampoline runs in the operator's own session" \
    file=src/integration.rs \
    expect=trampoline_is_a_noop_outside_a_seat \
    find='    in_pane && !pin.is_empty() && seat_harness == Some(harness)' \
    replace='    true'
