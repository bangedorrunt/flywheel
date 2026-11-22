# Sourced by scripts/verify-mutations.sh.
# Stage prompts on {target, verify}: renames alias, mechanics names retire,
# review and verify trace the production caller, crews never close beads,
# and the review/implement witnesses accept a clean review and unbound work.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    aliases_are_renames_and_mechanics_names_are_retired
    review_and_verify_trace_the_production_caller
    ce_work_claims_with_the_pin_and_never_closes
    a_clean_review_passes_only_with_agree
    implement_contract_failing_witnesses
)

mutation "R1  a mechanics name aliases a stage again" \
    file=src/stages.rs \
    expect=aliases_are_renames_and_mechanics_names_are_retired \
    find='    ("wave-verify", "ce-verify"),
];' \
    replace='    ("wave-verify", "ce-verify"),
    ("wave-dispatch", "ce-plan"),
];'

mutation "R2  ce-verify drops the seam check" \
    file=src/stages.rs \
    expect=review_and_verify_trace_the_production_caller \
    find='SEAM CHECK: confirm the changed code' \
    replace='CHECK: confirm the changed code'

mutation "R3  ce-work teaches a crew-side close" \
    file=src/stages.rs \
    expect=ce_work_claims_with_the_pin_and_never_closes \
    find='so never close it yourself.' \
    replace='then run br close <id>.'

mutation "R4  an unbacked REJECT passes with no findings" \
    file=src/phase_contracts.rs \
    expect=a_clean_review_passes_only_with_agree \
    find='        Some(v) if REVIEW_VERDICTS.contains(&v) && findings.is_empty() && v != "AGREE" => {' \
    replace='        Some(v) if REVIEW_VERDICTS.contains(&v) && findings.is_empty() && false => {'

mutation "R5  a bound bead need not be cited" \
    file=src/phase_contracts.rs \
    expect=implement_contract_failing_witnesses \
    find='        && !message.contains(bead)' \
    replace='        && false'
