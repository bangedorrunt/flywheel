# Sourced by scripts/verify-mutations.sh.
#
# bd-ni0ek: `ship land` resolves the toron project with the ONE resolver, never
# from the repo directory's name. A wrong slug silently releases the wrong
# leases, so the refusal is the behaviour under test.
#
#   scripts/verify-mutations.sh scripts/mutations/ship-land-project.spec.sh --cargo mbx
#
# bd-m5afp rides the same area: `ship land` refuses a receipt that carries no
# mutation witnesses, or one whose row did not produce its declared verdict.
#
# What this does not prove: the gate/close/release steps after resolution —
# they run against a real tracker and are covered by ship land itself.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::ship::tests)

SHIP=src/cmd/ship.rs
CRON=src/cron.rs

mutation "L1  the refusal falls back to the directory name" \
    file=$SHIP \
    expect=land_refuses_a_repo_whose_dir_is_not_its_toron_slug \
    find='    crate::cron::detect_project_in(cfg, dir, explicit).ok_or_else(|| {' \
    replace=$'    crate::cron::detect_project_in(cfg, dir, explicit)\n        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))\n        .ok_or_else(|| {'

mutation "L2  the resolver accepts a directory outside the workspace root" \
    file=$CRON \
    expect=land_refuses_a_repo_whose_dir_is_not_its_toron_slug \
    find='    let rel = dir.strip_prefix(cfg.workspace_root()).ok()?;' \
    replace='    let rel = dir.strip_prefix(cfg.workspace_root()).unwrap_or(dir);'

mutation "L3  a receipt with no mutation witnesses lands" \
    file=$SHIP \
    expect=a_receipt_without_mutation_witnesses_cannot_land \
    find='    if rows_opt.map_or(true, Vec::is_empty) {' \
    replace='    if false {'

mutation "L4  a mutation that stayed green still lands" \
    file=$SHIP \
    expect=a_receipt_without_mutation_witnesses_cannot_land \
    find='        if verdict != want {' \
    replace='        if false {'

mutation "L5  the receipt is not bound to the landed sha" \
    file=$SHIP \
    expect=a_receipt_is_bound_to_the_commit_it_proved \
    find='    if proved != sha {' \
    replace='    if false {'

mutation "L6  an empty mutation set lands without re-deriving the targets" \
    file=$SHIP \
    expect=a_receipt_is_bound_to_the_commit_it_proved \
    find='        if let Some(path) = changed.iter().find(|p| spec_targets_include(root, p)) {' \
    replace='        if let Some(path) = changed.iter().find(|_p| false) {'

mutation "L7  a dirty-tree receipt lands a commit" \
    file=$SHIP \
    expect=a_receipt_is_bound_to_the_commit_it_proved \
    find='    if doc.get("mode").and_then(Value::as_str) == Some("dirty") {' \
    replace='    if false {'

