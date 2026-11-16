# Sourced by scripts/verify-mutations.sh.
# bd-jhz8x.2 (ADR-0021 D4.5): a verify step's JSON stdout rides the result so a
# downstream foreach can consume it. Non-JSON stdout must leave `json` None.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(verify_sink_carries_json)

mutation "J1  non-JSON stdout is reported as an empty object" \
    file=src/cmd/internal.rs \
    expect=verify_sink_carries_json_stdout_and_none_for_plain_text \
    find='                    let json = serde_json::from_str::<serde_json::Value>(&stdout)
                        .ok()
                        .filter(|v| !v.is_null());' \
    replace='                    let json = Some(serde_json::json!({}));'
