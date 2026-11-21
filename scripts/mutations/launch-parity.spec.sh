# Sourced by scripts/verify-mutations.sh.
# ADR-0022 V8 (bd-6setu.1): rex and hc build the same launch for one seat. The
# mutant appends a rex-only flag, so the command behind the env prefix stops
# equalling the argv hc is handed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(rex_and_hc_build_the_same_launch_for_one_seat)

mutation "V8  rex appends a flag the hc argv never sees" \
    file=src/pane.rs \
    expect=rex_and_hc_build_the_same_launch_for_one_seat \
    find='    let mut cmd = vec!["env".to_string()];
    cmd.append(&mut parts);
    cmd.push(binary);' \
    replace='    let mut cmd = vec!["env".to_string()];
    cmd.append(&mut parts);
    cmd.push(binary);
    cmd.push("--rex-only".to_string());'
