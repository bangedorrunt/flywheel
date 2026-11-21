# Sourced by scripts/verify-mutations.sh. bd-08t3o: a witness must not inherit
# the outer proof's scratch export/build dir.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(proof_verify_mutations_ignores_an_inherited_build_dir)

mutation "A1  the pinned-siblings witness inherits the proof build dir" \
    file=scripts/test-verify-mutations-pinned-siblings.sh \
    expect=proof_verify_mutations_ignores_an_inherited_build_dir \
    find='unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR' \
    replace=':'
