# Sourced by scripts/verify-mutations.sh.
# bd-yc6jf: the captain ring and idle alarm must use the pull cursor as the one
# notion of consumed, so a pulled message never pages an empty wake.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_pulled_message_never_rings_or_pages both_captain_decisions_use_the_pull_cursor)

mutation "C1  the ring read stops using the pull cursor" \
    file=src/cmd/captain.rs \
    expect=both_captain_decisions_use_the_pull_cursor \
    find='        let rows = inbox_rows(&mail, &key, Some("last"))?;' \
    replace='        let rows = inbox_rows(&mail, &key, baseline_rfc.as_deref())?;'

mutation "C2  pulled rows still count in the idle alarm" \
    file=src/cmd/captain.rs \
    expect=a_pulled_message_never_rings_or_pages \
    find='        .filter(|row| created_us(row).is_some_and(|us| us > baseline_us))
        .filter(|row| !crate::mail_wake::is_system_notice_value(row))' \
    replace='        .filter(|row| !crate::mail_wake::is_system_notice_value(row))'
