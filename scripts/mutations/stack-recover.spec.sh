# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.9: absent legacy :18980 daemon means no stale hosts (success).

CARGO_ARGS=(-p flywheel --test stack_recover)
TEST_FILTER=(stack_recover_script_cases)

mutation "R1  absent legacy daemon treated as failure" \
    file=scripts/stack.sh \
    expect=stack_recover_script_cases \
    find=$'  # ADR-0020: the legacy :18980 doorbell daemon is retired. If it is absent,\n  # there are no stale legacy hosts to replace — that is success, not failure.\n  health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)" || {\n    echo "no legacy doorbell daemon; nothing to replace"\n    return 0\n  }\n' \
    replace=$'  # ADR-0020: the legacy :18980 doorbell daemon is retired. If it is absent,\n  # there are no stale legacy hosts to replace — that is success, not failure.\n  health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)" || {\n    echo "no legacy doorbell daemon; nothing to replace"\n    return 1\n  }\n'
