# Sourced by scripts/verify-mutations.sh.
# ce-lfg is the light loop and ideates only behind explore; ce-wave owns the
# planned path and its exit judges only the wave it was given; a branch arm
# seats every role its sequential steps use; brainstorm asks ce-lfg to explore.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    ce_lfg_ideates_only_when_asked_to_explore
    wave_swarm_execute_exit_runs_against_ledger
    ce_wave_exit_checks_the_wave_it_was_given
    a_branch_arm_seats_every_role_its_steps_use_and_reuses_sequential_ones
    adr0019_spec8_rows_resolve_to_their_primitive
)

mutation "X1  ce-lfg ideates on every run" \
    file=src/builtin_workflows.rs \
    expect=ce_lfg_ideates_only_when_asked_to_explore \
    find='    if: "{{inputs.explore}} == true"' \
    replace='    if: "1 == 1"'

mutation "X2  an open bead outside the wave blocks its exit" \
    file=src/builtin_workflows.rs \
    expect=wave_swarm_execute_exit_runs_against_ledger \
    find='IDS=$(br list --all --json | jq -r "$WAVE | select(.status != \"closed\") | .id")' \
    replace='IDS=$(br list --all --json | jq -r ".issues[] | select(.status != \"closed\") | .id")'

mutation "X3  the exit hard-codes a wave number again" \
    file=src/builtin_workflows.rs \
    expect=ce_wave_exit_checks_the_wave_it_was_given \
    find='index("{{inputs.wave_label}}"))' \
    replace='index("wave-2")) or .wave == 2)'

mutation "X4  an arm is sized by its busiest single step" \
    file=src/crew.rs \
    expect=a_branch_arm_seats_every_role_its_steps_use_and_reuses_sequential_ones \
    find='    let mut peaks: BTreeMap<String, usize> = BTreeMap::new();
    for s in steps {' \
    replace='    let mut peaks: BTreeMap<String, usize> = BTreeMap::new();
    for s in steps.iter().take(1) {'

mutation "X5  brainstorm runs ce-lfg without explore" \
    file=src/decide.rs \
    expect=adr0019_spec8_rows_resolve_to_their_primitive \
    find='            inputs: r#"{"explore":true}"#,' \
    replace='            inputs: r#"{}"#,'
