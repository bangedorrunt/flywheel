# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# A cached error is an invisible one (bd-tg4n).
#
# `OnceLock::get_or_init` stores the `Err` PERMANENTLY. One failed mailbox build
# therefore made mail unavailable for the rest of the process even after the
# cause was gone. Measured in the suite: two `register_spawned_agent_*` tests in
# `wake_identity_tests` red ~1 run in 3 under the module filter, green 4/4 in
# isolation — whichever test reached `Mail::from_config` first built the global
# mailbox on its OWN temp dir, and that test's closing `remove_dir_all` deleted
# the directory the mailbox lived on.
#
# `get_or_try_init` is the one-line fix and is UNSTABLE here (`once_cell_try`),
# so the cell holds `T` and a failing builder returns before the `set`.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(mail_shim::init_once_tests)

# ONE mutation, and it is not the obvious one.
#
# The obvious mutation — restore the cached failure — DOES NOT COMPILE, and
# that is the result rather than an inconvenience. `MAILBOX`/`RUNTIME` are
# `OnceLock<T>`, not `OnceLock<Result<T, String>>`: the cell cannot hold an
# error at all, so "memoise the failure" is unrepresentable rather than merely
# untested. Restoring the bug means changing four sites (both statics,
# `init_once`, `mailbox`, `try_mailbox`) back to the old shape, which is a
# revert, not a mutation.
#
# So the mutation below targets what IS still mutable: the two early returns
# that read a memoised value back. BOTH have to go — measured, and worth
# recording: deleting only the lock-free fast path leaves the post-gate one
# intact and the suite stays GREEN, because each guard independently decides
# the same thing. The honest mutation of an over-determined guard removes both
# mechanisms, not the one that looks load-bearing.
#
# Removing both still satisfies the failure test — a rebuild-every-time helper
# retries perfectly — which is exactly why the second test exists. Without it
# "memoise success" would be indistinguishable from "retry forever" and the
# shared mailbox would be rebuilt on every operation.
#
# Needle discipline, learned the hard way here: an earlier attempt named a
# single-line-site needle that the harness filed VACUOUS rather than NEEDLE-MISS,
# because its pre-check is `grep -qF` and grep reads a newline-separated needle
# as a list of patterns to match ANY of. A VACUOUS verdict is a claim about a
# TEST; a needle that does not exist is a claim about a MUTATION. Read the diff
# back after a surprising VACUOUS.
mutation "M1  a memoised value is never read back" \
    file=src/mail_shim.rs \
    expect=a_successful_build_runs_exactly_once \
    find='    if let Some(v) = cell.get() {
        return Ok(v);
    }
    let _held = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(v) = cell.get() {
        return Ok(v); // lost the race; the winner'"'"'s value is the answer
    }' \
    replace='    let _held = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);'
