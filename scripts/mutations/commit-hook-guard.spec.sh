# Sourced by scripts/verify-mutations.sh. bd-j1g64: both repos' pre-commit
# hooks must refuse a plain git commit with the flywheel commit hint.

CARGO_ARGS=(--test commit_hook_guard)
TEST_FILTER=(flywheel_hook_refuses_a_plain_commit_with_the_hint toron_hook_refuses_a_plain_commit_with_the_repo_hint)

mutation "H1  flywheel's hook accepts a plain commit" \
    file=scripts/hooks/pre-commit \
    expect=flywheel_hook_refuses_a_plain_commit_with_the_hint \
    find='if [ -z "${FLYWHEEL_COMMIT_TOKEN:-}" ] && [ "${FLYWHEEL_COMMIT_OPERATOR:-}" != "1" ]; then' \
    replace='if false; then'

mutation "H2  toron's hint drops the repo flag" \
    file=../toron/scripts/hooks/pre-commit \
    expect=toron_hook_refuses_a_plain_commit_with_the_repo_hint \
    find='echo "toron: use: flywheel commit <paths> -m <msg> --repo ../toron" >&2' \
    replace='echo "toron: use: flywheel commit <paths> -m <msg>" >&2'
