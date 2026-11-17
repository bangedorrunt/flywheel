# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.8 (ADR-0021 §6): four rows had NO mutation-proven witness at all and
# the map paired them with siblings' tests. One mutant per RULE, each `expect=`
# naming the witness that row actually cites. Ids are spec-qualified:
# `adr0021-named-witnesses R1` is not `adr0021-gates ...`.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    workflow_lowering_unreachable_node_named
    workflow_lowering_can_finish_names_stranded_state
    workflow_lowering_unbounded_back_edge_named
    workflow_lowering_approval_without_timeout_fails
    stage_contract_judge_arms_without_arms_fails
    wave_swarm_missing_witness_fails_at_result_timeout
    wave_swarm_no_marker_reads
    durable_body_witness_fail_goes_to_on_error
    durable_body_witness_missing_report_fails
)

# R1 — D5.1 reachable rule: every graph node becomes an entry, so nothing is
# unreachable and the check that exists to name an orphan never fires.
mutation "R1  a graph node with an unsatisfied needs is an entry anyway" \
    file=src/workflow_lowering.rs \
    expect=workflow_lowering_unreachable_node_named \
    find='        let mut entries: Vec<String> = nodes
            .iter()
            .filter(|(_, n)| n.needs.is_empty())
            .map(|(name, _)| renames[name].clone())
            .collect();' \
    replace='        let mut entries: Vec<String> = nodes
            .iter()
            .filter(|_| true)
            .map(|(name, _)| renames[name].clone())
            .collect();'

# C1 — D5.1 can-finish rule. The check itself is toron's kernel
# (`Topology::cannot_finish`); no YAML form strands a state, so the flywheel
# witness hand-builds a machine and goes through this wrapper. Mutating the
# wrapper is the only flywheel-side surface — see the map note.
mutation "C1  a stranded state is not reported by the lowering check" \
    file=src/workflow_lowering.rs \
    expect=workflow_lowering_can_finish_names_stranded_state \
    find='    fn check_can_finish(lowered: &Lowered) -> Vec<String> {
        lowered.machine.topology().cannot_finish()
    }' \
    replace='    fn check_can_finish(lowered: &Lowered) -> Vec<String> {
        let _ = lowered;
        Vec::new()
    }'

# B1 — D5.1 back-edge rule: an unbounded repetition stops being named.
mutation "B1  an unbounded back edge is not reported" \
    file=src/workflow_lowering.rs \
    expect=workflow_lowering_unbounded_back_edge_named \
    find='        if b.max.is_none() {
            errors.push(format!(
                "unbounded repetition at {} (no finite max)",
                b.from
            ));
        }' \
    replace='        let _ = b;'

# T1 — D5.1 approval rule: a wait with no finite timeout stops being recorded.
mutation "T1  an approval with no timeout is not recorded as unbounded" \
    file=src/workflow_lowering.rs \
    expect=workflow_lowering_approval_without_timeout_fails \
    find='            Action::RequestApproval { timeout_secs, .. } => {
                for e in exit {
                    self.edge(here, e);
                }
                if timeout_secs.is_none_or(|t| t == 0) {
                    self.unbounded_waits.push(here.to_string());
                }
            }' \
    replace='            Action::RequestApproval { timeout_secs, .. } => {
                for e in exit {
                    self.edge(here, e);
                }
                let _ = timeout_secs;
            }'

# J1 — A1: the judge-arms stage stops declaring the read the lint exists to
# demand, so a workflow dispatching it with no arms step lints clean.
mutation "J1  judge-arms stops declaring its arms read" \
    file=src/stages.rs \
    expect=stage_contract_judge_arms_without_arms_fails \
    find='    ("judge-arms", &["steps.arms"]),' \
    replace='    ("judge-arms", &[]),'

# X1 — A3: the graph node's own deadline is dropped, so a coder that never
# writes a witness waits forever instead of failing at result_timeout.
mutation "X1  a graph dispatch's await loses its deadline" \
    file=src/durable_body.rs \
    expect=wave_swarm_missing_witness_fails_at_result_timeout \
    find='                    let (deadline, claimed) = match run.model().node_state.get(&node) {
                        Some(toron_workflow::engine::durable::NodeState::AwaitingJob {
                            claim_deadline_ms,
                            result_deadline_ms,
                            claimed,
                            ..
                        }) => {
                            if *claimed {
                                (*result_deadline_ms, true)
                            } else {
                                (*claim_deadline_ms, false)
                            }
                        }
                        _ => (None, false),
                    };' \
    replace='                    let (deadline, claimed) = (None, false);'

# M1 — A3/D4.4: a step reads a completion marker again.
mutation "M1  the wave reads a .done marker again" \
    file=src/builtin_workflows.rs \
    expect=wave_swarm_no_marker_reads \
    find="        task: 'Execute bead {{item.id}} ({{item.title}}) for {{inputs.objective}}." \
    replace="        task: 'test -f .flywheel/.done && Execute bead {{item.id}} ({{item.title}}) for {{inputs.objective}}."

# W1 — D4.2: a failing witness settles instead of failing through on_error.
mutation "W1  a failing witness settles as success" \
    file=src/durable_body.rs \
    expect=durable_body_witness_fail_goes_to_on_error \
    find='        WitnessGate::Failed(error) => refused(error),' \
    replace='        WitnessGate::Failed(error) => {
            let _ = (error, refused);
            settled(value)
        }'

# W2 — D4.2: a missing report is accepted instead of refused.
mutation "W2  a missing machine-readable report is accepted" \
    file=src/phase_contracts.rs \
    expect=durable_body_witness_missing_report_fails \
    find='    let Some(report) = agent_report(step_out) else {
        return PhaseContractReport::fail(
            "phase",
            format!("no machine-readable report for phase `{phase}` (response is not JSON)"),
        );
    };' \
    replace='    let Some(report) = agent_report(step_out) else {
        return PhaseContractReport::ok("phase", "mutation: missing report accepted".to_string());
    };'
