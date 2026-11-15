# Sourced by scripts/verify-mutations.sh.
# A heartbeat within the ceiling means Quiet (pipe alive, no events), not Live
# (bd-ksbtp / wake-liveness.health).

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(wake_liveness::tests::quiet_when_heartbeat_inside_ceiling)

mutation "L1  quiet heartbeat classified as live" \
    file=crates/doorbell-rt/src/wake_liveness.rs \
    expect=wake_liveness::tests::quiet_when_heartbeat_inside_ceiling \
    find=$'    if let Some(hb) = last_heartbeat_ms {\n        let age = now_ms.saturating_sub(hb);\n        if age <= deaf_ceiling_ms {\n            return PipeHealth::Quiet {\n                last_heartbeat_ms: hb,\n            };\n        }\n    }' \
    replace=$'    if let Some(hb) = last_heartbeat_ms {\n        let age = now_ms.saturating_sub(hb);\n        if age <= deaf_ceiling_ms {\n            return PipeHealth::Live {\n                last_event_ms: hb,\n            };\n        }\n    }'
