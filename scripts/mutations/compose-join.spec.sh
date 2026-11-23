# Sourced by scripts/verify-mutations.sh. bd-3ann1: join binds to live seats
# and never lets a reviewer audit its own author.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(join_binds_execute_to_crew_and_spawns_only_a_reviewer a_reviewer_seat_is_never_the_author a_bound_step_dispatches_to_the_pin_with_the_bead_header a_join_bound_step_dispatches_to_the_chosen_pin compose_join_carries_stage_and_bead_header_to_the_runner)

mutation "J1  every step spawns a fresh pane" \
    file=src/compose_join.rs \
    expect=join_binds_execute_to_crew_and_spawns_only_a_reviewer \
    find=$'            Binding::Reuse { pin }\n            | Binding::FreshSession { pin }\n            | Binding::Other { pin }\n            | Binding::Patch { pin, .. } => (Some(pin), false, false),\n            Binding::Spawn => (None, true, false),' \
    replace=$'            Binding::Reuse { pin }\n            | Binding::FreshSession { pin }\n            | Binding::Other { pin }\n            | Binding::Patch { pin, .. } => (None, true, false),\n            Binding::Spawn => (None, true, false),'

mutation "J2  a reviewer may audit its own work" \
    file=src/compose_join.rs \
    expect=a_reviewer_seat_is_never_the_author \
    find='            reviews_author,' \
    replace='            reviews_author: BTreeSet::new(),'

mutation "J3  a bound step still dispatches to a fresh profile" \
    file=src/compose_join.rs \
    expect=a_bound_step_dispatches_to_the_pin_with_the_bead_header \
    find='            *assignee = pin.clone();' \
    replace='            let _ = pin;'

# bd-sfkm8: the real compose-join -> pane_dispatch path, not just the struct.
mutation "J4  the dispatch runner ignores the bound pin override" \
    file=src/pane.rs \
    expect=a_join_bound_step_dispatches_to_the_chosen_pin \
    find='        .or(bound_entry.as_ref())' \
    replace='        .or_else(|| { let _ = &bound_entry; None })'

mutation "J5  the bound step loses its BEAD header" \
    file=src/compose_join.rs \
    expect=compose_join_carries_stage_and_bead_header_to_the_runner \
    find='if let Some(id) = bead.filter(|b| !b.is_empty()) {' \
    replace='if let Some(id) = bead.filter(|b| b.is_empty()) {'
