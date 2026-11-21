# Sourced by scripts/verify-mutations.sh.
# bd-4n2bn: the pi seat's events bridge must load a first-party extension and
# carry only a TOKEN PATH, never the secret, into the seat config/env.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(pi_events_config_uses_a_per_seat_agentdir_and_a_token_path omp_seats_take_the_pi_style_events_bridge)

mutation "P1  a plaintext token env replaces the file path" \
    file=src/hc.rs \
    expect=pi_events_config_uses_a_per_seat_agentdir_and_a_token_path \
    find='    env.insert(
        "FLYWHEEL_TORON_TOKEN_FILE".to_string(),
        token_path.display().to_string(),
    );' \
    replace=$'    let _ = &token_path;\n    env.insert("FLYWHEEL_TORON_TOKEN".to_string(), "literal-token".to_string());'

mutation "P3  an omp seat falls back to the doorbell" \
    file=src/cmd/spawn.rs \
    expect=omp_seats_take_the_pi_style_events_bridge \
    find='    matches!(kind, "pi" | "omp")' \
    replace='    matches!(kind, "pi")'

mutation "P2  the seat settings no longer load the extension" \
    file=src/hc.rs \
    expect=pi_events_config_uses_a_per_seat_agentdir_and_a_token_path \
    find='        arr.push(serde_json::Value::String(want));' \
    replace='        let _ = want;'
