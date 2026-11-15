# Sourced by scripts/verify-mutations.sh.
# A system notice never rings a pane (bd-ohxou, bd-271ws).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(reservation_revoked_notice_never_rings)

mutation "W1  every notice is treated as mail" \
    file=src/mail_wake.rs \
    expect=reservation_revoked_notice_never_rings \
    find='    kind.is_some() || (subject.trim().is_empty() && looks_like_key(sender))' \
    replace='    false'
