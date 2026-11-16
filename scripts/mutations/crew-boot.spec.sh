# Sourced by scripts/verify-mutations.sh.
# bd-koafr: the boot prompt decides whether a crew starts on its bead or
# spends its first turns on mail and awareness triage.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    an_assigned_boot_names_the_bead_and_skips_triage
    an_unassigned_boot_waits_instead_of_triaging
)

mutation "B1  an unassigned crew is sent to triage" \
    file=src/main.rs \
    expect=an_unassigned_boot_waits_instead_of_triaging \
    find='        None => "Boot ready check: reply exactly READY and wait for an \' \
    replace='        None => "Boot ready check: reply exactly READY and read your mail, \'

mutation "B2  an assigned crew is sent to triage anyway" \
    file=src/main.rs \
    expect=an_assigned_boot_names_the_bead_and_skips_triage \
    find='             and run its VERIFY. Do not read mail or triage the project \' \
    replace='             and run its VERIFY. Read your mail and triage the project  \'
