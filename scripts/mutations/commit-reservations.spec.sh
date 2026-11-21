# Sourced by scripts/verify-mutations.sh. bd-ahkw1: commit refuses a live
# foreign lease, clears dead ones and releases its own on success; a missing
# pin or an unreadable toron answer fails closed.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_live_reservation_refuses_the_commit a_dead_lease_is_cleared_and_a_success_releases_own a_missing_pin_refuses_and_names_the_escape an_unanswerable_toron_refuses_instead_of_passing)

mutation "R1  commit ignores a live foreign lease" \
    file=src/cmd/commit.rs \
    expect=a_live_reservation_refuses_the_commit \
    find='            guard_commit_reservations(repo, paths, ctx)?;' \
    replace='            let _ = (paths, ctx);'

mutation "R2  a successful commit keeps its own leases" \
    file=src/cmd/commit.rs \
    expect=a_dead_lease_is_cleared_and_a_success_releases_own \
    find='                release_own_reservations(repo, paths, ctx);' \
    replace='                let _ = (paths, ctx);'

mutation "R3  a missing pin silently skips the guard" \
    file=src/cmd/commit.rs \
    expect=a_missing_pin_refuses_and_names_the_escape \
    find=$'    let pin = ctx.pin.as_deref().ok_or_else(|| {\n        anyhow!("flywheel commit: no AGENT_NAME/--as identity; set it or {RESERVATION_ESCAPE}")\n    })?;' \
    replace='    let pin = ctx.pin.as_deref().unwrap_or("");'

mutation "R4  an unreadable toron answer is read as no conflicts" \
    file=src/cmd/commit.rs \
    expect=an_unanswerable_toron_refuses_instead_of_passing \
    find=$'    if !out.status.success() {\n        bail!(\n            "flywheel commit: toron reserve conflicts failed' \
    replace=$'    if false {\n        bail!(\n            "flywheel commit: toron reserve conflicts failed'
