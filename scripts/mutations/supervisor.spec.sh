# Sourced by scripts/verify-mutations.sh.
#
# A stale supervisor unit is retired only through the port, and the finder keys
# by the canonical project (bd-bj9xk).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(stale_units_for_one_project_are_found_by_canonical_key)

DAEMON=src/daemon.rs

mutation "V1  the finder forgets the canonical-key guard" \
    file=$DAEMON \
    expect=stale_units_for_one_project_are_found_by_canonical_key \
    find=$'        if label == format!("dev.flywheel.doorbell.{canonical}") {\n            continue;\n        }\n' \
    replace=''
