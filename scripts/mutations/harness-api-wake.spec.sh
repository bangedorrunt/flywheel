# Sourced by scripts/verify-mutations.sh.
# bd-f888z: the API-wake decisions that must fail if changed.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    opencode_wake_rides_the_session_api
    api_route_delivers_without_typing_and_picks_delivery
    api_failure_falls_back_to_inject
    msg_id_is_stable_and_idempotent
    opencode_session_minted_into_argv_and_recorded
)

mutation "H1  opencode never selects the API route" \
    file=src/harness_caps.rs \
    expect=opencode_wake_rides_the_session_api \
    find='    let api_ok = r.is_some_and(|row| row.api.supported) && api_session_valid(kind, session);' \
    replace='    let api_ok = false;'

mutation "H2  API prompt types into the pane instead" \
    file=src/harness_api.rs \
    expect=api_route_delivers_without_typing_and_picks_delivery \
    find='            match api.prompt(session, text, delivery, id) {
                Ok(()) => Ok(RouteTaken::Api),' \
    replace='            match api.prompt(session, text, delivery, id) {
                Ok(()) => { inject()?; Ok(RouteTaken::Inject) },'

mutation "H3  API failure does not fall back to Inject" \
    file=src/harness_api.rs \
    expect=api_failure_falls_back_to_inject \
    find='                Err(_) => {
                    inject()?;
                    Ok(RouteTaken::Inject)
                }' \
    replace='                Err(e) => return Err(e),'

mutation "H4  msg id is not stable (hash dropped)" \
    file=src/harness_api.rs \
    expect=msg_id_is_stable_and_idempotent \
    find='        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);' \
    replace='        let _ = b;'

mutation "H5  spawn mints no opencode session" \
    file=src/cmd/spawn.rs \
    expect=opencode_session_minted_into_argv_and_recorded \
    find='    if canonical != "opencode" {
        return None;
    }' \
    replace='    return None;'
