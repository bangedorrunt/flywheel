# Sourced by scripts/verify-mutations.sh.
# bd-sfxu1: a pushed program_status_changed must become a typed status without
# a poll, and an OSC error must never be mapped to done/idle.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(an_osc_error_event_is_reported_as_error_never_idle_or_done)

HOST=src/doorbell_host.rs

mutation "S2  an OSC error is reported as idle" \
    file=$HOST \
    expect=an_osc_error_event_is_reported_as_error_never_idle_or_done \
    find='                    Some("error") => "error".to_string(),' \
    replace='                    Some("error") => "idle".to_string(),'
