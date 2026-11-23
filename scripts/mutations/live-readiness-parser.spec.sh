# Sourced by scripts/verify-mutations.sh. bd-live-guard-target-readiness:
# the live guard's readiness check must be about the NAMED pin, not any seat.

CARGO_ARGS=(-p flywheel --test live_doorbell_operator_guard)
TEST_FILTER=(readiness_targets_the_named_pin_not_any_seat)

mutation "R1  readiness falls back to any idle seat" \
    file=tests/live_doorbell_operator_guard.rs \
    expect=readiness_targets_the_named_pin_not_any_seat \
    find='        .find(|r| r.get("pin").and_then(serde_json::Value::as_str) == Some(pin))?;' \
    replace='        .find(|r| r.get("status").and_then(serde_json::Value::as_str).is_some_and(|s| s == "idle" || s == "done"))?;'
