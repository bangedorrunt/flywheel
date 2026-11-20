# Sourced by scripts/verify-mutations.sh.
# bd-evuag: the boot drain for a pin already idle must fail if skipped.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(doorbell_host::boot_drain_tests)

mutation "B1  boot refold never drains idle-at-boot pins" \
    file=src/doorbell_host.rs \
    expect=boot_refold_drains_a_deferral_for_a_pin_already_idle \
    find='                    if idle_at_boot {
                        idle_pins.insert(pin);
                    }' \
    replace='                    if false && idle_at_boot {
                        idle_pins.insert(pin);
                    }'
