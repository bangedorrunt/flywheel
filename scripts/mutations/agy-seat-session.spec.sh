# Sourced by scripts/verify-mutations.sh.
# bd-edva3: the conversation id is learned from the seat, never the spawner.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    agy_conversation_id_never_read_from_spawner_env
    conversation_id_comes_from_the_hook_payload
)

mutation "A1  spawn reads the conversation id from the spawner env" \
    file=src/cmd/spawn.rs \
    expect=agy_conversation_id_never_read_from_spawner_env \
    find='    let crate::harness_registry::SessionResume::ChosenId { start, .. } =
        crate::harness_registry::session_resume(kind)
    else {
        return None;
    };' \
    replace='    if crate::harness_registry::canonical(kind) == "antigravity" {
        return std::env::var("ANTIGRAVITY_CONVERSATION_ID").ok();
    }
    let crate::harness_registry::SessionResume::ChosenId { start, .. } =
        crate::harness_registry::session_resume(kind)
    else {
        return None;
    };'

mutation "A2  the hook payload parser never finds a conversation id" \
    file=src/turn_hooks.rs \
    expect=conversation_id_comes_from_the_hook_payload \
    find='    for key in ["conversationId", "conversation_id", "conversation"] {' \
    replace='    for key in [] as [&str; 0] {'
