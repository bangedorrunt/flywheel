# Sourced by scripts/verify-mutations.sh.
# bd-egvr6: a backend with no liveness probe is Unsupported, not Unknown or a
# failed probe. Classifying it otherwise makes flywheel log noise and misread
# the plane's capability.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_probe_distinguishes_unsupported_from_unknown)

mutation "U1  Unsupported liveness is not distinguished from Unknown" \
    file=src/main.rs \
    expect=the_probe_distinguishes_unsupported_from_unknown \
    find='        Err(e) if e.code() == &BackendErrorCode::Unsupported => LivenessOutcome::Unsupported,' \
    replace='        Err(e) if false => LivenessOutcome::Unsupported,'
