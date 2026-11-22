# Sourced by scripts/verify-mutations.sh.
# bd-ihiqo.2: old log shapes must be recognised and skipped silently.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    old_log_shapes_are_skipped_silently
)

mutation "G1  legacy shapes are no longer recognised" \
    file=src/state.rs \
    expect=old_log_shapes_are_skipped_silently \
    find='    value.get("kind").is_none() || value["kind"].as_str() == Some("already-pulled")' \
    replace='    value.get("kind").is_none() && value["kind"].as_str() == Some("already-pulled")'
