# Sourced by scripts/verify-mutations.sh.
# bd-ic40o: a host keyed by session ID stays present after the session is renamed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    renamed_session_is_still_present_for_an_id_keyed_host
)

mutation "P0  presence matches labels only" \
    file=src/doorbell.rs \
    expect=renamed_session_is_still_present_for_an_id_keyed_host \
    find='|| ws_id.strip_prefix("session:").unwrap_or(ws_id) == id' \
    replace='|| false'

mutation "P0  bare IDs never match" \
    file=src/doorbell.rs \
    expect=renamed_session_is_still_present_for_an_id_keyed_host \
    find='|| ws_id.strip_prefix("session:").unwrap_or(ws_id) == id' \
    replace='|| *ws_id == workspace'
