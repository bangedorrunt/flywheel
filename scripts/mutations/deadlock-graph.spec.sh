# Sourced by scripts/verify-mutations.sh.
# bd-v4dcv: the wait-for graph finds lease cycles and aging collab waits.
# Dropping overlap, cycle close-out, or aging must turn the matching test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(overlapping_leases_form_a_two_pin_cycle disjoint_leases_stay_out_of_the_graph a_collab_wait_against_a_lease_edge_is_a_cycle unknown_askers_count_as_waits_not_cycles reported_cycles_do_not_page_twice)

mutation "D1  disjoint leases also wait" \
    file=src/deadlock.rs \
    expect=disjoint_leases_stay_out_of_the_graph \
    find='            if !overlap {
                continue;
            }' \
    replace='            if false {
                continue;
            }'

mutation "D2  cycles never close" \
    file=src/deadlock.rs \
    expect=overlapping_leases_form_a_two_pin_cycle \
    find='        if scc.len() > 1 || self_loop {' \
    replace='        if false {'

mutation "D3  waits never age" \
    file=src/deadlock.rs \
    expect=unknown_askers_count_as_waits_not_cycles \
    find='(|t| now_us.saturating_sub(t) >= threshold_us)' \
    replace='(|_| false)'
