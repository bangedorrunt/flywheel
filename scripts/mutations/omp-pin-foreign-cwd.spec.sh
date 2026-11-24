#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-omp-pin-wrong-cwd-ivool: the omp build pin must ride with a seat spawned
# from any project cwd.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(omp_version_pin_survives_a_foreign_cwd)

mutation "O1  the omp launch drops the pinned build" \
    file=src/main.rs \
    expect=omp_version_pin_survives_a_foreign_cwd \
    find='"omp" => format!(
            "mise exec github:can1357/oh-my-pi@{} -- omp",
            crate::harness_registry::OMP_DEFAULT_VERSION
        ),' \
    replace='"omp" => "omp".to_string(),'
