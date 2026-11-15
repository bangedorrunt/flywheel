# Sourced by scripts/verify-mutations.sh.
# Retiring a duplicate supervisor unit must delete the plist/unit file
# (bd-ksbtp / supervisor.retire-plist).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(doorbell_supervise::tests::unload_retires_units_and_stops_children)

mutation "P1  retire leaves the duplicate unit file behind" \
    file=src/doorbell_supervise.rs \
    expect=doorbell_supervise::tests::unload_retires_units_and_stops_children \
    find=$'    if path.is_file() {\n        std::fs::remove_file(&path)?;\n    }' \
    replace=$'    if path.is_file() {\n        // mutation: retire must delete the unit file\n    }'
