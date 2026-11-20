# Sourced by scripts/verify-mutations.sh.
# bd-tdpix: the claim must be atomic across processes and the doorbell must
# drop the Prompt when it loses the claim.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(n_racing_claimers_produce_exactly_one_winner a_lost_claim_emits_no_prompt a_watch_line_without_a_prompt_leaves_the_claim_free a_deferred_wake_claims_when_it_rings_and_is_silent_if_native_won)

RUNG=src/wake_rung.rs
HOST=src/doorbell_host.rs

mutation "T1  the claim uses a truncating create instead of O_EXCL" \
    file=$RUNG \
    expect=n_racing_claimers_produce_exactly_one_winner \
    find='        .create_new(true)' \
    replace='        .create(true).truncate(true)'

mutation "T2  a lost claim still emits the Prompt (record after ring)" \
    file=$HOST \
    expect=a_lost_claim_emits_no_prompt \
    find='    if claimed {' \
    replace='    if true {'

mutation "T3  a Watch line with no Prompt still spends the claim" \
    file=src/doorbell_host.rs \
    expect=a_watch_line_without_a_prompt_leaves_the_claim_free \
    find=$'    if !has_prompt {\n        return effects;\n    }' \
    replace='    if false { return effects; }'

mutation "T4  a deferred ring ignores a lost claim" \
    file=src/doorbell_host.rs \
    expect=a_deferred_wake_claims_when_it_rings_and_is_silent_if_native_won \
    find='        Some(_) => crate::doorbell_host::gate_prompt_on_claim(false, effects),' \
    replace='        Some(_) => effects,'
