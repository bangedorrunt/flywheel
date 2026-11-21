# Sourced by scripts/verify-mutations.sh.
# bd-6qae0: a blocked pane is never a typing target.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(
    blocked_to_idle_still_drains_the_queue
    inject_gate_blocked_never_types_even_when_urgent
)

mutation "B1  blocked becomes injectable again" \
    file=crates/doorbell-core/src/lib.rs \
    expect=blocked_to_idle_still_drains_the_queue \
    find='    matches!(plane_status, "idle" | "done")' \
    replace='    matches!(plane_status, "idle" | "done" | "blocked")'

mutation "B2  a blocked pane is typed into when urgent" \
    file=crates/doorbell-core/src/wake_core.rs \
    expect=inject_gate_blocked_never_types_even_when_urgent \
    find='        (PresenceVerdict::Blocked, _) => {
            InjectGate::Digest("blocked pane has no typing target; mail held for digest")
        }' \
    replace='        (PresenceVerdict::Blocked, _) => InjectGate::Inject,'
