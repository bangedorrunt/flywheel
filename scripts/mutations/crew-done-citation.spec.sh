# Sourced by scripts/verify-mutations.sh.
# bd-1czzw: crew done must refuse receipts whose commits do not cite their beads.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    an_uncited_commit_is_refused_naming_bead_and_sha
    a_valid_receipt_parses_and_validates
)

mutation "C1  the citation check is removed" \
    file=src/crew_receipt.rs \
    expect=an_uncited_commit_is_refused_naming_bead_and_sha \
    find='        if !checks.commit_cites_bead(&repo, &b.sha, &b.id) {' \
    replace='        if false && !checks.commit_cites_bead(&repo, &b.sha, &b.id) {'
