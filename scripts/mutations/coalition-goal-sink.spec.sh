#!/usr/bin/env bash
# bd-yf465: coalition form must give the goal engine a job sink.
# Removing the injected runner reintroduces a sink-less engine, so a goal
# with dispatch_job (ce-compound) fails and form cannot complete.
set -uo pipefail

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    form_builtin_goal_with_dispatch_steps_needs_a_job_sink
    production_store_wires_the_live_goal_runner
)

mutation "S1  coalition form loses its job sink" \
  file=src/coalition.rs \
  expect=form_builtin_goal_with_dispatch_steps_needs_a_job_sink \
  find='                Some(self.goal_runner.clone()),' \
  replace='                None,'

mutation "S2  production store defaults to the dry-run recorder" \
  file=src/coalition.rs \
  expect=production_store_wires_the_live_goal_runner \
  find='                    goal_runner: live_goal_runner(project, "coalition-goal")?,' \
  replace='                    goal_runner: crate::cron::recording_runner(),'
