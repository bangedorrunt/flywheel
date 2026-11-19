# Sourced by scripts/verify-mutations.sh.
# bd-bi0mv: a receipt the captain acts on is a validated schema. A guard that
# cannot fire turns prose into a false-green receipt and mis-lands a bead.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_bad_field_names_itself_in_the_error a_valid_receipt_parses_and_validates one_land_call_per_bead_with_its_commit_and_repo)

mutation "C1  a non-40-hex commit sha is accepted" \
    file=src/crew_receipt.rs \
    expect=a_bad_field_names_itself_in_the_error \
    find='    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())' \
    replace='    true'

mutation "C2  a red fence verdict passes validation" \
    file=src/crew_receipt.rs \
    expect=a_bad_field_names_itself_in_the_error \
    find='    if r.fence.verdict != "green" {' \
    replace='    if false {'

mutation "C3  crew land drops the repo column" \
    file=src/cmd/crew.rs \
    expect=one_land_call_per_bead_with_its_commit_and_repo \
    find='        land(bead, &commit.sha, &commit.repo)?;' \
    replace='        land(bead, &commit.sha, ".")?;'
