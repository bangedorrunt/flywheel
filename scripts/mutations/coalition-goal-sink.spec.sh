#!/usr/bin/env bash
# bd-yf465: coalition form must run its goal on an engine with a job sink.
# Removing the recording runner reintroduces the sink-less engine, so a goal
# with dispatch_job (ce-compound) fails and every coalition forming test dies.
set -uo pipefail

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(form_builtin_goal_with_dispatch_steps_needs_a_job_sink)

mutation "S1  coalition form loses its job sink" \
  file=src/coalition.rs \
  expect=form_builtin_goal_with_dispatch_steps_needs_a_job_sink \
  find='                Some(crate::cron::recording_runner()),' \
  replace='                None,'
