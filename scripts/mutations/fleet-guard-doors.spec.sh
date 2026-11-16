# Sourced by scripts/verify-mutations.sh.
# Every DOOR that replaces a live binary or bounces the doorbell must consult
# the captain-only guard (bd-0uzrq.24): `flywheel upgrade`, `flywheel stack
# install`, `doorbell supervise`, and scripts/stack.sh install called directly.

CARGO_ARGS=(-p flywheel --test fleet_guard --test stack_guard)
TEST_FILTER=(refuses_a_crew)

mutation "D1  upgrade installs without the guard" \
    file=src/main.rs \
    expect=upgrade_refuses_a_crew_pane_with_no_pin \
    find='        Some(Commands::Upgrade { operator }) => {
            crate::authority::guard_fleet_mutation(&cfg, None, *operator, "flywheel upgrade")
                .and_then(|()| cmd_upgrade())
        }' \
    replace='        Some(Commands::Upgrade { operator: _ }) => cmd_upgrade(),'

mutation "D2  doorbell supervise bounces without the guard" \
    file=src/main.rs \
    expect=doorbell_supervise_refuses_a_crew_pin \
    find='            DoorbellCmd::Supervise { project, operator } => {
                crate::authority::guard_fleet_mutation(
                    &cfg,
                    Some(project),
                    *operator,
                    "doorbell supervise",
                )
                .and_then(|()| crate::cmd::doorbell::cmd_doorbell_supervise(&cfg, project))
            }' \
    replace='            DoorbellCmd::Supervise { project, operator: _ } => {
                crate::cmd::doorbell::cmd_doorbell_supervise(&cfg, project)
            }'

mutation "D3  stack install skips the guard" \
    file=src/cmd/stack.rs \
    expect=stack_install_refuses_a_crew_pin \
    find='            if !dry_run {' \
    replace='            if false {'

mutation "D4  the script installs for anyone" \
    file=scripts/stack.sh \
    expect=stack_script_refuses_a_crew_pin \
    find='  require_fleet_authority' \
    replace='  : require_fleet_authority  # mutation: guard dropped'

mutation "D5  daemon start bounces the live hosts without the guard" \
    file=src/main.rs \
    expect=daemon_start_refuses_a_crew_pin \
    find='    if !what.is_empty() {
        crate::authority::guard_fleet_mutation(cfg, None, operator, what)?;
    }' \
    replace='    let _ = (operator, what);'
