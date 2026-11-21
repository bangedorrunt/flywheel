# Sourced by scripts/verify-mutations.sh.
# bd-62ei8: a commit whose tree equals HEAD must be refused unless --allow-empty.
# Disabling the guard makes the refusal test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(an_unchanged_path_refuses_an_empty_commit_unless_allowed)

COMMIT=src/cmd/commit.rs

mutation "E1  the empty-commit guard is disabled" \
    file=$COMMIT \
    expect=an_unchanged_path_refuses_an_empty_commit_unless_allowed \
    find='    if allow_empty {
        return Ok(());
    }
    let head_tree' \
    replace='    if true {
        return Ok(());
    }
    let head_tree'
