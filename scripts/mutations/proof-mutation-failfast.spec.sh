# Sourced by scripts/verify-mutations.sh. bd-xwn96: the proof lever must stop
# at the first unproven spec, name it in the receipt, and let a crew witness
# land under the key the proof will look up.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_mutation_failfast_and_crew_cache)

mutation "F1  the mutation stage never stops early" \
    file=scripts/proof-lib.sh \
    expect=proof_mutation_failfast_and_crew_cache \
    find='  [ "$1" -ne 0 ] && [ "$2" -eq 0 ]' \
    replace='  return 1'

mutation "F2  the receipt forgets the failing spec" \
    file=scripts/proof-lib.sh \
    expect=proof_mutation_failfast_and_crew_cache \
    find='    "failed_mutation_spec": os.environ.get("PROOF_MUTATION_FAILED_SPEC") or None,' \
    replace='    "failed_mutation_spec": None,'

mutation "F3  the crew never computes a proof-compatible key" \
    file=scripts/verify-mutations.sh \
    expect=proof_mutation_failfast_and_crew_cache \
    find='        out_args+=("--cache-key" "$CACHE_KEY")' \
    replace='        out_args+=("--no-cache")'
