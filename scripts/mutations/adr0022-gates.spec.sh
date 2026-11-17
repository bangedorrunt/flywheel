# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# ADR-0022's own gates (bd-6setu.5, audit gap 2). Every row breaks one rule the
# ADR substitutes for review judgement, and the witness named in `expect` must
# go RED. A gate nobody has seen fail is a comment with extra steps.
#
# Rows 1-7 are the decisions and state-machine laws; rows 8-9 are the two
# acceptance claims the audit found asserted but never witnessed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    prefer_spawn_user_model_wins
    resolve_ask_retired_without_replacement_is_retired
    plan_gate_blocked_with_yes_asks_and_names_remedy
    prefer_spawn_model_from_preference
    seat_reconcile_env_model_gives_respawn_fresh
    seat_reconcile_no_change_gives_keep
    seat_binder_reviewer_never_author
    plan_patch_harness_refilters_model_and_clears_consent
    plan_machine_jev_timeout_reaches_resolved_then_awaiting
    plan_machine_jev_rate_limited_reaches_resolved
    plan_machine_jev_missing_key_reaches_resolved
    plan_machine_jev_shrug_reaches_resolved
    timeout_alone_never_allows_rebind
    needs_consent_lists_jev_field
    needs_consent_over_budget_lists_estimate
    the_precedence_ladder_never_lets_a_lower_rung_win
    a_briefing_plan_carries_its_consent_items
)

SPAWN=src/cmd/spawn.rs
PLAN=src/swarm_plan.rs
MODELS=src/models.rs
RECONCILE=src/seat_reconcile.rs
BINDER=src/seat_binder.rs
CHART=src/elm/plan_statechart.rs

# D2: user > preference. The mutant lets a remembered preference overwrite an
# explicit --model, which is the one precedence inversion a captain feels.
mutation "D2  a preference overwrites the user's explicit model" \
    file=$SPAWN \
    expect=prefer_spawn_user_model_wins \
    find='    if bare_model.is_empty() {' \
    replace='    if true {'

# D4: retirement is data. The mutant ignores `retired`, so an id with no
# replacement resolves instead of refusing with the remedy.
mutation "D4  a retired model with no replacement resolves anyway" \
    file=$MODELS \
    expect=resolve_ask_retired_without_replacement_is_retired \
    find='if self.models.get(&cur).is_some_and(|e| e.retired) {' \
    replace='if false {'

# D5/V3: a blocked seat never launches. The mutant makes the blocked guard
# vacuous, so --yes confirms the plan and the seat with a remedy never given
# goes out.
mutation "V3  a blocked seat reaches Confirmed under --yes" \
    file=$CHART \
    expect=plan_gate_blocked_with_yes_asks_and_names_remedy \
    find='    items.iter().any(|i| i.reason == ConsentReason::Blocked)' \
    replace='    items.iter().any(|_| false)'

# D6/acceptance 4: ask once. The mutant treats a standing preference as
# consent-worthy, so the next session asks the question the preference already
# answered.
mutation "D6  a remembered preference is asked again" \
    file=$PLAN \
    expect=prefer_spawn_model_from_preference \
    find='                    if field.as_ref().is_some_and(|f| f.from == Provenance::Jev) {' \
    replace='                    if field.as_ref().is_some_and(|f| matches!(f.from, Provenance::Jev | Provenance::Preference)) {'

# D7/V5: an env-delivered model change never takes Relaunching. The mutant
# relaunches an env-pinned harness, which cannot change the model it started
# with.
mutation "V5  an env-delivered model takes Relaunching" \
    file=$RECONCILE \
    expect=seat_reconcile_env_model_gives_respawn_fresh \
    find='        if model_by_env(live.harness.as_str()) {' \
    replace='        if false {'

# V9: reconciling a swarm that already matches yields no patch. The mutant
# invents a model diff, so every reconcile relaunches a correct seat.
mutation "V9  a matching seat still yields a patch" \
    file=$RECONCILE \
    expect=seat_reconcile_no_change_gives_keep \
    find='    let model_diff = desired.model != live.model;' \
    replace='    let model_diff = true;'

# V10: a review or verify step never binds the seat that authored the work.
# The mutant drops the author filter, so the reviewer inherits the author.
mutation "V10  a review step binds its author" \
    file=$BINDER \
    expect=seat_binder_reviewer_never_author \
    find='.filter(|s| !is_author(need, &s.pin))' \
    replace='.filter(|_| true)'

# D5: THE RULE, not the gate. A Jev value is asked about; dropping that arm
# makes the captain launch on a Jev choice nobody confirmed.
mutation "D5A  a Jev field never enters needs_consent" \
    file=$PLAN \
    expect=needs_consent_lists_jev_field \
    find='                    if field.as_ref().is_some_and(|f| f.from == Provenance::Jev) {' \
    replace='                    if false {'

# D5 rule 3: an estimate over the policy budget is asked about.
mutation "D5B  an over-budget plan never enters needs_consent" \
    file=$PLAN \
    expect=needs_consent_over_budget_lists_estimate \
    find='        if est_usd_per_mtok.is_some_and(|est| est > policy.budget_usd_per_mtok) {' \
    replace='        if false {'

# V7 as the LADDER, not one pair: invert it and every ordered pair fails.
mutation "V7  the precedence ladder lets a lower rung win" \
    file=$PLAN \
    expect=the_precedence_ladder_never_lets_a_lower_rung_win \
    find='        self.rank() < other.rank()' \
    replace='        self.rank() > other.rank()'

# A2: briefing carries its consent items, not just seats.
mutation "A2  a briefing attaches seats but no consent items" \
    file=src/briefing.rs \
    expect=a_briefing_plan_carries_its_consent_items \
    find='    plan.needs_consent = consent_plan::needs_consent(&plan, policy, None, cap);' \
    replace='    plan.needs_consent = Vec::new();'

# Acceptance 3: a harness patch re-filters the seat's model and clears the
# consent entry. The mutant drops the patch, so the plan keeps the old harness
# and the model that belonged to it.
mutation "A3  a harness patch is dropped before re-resolution" \
    file=$SPAWN \
    expect=plan_patch_harness_refilters_model_and_clears_consent \
    find='            crate::swarm_plan::PatchField::Harness => e.harness = Some(p.value.clone()),' \
    replace='            crate::swarm_plan::PatchField::Harness => e.harness = None,'

# V2 (bd-6setu.7): a Jev failure never blocks Resolve. The mutant gates the
# Drafting->Resolved edge on JevOutcome::Ok, so a timeout, a 429, a missing key
# or a shrug leaves the plan drafting forever.
mutation "V2  a Jev failure blocks Resolve" \
    file=$CHART \
    expect=plan_machine_jev_timeout_reaches_resolved_then_awaiting \
    find='                    .on(|e| matches!(e, PlanEvent::Resolve { .. }))' \
    replace='                    .on(|e| matches!(e, PlanEvent::Resolve { jev: JevOutcome::Ok, .. }))'

# V6 (bd-6setu.7): no hop on Timeout / Unknown / empty evidence. The mutant lets
# a bare timeout drive a rebind, which is the failure class toron ADR-0032 says
# must never justify one.
mutation "V6  a timeout alone allows a rebind" \
    file=src/agent_ref.rs \
    expect=timeout_alone_never_allows_rebind \
    find='                | FailureKind::RateLimit' \
    replace='                | FailureKind::RateLimit
                | FailureKind::Timeout'
