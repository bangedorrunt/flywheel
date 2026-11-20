# Sourced by scripts/verify-mutations.sh.
# bd-42vhw: ship land records a gate the bead's policy allows and reads the
# bead's OWN status (bd-pgfqg's HEAD-lock retry moved to the private-index verb).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    the_legal_gate_set_is_read_from_br_and_respected
    closed_is_the_beads_own_status_not_a_child_field
)

SHIP=src/cmd/ship.rs

mutation "G1  the gate name is hardcoded again" \
    file=$SHIP \
    expect=the_legal_gate_set_is_read_from_br_and_respected \
    find='    for want in ["command-verified", "unit-test-verified"] {
        if legal.iter().any(|name| name == want) {
            return Some(want.to_string());
        }
    }
    legal.first().cloned()' \
    replace='    legal
        .iter()
        .find(|name| *name == "unit-test-verified")
        .cloned()'

mutation "S1  closed is a substring of the payload again" \
    file=$SHIP \
    expect=closed_is_the_beads_own_status_not_a_child_field \
    find='    let doc: Value = serde_json::from_str(br_json).ok()?;
    doc.get("status")
        .and_then(Value::as_str)
        .map(str::to_string)' \
    replace='    let _ = serde_json::from_str::<Value>(br_json);
    br_json
        .contains("status")
        .then(|| "closed".to_string())'
