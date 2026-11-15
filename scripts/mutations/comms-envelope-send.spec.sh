# Sourced by scripts/verify-mutations.sh.
# A recorded intent whose epoch is still current must resend the same msg id,
# never mint a fresh one (bd-ksbtp / comms.envelope-send).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(comms_send::tests::a_retry_resends_the_recorded_msg_id_and_never_mints_twice)

mutation "E1  current-epoch resend mints a fresh msg id" \
    file=src/comms_send.rs \
    expect=comms_send::tests::a_retry_resends_the_recorded_msg_id_and_never_mints_twice \
    find=$'        (Some(env), None) if store_epoch.is_none_or(|cur| env.msg.epoch >= cur) => {\n            SendPlan::Resend(env)\n        }' \
    replace=$'        (Some(env), None) if store_epoch.is_none_or(|cur| env.msg.epoch > cur) => {\n            SendPlan::Resend(env)\n        }'
