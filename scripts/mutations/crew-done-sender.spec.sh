# Sourced by scripts/verify-mutations.sh.
# bd-9nck5: crew done's plane-P fallback sent from the BRIEF ID, which has no
# mail key, so the captain heard nothing after a pane wake failed. The sender
# must come from the identity ladder (--as, AGENT_NAME, then the brief's pin).

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_done_fallback_sender_uses_the_identity_ladder)

mutation "S1  the fallback sends from the brief id again" \
    file=src/cmd/crew.rs \
    expect=the_done_fallback_sender_uses_the_identity_ladder \
    find='    let reporter = crate::authority::select_agent_identity(r#as, &|k| std::env::var(k).ok());' \
    replace='    let reporter = Some(brief.to_string());'
