# Sourced by scripts/verify-mutations.sh.
# bd-6setu.1 (ADR-0022 D3): the coupled set is a registry row, never a second
# `matches!` beside it. The mutant puts the hardcoded set back in models.rs
# while the registry keeps the flag, so the two truths drift.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(the_coupled_set_is_the_registry_row the_coupled_resolver_follows_the_registry_rows)

mutation "C1  the resolver stops reading the registry row" \
    file=src/models.rs \
    expect=the_coupled_resolver_follows_the_registry_rows \
    find='        if !crate::harness_registry::is_coupled(&harness) {' \
    replace='        if !matches!(harness.as_str(), "grok") {'
