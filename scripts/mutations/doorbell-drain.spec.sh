# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.23: a mid-less mail wake must not ring (or queue) on its own; the
# inbox fetch carries the message id and owns that delivery.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(an_unidentified_mail_wake_is_left_to_the_inbox_fetch)

mutation "M1  mid-less mail wake rings anyway" \
    file=crates/doorbell-core/src/lib.rs \
    expect=an_unidentified_mail_wake_is_left_to_the_inbox_fetch \
    find='let unidentified_mail = mail && message_id.is_none();' \
    replace='let unidentified_mail = false;'
