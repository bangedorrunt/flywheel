# Sourced by scripts/verify-mutations.sh.
# bd-6setu.12 (ADR-0022 acceptance 5): a classified 429 permits the hop. Drop it
# from the permitted kinds and the 429 no longer respawns - the fallback the
# acceptance promises silently stops existing.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(acceptance5_429_respawn_journals_one_rebind_intent_and_keeps_the_pin)

mutation "A5  a classified 429 no longer permits a hop" \
    file=src/agent_ref.rs \
    expect=acceptance5_429_respawn_journals_one_rebind_intent_and_keeps_the_pin \
    find='                | FailureKind::RateLimit' \
    replace='                // mutation: a 429 no longer permits a hop'
