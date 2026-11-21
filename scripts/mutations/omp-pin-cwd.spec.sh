# Sourced by scripts/verify-mutations.sh.
# bd-omp-pin: the launch must name the pinned omp build regardless of cwd.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(omp_pin_survives_a_foreign_project_cwd)

MAIN=src/main.rs

mutation "P1  omp launch falls back to a bare PATH binary" \
    file=$MAIN \
    expect=omp_pin_survives_a_foreign_project_cwd \
    find='        "omp" => format!(' \
    replace='        "never-a-harness" => format!('
