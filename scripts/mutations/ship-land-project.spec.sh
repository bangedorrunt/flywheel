# Sourced by scripts/verify-mutations.sh.
#
# bd-ni0ek: `ship land` resolves the toron project with the ONE resolver, never
# from the repo directory's name. A wrong slug silently releases the wrong
# leases, so the refusal is the behaviour under test.
#
#   scripts/verify-mutations.sh scripts/mutations/ship-land-project.spec.sh --cargo mbx
#
# What this does not prove: the gate/close/release steps after resolution —
# they run against a real tracker and are covered by ship land itself.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(land_refuses_a_repo_whose_dir_is_not_its_toron_slug)

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
