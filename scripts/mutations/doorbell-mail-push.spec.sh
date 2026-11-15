# Sourced by scripts/verify-mutations.sh.
# A mail-delivered push event must ring an idle pin at once (bd-0uzrq.18).

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(mail_delivered_watch_rings_once_then_poll_dedups)

mutation "M1  mail-delivered push mapping dropped" \
    file=crates/doorbell-core/src/lib.rs \
    expect=mail_delivered_watch_rings_once_then_poll_dedups \
    find=$'    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {\n        return Some(EventClass::PlanePMail);\n    }\n' \
    replace=$'    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {\n        return None;\n    }\n'
