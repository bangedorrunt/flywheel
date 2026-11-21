# Sourced by scripts/verify-mutations.sh.
# bd-cdite: typed mail importance must reach the plugin as meta.importance.
# Dropping it at either bridge hop makes the real-fixture test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(occurrence_importance_survives_the_bridge_to_the_wire)

BRIDGE=src/events_bridge.rs

mutation "I1  the notice drops the occurrence importance" \
    file=$BRIDGE \
    expect=occurrence_importance_survives_the_bridge_to_the_wire \
    find='        importance: event.importance.clone(),' \
    replace='        importance: None,'

mutation "I2  the wire omits meta.importance" \
    file=$BRIDGE \
    expect=occurrence_importance_survives_the_bridge_to_the_wire \
    find='    if let Some(importance) = &notice.importance {
        meta["importance"] = serde_json::json!(importance);
    }' \
    replace='    let _ = &notice.importance;'
