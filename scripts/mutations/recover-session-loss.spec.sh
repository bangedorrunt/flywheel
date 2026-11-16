# Sourced by scripts/verify-mutations.sh.
# After a host blackout no owned workspace survives: recover must recreate the
# swarm workspace and respawn every gone non-captain pin (bd-0uzrq.21).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(recover_session_loss_tests)

mutation "R1  recover refuses when no workspace survived" \
    file=src/main.rs \
    expect=recover_session_loss_tests::recover_after_session_loss_respawns_every_gone_crew_pin \
    find='            return recreate("no swarm workspace survived".to_string());' \
    replace='            return Err(e);'

mutation "R2  a gone crew pin is skipped instead of respawned" \
    file=src/main.rs \
    expect=recover_session_loss_tests::recover_after_session_loss_respawns_every_gone_crew_pin \
    find='            BindingState::Gone => RecoverDecision::Respawn {
                reason: "agent-gone",
            },' \
    replace='            BindingState::Gone => RecoverDecision::Healthy,'
