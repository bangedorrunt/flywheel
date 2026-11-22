# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.6 (ADR-0021 §6): five decisions were WITNESSED but never
# mutation-proven, so a regression in any of them read green. One mutant per
# item; each must name the test the map cites. The reachable/can-finish pair
# mutants the PRODUCTION report (`chart_report`), not the test-only helper.
#
# Ids are spec-qualified: `adr0021-witnessed E1` is not `adr0021-gates ...`.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    stage_contract_seed_phases_complete
    dispatch_resolves_effort_stage_over_profile
    statechart_report_fails_on_an_orphan_and_a_trap
    stage_contract_list_filters
)

# E1 — D3.5: the review/verify seeds stop being fresh + read-only.
mutation "E1  a review seed stops being fresh and read-only" \
    file=src/stages.rs \
    expect=stage_contract_seed_phases_complete \
    find='        for name in SEED_FRESH_READONLY {
            if let Some(stage) = stages.get_mut(*name) {
                stage.context = Some(StageContext::Fresh);
                stage.mode = Some(StageMode::ReadOnly);
            }
        }' \
    replace='        let _ = SEED_FRESH_READONLY;'

# E2 — D3.6: the stage's effort stops reaching the leaf, so the profile wins.
mutation "E2  the stage effort no longer reaches the leaf" \
    file=src/cmd/internal.rs \
    expect=dispatch_resolves_effort_stage_over_profile \
    find='        effort: crate::headless::resolve_dispatch_field(spec_effort, stage_effort, profile_effort),' \
    replace='        effort: crate::headless::resolve_dispatch_field(spec_effort, None, profile_effort),'

# E4 — D5.2: an unreachable state is not reported.
# E4 — D5.2: an unreachable state reports as reachable.
mutation "E4  an unreachable state is not reported" \
    file=src/cmd/workflow.rs \
    expect=statechart_report_fails_on_an_orphan_and_a_trap \
    find='    let unreachable = names(topology.unreachable());' \
    replace='    let unreachable = names(Vec::<S>::new());'

# E5 — D5.2: a stranded state (no path to the goal) is not reported.
mutation "E5  a stranded state is not reported" \
    file=src/cmd/workflow.rs \
    expect=statechart_report_fails_on_an_orphan_and_a_trap \
    find='    let stuck = names(topology.cannot_reach(is_goal));' \
    replace='    let stuck = names(Vec::<S>::new());'

# E6 — D6: `stage list --phase` stops filtering.
mutation "E6  stage list --phase ignores the phase" \
    file=src/stages.rs \
    expect=stage_contract_list_filters \
    find='        .filter(|r| r.phase.as_deref() == Some(phase))' \
    replace='        .filter(|_| true)'
