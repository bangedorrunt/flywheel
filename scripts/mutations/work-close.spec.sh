# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.3: the close path trusts nothing unverified — an empty verify
# command is refused, a passing verify never mints a tracking bead, and the
# journal receipt carries the verify command and its tail.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(adhoc_verify_comes_from_the_flag bead_auto_fires_only_when_work_outlives_the_attempt close_report_carries_verify_and_tail)

mutation "W1  an empty verify command closes" \
    file=src/work_unit.rs \
    expect=adhoc_verify_comes_from_the_flag \
    find='        if verify.trim().is_empty() {' \
    replace='        if false {'

mutation "W2  every close mints a tracking bead" \
    file=src/work_unit.rs \
    expect=bead_auto_fires_only_when_work_outlives_the_attempt \
    find='    !verify_passed && auto_bead' \
    replace='    true'

mutation "W3  the journal receipt drops the verify tail" \
    file=src/work_unit.rs \
    expect=close_report_carries_verify_and_tail \
    find='    format!("status: {status}\ncommit: {sha}\nverify: {verify}\n{tail}\n")' \
    replace='    format!("status: {status}\ncommit: {sha}\n")'
