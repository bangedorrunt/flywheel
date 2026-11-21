# Sourced by scripts/verify-mutations.sh.
# bd-tdpix: the claim must be atomic across processes and the doorbell must
# drop the Prompt when it loses the claim.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(n_racing_claimers_produce_exactly_one_winner a_lost_claim_emits_no_prompt a_watch_line_without_a_prompt_leaves_the_claim_free a_real_watch_line_through_the_fold_carries_and_claims_its_event_id a_sweep_conflict_prompt_claims_before_delivery)

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

mutation "T3  Prompts stop claiming by event id" \
    file=src/doorbell_host.rs \
    expect=a_real_watch_line_through_the_fold_carries_and_claims_its_event_id \
    find=$'            } => crate::wake_rung::claim(state_dir, id),' \
    replace='            } => true,'


mutation "T5  the typed event id is dropped (falls back to line comparison)" \
    file=src/doorbell_host.rs \
    expect=a_real_watch_line_through_the_fold_carries_and_claims_its_event_id \
    find='                event_id: watch_id.map(str::to_string),' \
    replace='                event_id: None,'

mutation "T6  a Sweep conflict Prompt bypasses the claim" \
    file=$HOST \
    expect=a_sweep_conflict_prompt_claims_before_delivery \
    find='                        event_id,' \
    replace='                        event_id: None,'
