# Sourced by scripts/verify-mutations.sh.
# bd-e4gal: a profileless spawn row with no harness makes harness_of return ""
# and the guard inject over the operator's typed line. Dropping the write must
# turn the row witness red.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_profileless_spawn_row_is_found_by_harness_of)

mutation "H1  the spawn row does not record the launched harness" \
    file=src/cmd/spawn.rs \
    expect=a_profileless_spawn_row_is_found_by_harness_of \
    find='        entry.harness = Some(kind.to_string());' \
    replace='        let _ = kind;'
