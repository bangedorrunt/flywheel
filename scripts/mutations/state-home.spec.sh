#!/usr/bin/env bash
# bd-uu58: `lock_env()` must pin a hermetic XDG_STATE_HOME, and the restore in
# TestStateHome::drop must put back a foreign value rather than clearing it.
#
# Every mutation here targets src/config.rs. Each one must turn a named test
# RED; a green run means the test cannot fail and proves nothing.
set -uo pipefail

# One filter for the WHOLE spec, so every mutation's `expect=` test is actually
# in the run. A filter naming only M1/M3's test makes M2 look VACUOUS: its named
# test is not selected, so the suite stays green for the wrong reason.
CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(config::tests::)

mutation "M1: constructor stops pinning XDG_STATE_HOME" \
  file=src/config.rs \
  find='    let prev = std::env::var_os("XDG_STATE_HOME");
    unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };' \
  replace='    let prev = std::env::var_os("XDG_STATE_HOME");
    let _ = &dir;' \
  expect=lock_env_pins_a_private_hermetic_state_dir

mutation "M2: drop clears the var instead of restoring the prior value" \
  file=src/config.rs \
  find='        match self.prev.take() {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }' \
  replace='        let _ = self.prev.take();
        unsafe { std::env::remove_var("XDG_STATE_HOME") };' \
  expect=drop_restores_a_foreign_prior_value_rather_than_clearing_it

# M3: every lock-holder gets the SAME dir, so a stale fixture is inherited by
# the next test. This is the bd-9w95 flake mechanism, so it must be RED.
mutation "M3: lock_env returns a shared dir instead of a fresh one" \
  file=src/config.rs \
  find='pub fn lock_env() -> TestStateHome {
    test_state_home()
}' \
  replace='pub fn lock_env() -> TestStateHome {
    static SHARED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let dir = SHARED.get_or_init(|| {
        let d = std::env::temp_dir().join("flywheel-bd-uu58-shared");
        std::fs::create_dir_all(&d).expect("create shared");
        d
    });
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = std::env::var_os("XDG_STATE_HOME");
    unsafe { std::env::set_var("XDG_STATE_HOME", dir) };
    TestStateHome { _env, dir: dir.clone(), prev }
}' \
  expect=lock_env_pins_a_private_hermetic_state_dir
