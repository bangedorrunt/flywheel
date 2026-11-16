# Sourced by scripts/verify-mutations.sh.
# The proof harness's own two rules (bd-jp9z1.5): the suite step executes the
# tests/ targets, and the mutation harness never edits the caller's tree.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_)

mutation "S1  the suite step only runs --bin flywheel" \
    file=scripts/clean-proof.sh \
    expect=proof_suite_step_runs_the_tests_directory \
    find='run_step suite "${MBX[@]}" test -p flywheel --tests' \
    replace='run_step suite "${MBX[@]}" test -p flywheel --bin flywheel'

mutation "I1  the mutation harness edits the caller tree" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_never_edits_the_shared_tree \
    find='        isolate_if_shared "${original[@]}"' \
    replace='        : isolate_if_shared "${original[@]}"'

# bd-proof-toron-bin: the proof builds the sibling toron for the suite, then the
# mutation stage runs in an export whose ../toron link is the unbuilt workspace
# tree. T1 stops handing the built CLI over; T2 drops it across the isolation
# re-exec. Either one makes adr0027-s1-receipts fail only inside the proof.

mutation "T1  the mutation stage never hears about the built toron" \
    file=scripts/clean-proof.sh \
    expect=proof_mutation_stage_hands_the_built_toron_to_the_mutations \
    find='    [ -n "$TORON_BIN_BUILT" ] && proof_env+=("TORON_BIN=$TORON_BIN_BUILT")' \
    replace='    :'

mutation "T2  the isolation re-exec drops the built toron" \
    file=scripts/verify-mutations.sh \
    expect=proof_mutation_stage_hands_the_built_toron_to_the_mutations \
    find='        TORON_BIN="$toron_bin" VERIFY_MUTATIONS_IN_EXPORT=1 exec bash' \
    replace='        VERIFY_MUTATIONS_IN_EXPORT=1 exec bash'

# bd-proof-toron-bin, second half: the handed-over path must be a file the proof
# OWNS. toron/target in the export is a symlink into mbx's shared cache, which
# mbx evicts mid-proof (proof38: "run failed: NotFound"). T3 hands over the
# target path again.

mutation "T3  the built toron is handed over from the evictable target dir" \
    file=scripts/clean-proof.sh \
    expect=proof_mutation_stage_hands_the_built_toron_to_the_mutations \
    find='    TORON_BIN_BUILT="$TMP/bin/toron"' \
    replace='    TORON_BIN_BUILT="$TMP/toron/target/debug/toron"'
