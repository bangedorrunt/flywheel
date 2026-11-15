# Sourced by scripts/verify-mutations.sh.
# Both-yes is a contradiction and must not collapse to a verdict (bd-3uqz).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(stuck_vs_busy::tests::both_yes_is_a_contradiction_and_is_not_resolved_by_picking_a_side)

mutation "B1  contradiction collapses to wedged" \
    file=src/stuck_vs_busy.rs \
    expect=both_yes_is_a_contradiction_and_is_not_resolved_by_picking_a_side \
    find=$'    if w > YES && k > YES {\n        return (\n            Triage::Unknown,\n            format!(\n                "{nums} — both propositions read yes, which is a contradiction in the \\\\\n                 evidence rather than a verdict; not committed"\n            ),\n        );\n    }\n' \
    replace=$'    if w > YES && k > YES {\n        return (Triage::Wedged, format!("{nums} — forced"));\n    }\n'
