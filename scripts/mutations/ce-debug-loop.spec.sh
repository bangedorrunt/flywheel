# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.5: ce-debug's repair loop must stop on the first passing verify.
# A guard-less loop burns all 3 fix rounds and never reports stopped="until".

CARGO_ARGS=(-p flywheel --test sc_bugfix_playbook)
TEST_FILTER=(ce_debug_loop_exits_on_first_passing_verify)

mutation "C1  ce-debug repair loop loses its until guard" \
    file=src/builtin_workflows.rs \
    expect=ce_debug_loop_exits_on_first_passing_verify \
    find='    until: steps_verify_ok == true' \
    replace='    # until guard removed by mutation'
