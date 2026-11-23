# Sourced by scripts/verify-mutations.sh.
# bd-n21ml: a red proof must route its rows to the owning crews.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    red_rows_mail_the_owner_with_log_and_rerun
)

mutation "R1  the red-row mailer never runs" \
    file=scripts/clean-proof.sh \
    expect=red_rows_mail_the_owner_with_log_and_rerun \
    find='if receipt.get("ok"):' \
    replace='if True:'

mutation "R2  unresolved owner falls back to a git name" \
    file=scripts/clean-proof.sh \
    expect=red_rows_mail_the_owner_with_log_and_rerun \
    find='captain = os.environ.get("AGENT_NAME") or sender' \
    replace='captain = "bangedorrunt"'

mutation "R3  a sibling step is resolved in the flywheel repo" \
    file=scripts/clean-proof.sh \
    expect=red_rows_mail_the_owner_with_log_and_rerun \
    find='                scoped_repo = os.path.normpath(os.path.join(repo, sib["path"]))' \
    replace='                scoped_repo = repo'

mutation "R4  caught RED rows are mailed as failures again" \
    file=scripts/clean-proof.sh \
    expect=red_rows_mail_the_owner_with_log_and_rerun \
    find='    if not isinstance(row, dict) or row.get("verdict") == "RED":' \
    replace='    if not isinstance(row, dict) or row.get("verdict") != "RED":'
