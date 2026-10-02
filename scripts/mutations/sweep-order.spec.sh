# shellcheck shell=bash
# Sourced by scripts/verify-mutations.sh.
# shellcheck disable=SC2034

# The flywheel order-blind-fixture + dead-guard sweep (bd-9w95).
#
# Two shapes, two different mutations:
#
#   * An ORDER-BLIND site resolves a tie by iteration order. The mutation puts
#     back a selector that DOES depend on order, and the named test must go red.
#     The fixture is the load-bearing half: ids are content hashes, so a fixture
#     whose winner happens to arrive last agrees with last-wins and proves
#     nothing. Every tie fixture here is checked in BOTH orders.
#
#   * A DEAD GUARD asks the opposite question — this guard is supposed to be
#     unfired, so green is the answer. `expect_green` reports green as
#     DEAD-CONFIRMED and red as STILL-LOAD-BEARING. These mutations run in the
#     INVERSE direction (add the guard back) so they stay applicable to the tree
#     as committed, rather than needing the pre-fix source that no longer exists.
#
# What stays undecided is reported as such rather than filed as coverage.

CARGO_ARGS=(--bin flywheel)
# The three tests these mutations touch, NOT the whole `wake_identity_tests`
# module. Two reasons, both measured:
#
#   * `wake_identity_tests` carries a PRE-EXISTING flake — `register_spawned_agent_*`
#     went red in roughly 1 run in 3 under the module filter and green 4/4 in
#     isolation, because their spawn registry lands in the ambient XDG_STATE_HOME.
#     A harness that runs the suite once per mutation cannot tell that flake from
#     its own mutation, and it showed up as a red "control after restore" on an
#     unmutated tree, which is exactly the signal the control exists to provide.
#   * The three targets are hermetic and finish in ~0.01s, so each mutation costs
#     a compile rather than a 37-second suite.
TEST_FILTER=(vbf2_a_shared_microsecond vbf2_newest_urgent_wins skills_injected_reads_the_later)

# --- order-blind: the wake selector ---------------------------------------
# `newest_urgent` exists because the wire order cannot be trusted (bd-vbf2).
# Keying on `created_ts_us` alone left the same defect one level down: a burst
# can land two urgent rows in the same microsecond, and the winner became
# whichever row the slice ended on.
mutation "S1  the wake selector drops the id tiebreak" \
    file=src/main.rs \
    expect=vbf2_a_shared_microsecond_resolves_by_id_in_either_order \
    find=$'            (\n                m.get("created_ts_us")\n                    .and_then(serde_json::Value::as_u64)\n                    .unwrap_or(0),\n                m.get("id")\n                    .and_then(serde_json::Value::as_str)\n                    .unwrap_or(""),\n            )' \
    replace=$'            m.get("created_ts_us")\n                .and_then(serde_json::Value::as_u64)\n                .unwrap_or(0)'

# The same site, the OTHER half of the rule: the id must break the tie, not
# outrank the timestamp. Swapping the tuple makes a 5-minute-older row with a
# lexically higher id win, which is the regression bd-vbf2's own test catches.
#
# The obvious companion — `.rev()` before `max_by_key` — was measured first and
# is VACUOUS, which says something about the MUTATION rather than the test: once
# the tiebreak exists the answer is order-independent by construction, so
# reversing the iterator cannot change it. An order-blind fixture needs an
# order-DEPENDENT mutation; reversing only proves the fix already holds.
mutation "S2  the wake selector lets the id outrank the timestamp" \
    file=src/main.rs \
    expect=vbf2_newest_urgent_wins_regardless_of_wire_order \
    find=$'            (\n                m.get("created_ts_us")\n                    .and_then(serde_json::Value::as_u64)\n                    .unwrap_or(0),\n                m.get("id")\n                    .and_then(serde_json::Value::as_str)\n                    .unwrap_or(""),\n            )' \
    replace=$'            (\n                m.get("id")\n                    .and_then(serde_json::Value::as_str)\n                    .unwrap_or(""),\n                m.get("created_ts_us")\n                    .and_then(serde_json::Value::as_u64)\n                    .unwrap_or(0),\n            )'

# `skills_injected_for`: `incarnation` counts relaunches of ONE pane, so two
# fresh panes for one name tie at 0 and the answer is the later registry row.
# This mutation makes the site read the FIRST maximum instead, which is the
# other half of an order-dependent answer.
mutation "S3  skills_injected takes the first tied row" \
    file=src/main.rs \
    expect=skills_injected_reads_the_later_registry_row_when_incarnations_tie \
    find='.max_by_key(|e| e.incarnation)' \
    replace='.rev().max_by_key(|e| e.incarnation)'

