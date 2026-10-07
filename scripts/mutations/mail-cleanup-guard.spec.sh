# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# A guard that can never fire (bd-gpaa).
#
# The bd-tg4n cleanup helper refused to remove a test's temp dir when it was the
# process-global mailbox's own directory. It compared with `==`:
#
#     if built_mail_root() == Some(dir) { return; }
#
# and could therefore never match. `built_mail_root()` reads `BUILT_ROOTS`, which
# `build_mailbox` fills with the Mailbox DATA dir — `{root}/projects` — while
# every caller passes the DURABLE ROOT `{root}` that it set `TORON_DATA_DIR` to.
# Two different paths, so the guard was a permanent no-op that read like a fix.
# `wake_identity_tests` kept failing 2 runs in 12, verbatim:
#
#   mail error subscription rejected: replay failure: error returned from
#   database: (code: 1) no such table: bus_events
#
# The pool was still open; the store underneath it had been removed by the test
# that happened to reach `Mail::from_config` first, whose own closing cleanup
# deleted its own temp dir.
#
# bd-tg4n could not test the guard at all — `BUILT_ROOTS` is a `OnceLock` only
# populated by a live mailbox build — so it shipped a test for the OTHER half
# ("an unrelated dir is still removed") and a comment claiming the guard branch
# was covered by measured end-to-end behaviour. That is the vacuity this spec
# exists against: the fix was half-tested, and the half that mattered was the
# half that did not work. The predicate is now pure — `(built, dir) -> bool` —
# which is what makes every branch drivable.
#
# `TEST_FILTER` is one hermetic module: no env, no mailbox, no clock. These are
# the same cases the previous spec drove, plus the ancestor shape.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(mail_shim::cleanup_test_data_dir_tests)

# G1 — THE REGRESSION. Back to equality, i.e. exactly what bd-tg4n shipped. The
# durable-root case is the one every caller in the module hits, so this is the
# mutation that would have caught the original no-op.
mutation "G1  the guard is equality again, so it never matches the root" \
    file=src/mail_shim.rs \
    expect=the_durable_root_holding_the_store_is_protected \
    find='    matches!(built, Some(root) if root.starts_with(dir))' \
    replace='    matches!(built, Some(root) if root == dir)'

# G2 — the guard deleted outright. The data-dir spelling is the one the equality
# guard DID get right, so it is the branch that must not lose protection when
# the ancestor form is added.
mutation "G2  the guard protects nothing" \
    file=src/mail_shim.rs \
    expect=the_store_dir_itself_is_protected \
    find='    matches!(built, Some(root) if root.starts_with(dir))
}' \
    replace='    let _ = (built, dir);
    false
}'

# G3 — the plausible wrong repair. Someone reaches for a TEXT prefix check
# instead of a path one; every case still passes except the sibling, and
# `fw-bind-stamp-XY` would keep `fw-bind-stamp-XYZ`'s store alive forever.
mutation "G3  the guard compares string prefixes, not path components" \
    file=src/mail_shim.rs \
    expect=a_string_prefix_sibling_is_not_protected \
    find='    matches!(built, Some(root) if root.starts_with(dir))' \
    replace='    matches!(built, Some(root) if root.to_string_lossy().starts_with(dir.to_string_lossy().as_ref()))'

# G4 — over-correction: protect everything. This is the direction a guard drifts
# when the symptom is "a store went missing" and the fix is "delete less". It
# leaks a temp dir per test per run and is invisible in a red-suite reading.
mutation "G4  the guard protects everything, leaking every temp dir" \
    file=src/mail_shim.rs \
    expect=an_unrelated_dir_and_a_missing_store_are_unprotected \
    find='    matches!(built, Some(root) if root.starts_with(dir))' \
    replace='    built.is_some() || dir.as_os_str().is_empty()'

# G5 — the cleanup stops deleting. Without this, G1–G4 all pass on a helper that
# never touches the filesystem, and the suite stops leaving temp dirs behind
# while every protection test stays green.
mutation "G5  the cleanup removes nothing at all" \
    file=src/mail_shim.rs \
    expect=an_unrelated_temp_dir_is_still_removed \
    find='    let _ = std::fs::remove_dir_all(dir);
}' \
    replace='    let _ = dir;
}'