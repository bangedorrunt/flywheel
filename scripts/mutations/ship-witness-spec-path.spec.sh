# Sourced by scripts/verify-mutations.sh. bd-1y5qq: a bare witness spec name
# must resolve to scripts/mutations/<name> before recomputing its cache key.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_bare_spec_name_resolves_to_the_same_cache_key)

mutation "S1  a bare spec name is passed through unchanged" \
    file=src/cmd/ship.rs \
    expect=a_bare_spec_name_resolves_to_the_same_cache_key \
    find='        format!("scripts/mutations/{spec}")' \
    replace='        spec.to_string()'
