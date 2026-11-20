# Sourced by scripts/verify-mutations.sh.
# bd-o1z4m review: the per-pane events config must keep the token as a PATH,
# enable the native flag, and share the doorbell rung set for one ring.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_config_carries_only_a_token_path the_pane_config_enables_the_flag_and_merges_existing_content one_event_id_rings_once_across_both_paths)

HC=src/hc.rs
BRIDGE=src/events_bridge.rs

mutation "O1  the config carries a literal Authorization header instead of a token path" \
    file=$HC \
    expect=the_config_carries_only_a_token_path \
    find='        "env": { "FLYWHEEL_TORON_TOKEN_FILE": token_path.to_string_lossy() },' \
    replace='        "headers": { "Authorization": "Bearer literal-secret" },'

mutation "O2  the native mcp_events flag is not enabled" \
    file=$HC \
    expect=the_pane_config_enables_the_flag_and_merges_existing_content \
    find='    obj.insert("mcp_events".to_string(), serde_json::Value::Bool(true));' \
    replace='    obj.insert("mcp_events".to_string(), serde_json::Value::Bool(false));'

mutation "O3  the fallback bypasses the shared rung dedupe" \
    file=$BRIDGE \
    expect=one_event_id_rings_once_across_both_paths \
    find='    event_id.is_empty() || rung_once(state_dir, event_id)' \
    replace='    true'
