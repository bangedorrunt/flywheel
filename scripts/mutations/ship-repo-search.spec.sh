# Sourced by scripts/verify-mutations.sh.
# bd-9zqjo (b): a flywheel tracker row can prove work that landed in another
# repo. `--repo` moves only the git SEARCH; the tracker checks stay in `--dir`.
# S1 restores the pre-fix shape (search the tracker repo) and the witness must
# go RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(head_names_bead_searches_the_work_repo_when_named)

mutation "S1  the ship gate asks the tracker repo instead of the named one" \
    file=src/cmd/ship.rs \
    expect=head_names_bead_searches_the_work_repo_when_named \
    find='        .args(["merge-base", "--is-ancestor", &sha, "HEAD"])
        .current_dir(search)' \
    replace='        .args(["merge-base", "--is-ancestor", &sha, "HEAD"])
        .current_dir(cwd)'
