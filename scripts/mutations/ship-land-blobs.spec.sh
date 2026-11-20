# Sourced by scripts/verify-mutations.sh.
# bd-kz3sc rework: ship land must commit ONLY this bead's tracker rows, never a
# neighbour's dirty rows from the shared worktree.

CARGO_ARGS=(-p flywheel --test ship_land)
TEST_FILTER=(a_neighbours_dirty_tracker_row_is_not_committed_with_this_bead)

mutation "B1  commit the whole tracker file instead of this bead's rows" \
    file=src/cmd/ship.rs \
    expect=a_neighbours_dirty_tracker_row_is_not_committed_with_this_bead \
    find='        let work_rows: Vec<&str> = work
            .lines()
            .filter(|line| line_is_bead(line, key, bead))
            .collect();' \
    replace='        let work_rows: Vec<&str> = work.lines().collect();'
