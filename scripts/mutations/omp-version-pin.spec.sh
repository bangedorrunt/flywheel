# Sourced by scripts/verify-mutations.sh.
# bd-t2z0b: an omp spawn with no version must use the pinned pi-fork build.
# Removing the pin makes the spawn-plan test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(omp_with_no_version_uses_the_pinned_default)

REG=src/harness_registry.rs

mutation "V1  omp has no pinned default version" \
    file=$REG \
    expect=omp_with_no_version_uses_the_pinned_default \
    find='        "omp" => Some(OMP_DEFAULT_VERSION),' \
    replace='        "omp" => None,'
