# Sourced by scripts/verify-mutations.sh.
# bd-9ddjh: snapshot, digest and the deferral scan read only the journals they ask for.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    journal_read_workspaces_reads_only_the_wanted_journals
)

mutation "P1  scoped read parses every workspace journal" \
    file=src/doorbell.rs \
    expect=journal_read_workspaces_reads_only_the_wanted_journals \
    find='        for row in store.fold_workspace(ws).rows {' \
    replace='        for row in journal_workspaces_list(state_dir).iter().flat_map(|w| store.fold_workspace(w).rows) {'

mutation "P1  full read drops the workspace list" \
    file=src/doorbell.rs \
    expect=journal_read_workspaces_reads_only_the_wanted_journals \
    find='    journal_read_workspaces(state_dir, &workspaces)' \
    replace='    journal_read_workspaces(state_dir, &Default::default())'
