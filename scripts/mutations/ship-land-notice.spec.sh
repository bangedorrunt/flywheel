# Sourced by scripts/verify-mutations.sh.
# bd-n21ml: a landed bead must mail its assignee exactly once.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    landed_notice_names_the_bead_sha_and_assignee
    ship_land_sends_exactly_one_landed_notice
)

mutation "S1  the notice loses its bead correlation" \
    file=src/cmd/ship.rs \
    expect=landed_notice_names_the_bead_sha_and_assignee \
    find='        "--corr".into(),
        bead.into(),' \
    replace='        "--corr".into(),
        String::new(),'

mutation "S2  the sender stops building the notice" \
    file=src/cmd/ship.rs \
    expect=ship_land_sends_exactly_one_landed_notice \
    find='    let args = landed_mail_args(&plan.project, &plan.pin, &to, &plan.bead, sha);' \
    replace='    let args: Vec<String> = Vec::new();'
