# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# The dead-guard half of the flywheel sweep (bd-9w95); the order-blind half is
# `sweep-order.spec.sh`. Split only so each run fits one command window.
#
# A DEAD GUARD asks the opposite question from a normal mutation: this guard is
# supposed to be unfired, so green is the ANSWER. `expect_green` reports green
# as DEAD-CONFIRMED and red as STILL-LOAD-BEARING. Scoring these like normal
# mutations would file the whole point of them as a failing test.
#
# Every mutation runs in the INVERSE direction — add the guard back — so it
# stays applicable to the tree as committed. The usual "delete the guard and
# watch the suite" form needs the pre-fix source, which no longer exists once
# the fix has landed, and a spec full of NEEDLE-MISS is worse than no spec.
#
# Green on an `expect_green` row means NO TEST DECIDES, which is a weaker claim
# than "the guard is dead". The comment on each mutation says which of the two
# it is, so the difference survives the run.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(bd_4pxy_tests recorder_tests artifact::tests headless::tests)

# `.max(1)` on a `u32` that already had 1 added. Two mechanisms each guaranteed
# the floor; only one was needed, so the commit deletes `.max(1)`. Re-adding it
# cannot change a test — that is the measurement, and it is why the deletion is
# safe rather than merely plausible.
mutation "S4  settle_retry re-adds the dead .max(1) floor" \
    file=src/main.rs \
    expect=settle_retry_exhaustion_returns_last_error \
    find='max_attempts: retries.saturating_add(1),' \
    replace='max_attempts: retries.saturating_add(1).max(1),' \
    expect_green

# The surviving half, proved load-bearing in the other direction: drop the
# `+1` and a 3-retry budget silently becomes 3 total attempts instead of 4.
mutation "S5  settle_retry loses the +1 that makes it a RETRY budget" \
    file=src/main.rs \
    expect=settle_retry_exhaustion_returns_last_error \
    find='max_attempts: retries.saturating_add(1),' \
    replace='max_attempts: retries,'

# Const-vs-const: 8192 minus a 14-byte suffix. `saturating_sub` can never fire
# here, so it is unfired insurance against a FUTURE edit to the constant rather
# than a guard on a runtime value — which is why the commit keeps it. Green
# confirms it protects nothing today; it does not make the insurance wrong.
mutation "S6  the snapshot truncation guard is unfired" \
    file=src/artifact.rs \
    expect=truncate \
    find='MAX_EVALUATION_SNAPSHOT_BYTES.saturating_sub(EVALUATION_TRUNCATION_SUFFIX.len())' \
    replace='MAX_EVALUATION_SNAPSHOT_BYTES - EVALUATION_TRUNCATION_SUFFIX.len()' \
    expect_green

# `(next_seq() - 512).max(0)`: the subtraction cannot underflow i64 (the seq
# starts at 0 and grows), and a negative floor prunes nothing, so the clamp is
# behaviourally inert on this path. Recorded rather than deleted — it states the
# intent, and the arm that would make it matter is a seq that can go backwards.
mutation "S7  the run-state retain floor clamp is behaviourally inert" \
    file=src/run_state_recorder.rs \
    expect=retain_prunes_strictly_below_the_stated_floor \
    find='(recorder.next_seq() - RETAIN_BOUNDARIES).max(0)' \
    replace='(recorder.next_seq() - RETAIN_BOUNDARIES)' \
    expect_green

# `argv.len().saturating_sub(1)` — the "before the prompt positional" comment
# asserts argv is never empty, so a plain subtraction would panic loudly if that
# ever stopped being true. Green here means NO TEST DECIDES, and this row is the
# one place where the two readings diverge: either argv cannot be empty (the
# guard is dead) or no test says so (the guard is unproven). Unresolved.
mutation "S8  nothing pins whether argv can be empty" \
    file=src/headless.rs \
    expect=arg \
    find='argv.len().saturating_sub(1)' \
    replace='argv.len() - 1' \
    expect_green
