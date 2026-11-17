# Sourced by scripts/verify-mutations.sh.
# bd-c48to: a per-path reservation-release notice is transport chatter, never
# unread work. Drop the subject rule and the 30 notices bury the brief, which
# pages the captain a second time for nothing.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(thirty_release_notices_and_one_brief_page_only_on_the_brief)

mutation "R1  release notices count as unread work again" \
    file=src/mail_wake.rs \
    expect=thirty_release_notices_and_one_brief_page_only_on_the_brief \
    find='    subject.trim_start().starts_with("reservation released:")' \
    replace='    subject.trim_start().starts_with("reservation released: (never)")'
