# Sourced by scripts/verify-mutations.sh.
# bd-aberm: the private GIT_INDEX_FILE is the whole point of the verb. Drop it
# and another lane's staged files come back, and the fence sees the shared tree.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(shared_index_staged_files_are_never_swept fence_sees_only_the_private_tree named_paths_are_committed_byte_equal a_red_fence_leaves_head_and_shared_index_untouched concurrent_head_move_is_retried_not_overwritten)

mutation "P1  the private index is ignored, git uses the shared index" \
    file=src/cmd/commit.rs \
    expect=shared_index_staged_files_are_never_swept \
    find='            cmd.env("GIT_INDEX_FILE", index);' \
    replace='            cmd.env("GIT_INDEX_FILE", Path::new("/dev/null"));'
