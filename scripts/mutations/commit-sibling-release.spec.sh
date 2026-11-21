# Sourced by scripts/verify-mutations.sh.
# bd-kam8b: a commit from a sibling repo checks and releases the
# project-relative paths the board holds. Dropping the mapping leaves the
# sibling's leases held (and the check blind), which the commit test catches.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(sibling_commit_checks_and_releases_prefixed_paths sibling_paths_map_repo_prefixed_for_the_board)

mutation "K1  the project repo is prefixed too" \
    file=src/cmd/commit.rs \
    expect=sibling_paths_map_repo_prefixed_for_the_board \
    find='        .filter(|rel| !rel.as_os_str().is_empty())' \
    replace='        .filter(|_| true)'

mutation "K2  the release frees the repo-relative paths" \
    file=src/cmd/commit.rs \
    expect=sibling_commit_checks_and_releases_prefixed_paths \
    find='            &release.join(","),' \
    replace='            &paths.join(","),'

mutation "K3  the check asks about repo-relative paths" \
    file=src/cmd/commit.rs \
    expect=sibling_commit_checks_and_releases_prefixed_paths \
    find='            &check.join(","),' \
    replace='            &paths.join(","),'
