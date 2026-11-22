# Sourced by scripts/verify-mutations.sh.
# The memory preflight asks chie once per check and reports how far it got:
# one free-text query plus one glob check per path, a worst-status fold, and
# an incomplete check that never renders as clean.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    check_trap_is_one_call_with_query_command_and_path
    briefing_sends_one_query_and_one_check_per_path_and_never_recalls
    a_hung_path_check_keeps_the_query_hits_and_says_timeout
    the_worst_call_status_wins
    an_offline_check_never_renders_as_clean
)

MEM=src/cmd/memory.rs

mutation "U1  modified paths drop out of the briefing query" \
    file=$MEM \
    expect=briefing_sends_one_query_and_one_check_per_path_and_never_recalls \
    find='    terms.extend(paths.iter().map(String::as_str));' \
    replace='    let _ = &paths;'

mutation "U2  untracked directories collapse to one entry" \
    file=$MEM \
    expect=briefing_sends_one_query_and_one_check_per_path_and_never_recalls \
    find='        .args(["status", "--porcelain", "--untracked-files=all"])' \
    replace='        .args(["status", "--porcelain"])'

mutation "U3  the CLI check stops reading the query as a command" \
    file=$MEM \
    expect=check_trap_is_one_call_with_query_command_and_path \
    find='        command: Some(q.to_string()),' \
    replace='        command: None,'

mutation "U4  a timed-out call reads as offline" \
    file=$MEM \
    expect=a_hung_path_check_keeps_the_query_hits_and_says_timeout \
    find='            return CallOutcome::Timeout(e.to_string());' \
    replace='            return CallOutcome::Offline;'

mutation "U5  the fold forgets a timeout" \
    file=$MEM \
    expect=the_worst_call_status_wins \
    find='                status = status.max(TrapStatus::Timeout);' \
    replace='                let _ = TrapStatus::Timeout;'

mutation "U6  an offline check with no traps renders as clean" \
    file=$MEM \
    expect=an_offline_check_never_renders_as_clean \
    find='    if check.traps.is_empty() && note.is_none() {' \
    replace='    if check.traps.is_empty() {'
