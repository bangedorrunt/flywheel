# Sourced by scripts/verify-mutations.sh.
# ADR-0021's own gates (bd-jhz8x.1). Each mutant breaks one production check and
# the gate witness listed in `expect` must go RED: a check nobody has seen fail
# is a comment (ADR acceptance #6).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(adr0021)

mutation "S1  a fan-out shape lints clean without a fan-out step" \
    file=src/workflow_catalog.rs \
    expect=adr0021_shape_lint_refuses_a_fanout_without_parallel \
    find='            if has_fanout {
                Ok(())
            } else {
                Err("shape fan-out needs a parallel/foreach step".to_string())
            }' \
    replace='            let _ = has_fanout;
            Ok(())'

mutation "C1  a lower tier's compose declaration beats the upper tier" \
    file=src/workflow_catalog.rs \
    expect=adr0021_compose_override_wins_from_the_upper_tier \
    find='            compose: upper.compose.or(self.compose),' \
    replace='            compose: self.compose.or(upper.compose),'

mutation "R1  a stage's missing reads are not reported" \
    file=src/cmd/workflow.rs \
    expect=adr0021_reads_lint_refuses_a_missing_read \
    find='        let missing = stages::reads_unsatisfied(&stage.reads, &declared, &step_prior);' \
    replace='        let missing: Vec<String> = Vec::new();'

mutation "W1  an unknown witness succeeds by default" \
    file=src/phase_contracts.rs \
    expect=adr0021_witness_exit_refuses_an_empty_report \
    find='    if !WITNESSES.contains(&name) {
        return PhaseContractReport::fail(
            "witness",
            format!("unknown witness `{name}` (valid: {})", WITNESSES.join(", ")),
        );
    }' \
    replace='    if !WITNESSES.contains(&name) {
        return PhaseContractReport::ok("witness", "mutation: unknown witness accepted".into());
    }'

mutation "L1  an unbounded wait is not flagged" \
    file=src/workflow_lowering.rs \
    expect=adr0021_lowering_flags_an_unbounded_wait \
    find='                if self.resolve_max(until_ms).is_none() || *max_wait_secs == 0 {' \
    replace='                if false {'

mutation "F1  a changed digest converges anyway" \
    file=src/plan_fence.rs \
    expect=adr0021_plan_fence_never_converges_on_a_changed_digest \
    find='    Some(new_digest) == previous' \
    replace='    true || Some(new_digest) == previous'
