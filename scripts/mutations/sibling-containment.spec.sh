# Sourced by scripts/verify-mutations.sh.
# bd-9i0e3: --repo containment uses the pinned sibling rev, not flywheel history.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    sibling_containment_uses_the_pinned_sibling_rev
)

mutation "S1  containment reverts to flywheel history" \
    file=src/cmd/ship.rs \
    expect=sibling_containment_uses_the_pinned_sibling_rev \
    find='                    sib.to_path_buf(),
                    rev.clone(),
                    format!(" pins sibling rev {rev}"),' \
    replace='                    root.to_path_buf(),
                    proved.to_string(),
                    String::new(),'
