# Sourced by scripts/verify-mutations.sh.
# bd-i4edn: install re-registers launchd jobs from their plists, and stack
# status fails when launchd cannot spawn a flywheel job.

CARGO_ARGS=(-p flywheel)
TEST_FILTER=(
    stack_restart_reregisters_jobs_from_their_plists
    a_job_launchd_cannot_spawn_is_a_failure
)

mutation "L1  restart goes back to kickstart for every job" \
    file=scripts/stack.sh \
    expect=stack_restart_reregisters_jobs_from_their_plists \
    find='  if [ -f "$plist" ]; then' \
    replace='  if false; then'

mutation "L2  a job launchd cannot spawn reads as healthy" \
    file=src/cmd/stack.rs \
    expect=a_job_launchd_cannot_spawn_is_a_failure \
    find='        .any(|line| line.trim() == "job state = spawn failed")' \
    replace='        .any(|line| line.trim() == "job state = never")'
