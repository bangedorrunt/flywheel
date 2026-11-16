# Sourced by scripts/verify-mutations.sh.
# bd-6setu.3 (ADR-0022 D1): the plan record's id is minted once and kept, so a
# re-resolve joins the plan the live swarm runs. Mint again on every resolve and
# the swarm forks a second identity that no replay can match.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(plan_record_mints_once_and_keeps_the_patch_log)

mutation "P1  a re-resolve mints a second plan id" \
    file=src/state.rs \
    expect=plan_record_mints_once_and_keeps_the_patch_log \
    find='            .map(|r| r.id.clone())' \
    replace='            .map(|_r| mint_plan_id())'
