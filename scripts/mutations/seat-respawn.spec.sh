# Sourced by scripts/verify-mutations.sh.
#
# bd-fbiza (5): a Respawn through the one seat interpreter is journal-first,
# replayable under the same effect key, and lands one holder.
#
#   scripts/verify-mutations.sh scripts/mutations/seat-respawn.spec.sh --cargo mbx
#
# What this does not prove: the live pane order inside
# cmd::internal::respawn_binding (clear_pane_identity then write_pane_identity),
# which the fake models rather than runs; the live leg covers that end to end.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(respawn_replays_one_effect_key_and_never_doubles_a_holder)

AGENT_REF=src/agent_ref.rs
HOST=src/supervisor_host.rs

mutation "R1  the rebind journals no Prepared row before the provider" \
    file=$AGENT_REF \
    expect=respawn_replays_one_effect_key_and_never_doubles_a_holder \
    find=$'    intent.state = EffectState::Prepared;\n    journal_rebind(project, &intent)?;\n    intent.state = EffectState::InFlight;\n    journal_rebind(project, &intent)?;\n    match apply(&intent) {\n        Ok(Some(v)) => {' \
    replace=$'    intent.state = EffectState::Prepared;\n    intent.state = EffectState::InFlight;\n    journal_rebind(project, &intent)?;\n    match apply(&intent) {\n        Ok(Some(v)) => {'

mutation "R2  a landed rebind is not Settled" \
    file=$AGENT_REF \
    expect=respawn_replays_one_effect_key_and_never_doubles_a_holder \
    find=$'        Ok(Some(v)) => {\n            intent.state = EffectState::Settled;' \
    replace=$'        Ok(Some(v)) => {\n            intent.state = EffectState::Unknown;'

mutation "R3  a restarted host never takes over its dead predecessor's claim" \
    file=$HOST \
    expect=respawn_replays_one_effect_key_and_never_doubles_a_holder \
    find='Some(theirs) if takes_over(&self.owner, theirs) => match claim(theirs) {' \
    replace='Some(theirs) if false && takes_over(&self.owner, theirs) => match claim(theirs) {'
