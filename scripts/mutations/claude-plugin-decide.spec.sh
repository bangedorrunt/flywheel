# Sourced by scripts/verify-mutations.sh. bd-9os20: the Claude module's pure
# decision must keep its subagent, urgency and stop-after-3 rules.

CARGO_ARGS=(--test hook_decide)
TEST_FILTER=(hook_decide)

mutation "D1  subagents receive notices" \
    file=src/hook_decide.rs \
    expect=subagents_never_receive_notices \
    find='    let healthy = wake.failures < MAX_FAILURES && !wake.subagent && wake.claimed;' \
    replace='    let healthy = wake.failures < MAX_FAILURES && wake.claimed;'

mutation "D2  urgent mail submits instead of appending" \
    file=src/hook_decide.rs \
    expect=normal_mail_submits_and_urgent_appends \
    find='        Action::Append' \
    replace='        Action::Submit'

mutation "D3  delivery never stops" \
    file=src/hook_decide.rs \
    expect=delivery_stops_after_three_failed_checks \
    find='wake.failures < MAX_FAILURES' \
    replace='true'
