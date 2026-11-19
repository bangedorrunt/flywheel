# Sourced by scripts/verify-mutations.sh.
# bd-bi0mv: a receipt the captain acts on is a validated schema. Each mutant
# restores one field-level guard or the shared repo resolver.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_bad_field_names_itself_in_the_error a_fence_that_does_not_prove_a_bead_is_refused repo_resolution_is_shared_and_bounded land_targets_the_resolved_repo_not_the_callers_cwd)

mutation "C1  a non-40-hex bead sha is accepted" \
    file=src/crew_receipt.rs \
    expect=a_bad_field_names_itself_in_the_error \
    find='    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())' \
    replace='    true'

mutation "C2  a red fence verdict passes validation" \
    file=src/crew_receipt.rs \
    expect=a_bad_field_names_itself_in_the_error \
    find='    if r.fence.verdict != FenceVerdict::Green {' \
    replace='    if false {'

mutation "C3  a stale fence sha is accepted" \
    file=src/crew_receipt.rs \
    expect=a_fence_that_does_not_prove_a_bead_is_refused \
    find='        let proved = b.sha == r.fence.sha || checks.is_ancestor(&repo, &b.sha, &r.fence.sha);' \
    replace='        let proved = true;'

mutation "C4  crew land ignores the shared repo resolver" \
    file=src/cmd/crew.rs \
    expect=land_targets_the_resolved_repo_not_the_callers_cwd \
    find='        let repo = crate::crew_receipt::resolve_repo(workspace_root, project, &b.repo)
            .map_err(|e| anyhow!("invalid typed receipt ({e})"))?;' \
    replace='        let repo = std::path::PathBuf::from(&b.repo);'
