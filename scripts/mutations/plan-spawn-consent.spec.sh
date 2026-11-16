# Sourced by scripts/verify-mutations.sh.
# bd-6setu.3 (ADR-0022 D1 condition 3): a planned spawn is what needs consent.
# Stop naming spawns and apply starts panes with nobody asked.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_lost_seat_is_a_planned_spawn)

mutation "S1  a planned spawn is not named for consent" \
    file=src/cmd/swarm.rs \
    expect=a_lost_seat_is_a_planned_spawn \
    find='            SeatEffect::Spawn { role, count } => Some(format!("{role} x{count}")),' \
    replace='            SeatEffect::Spawn { .. } => None,'
