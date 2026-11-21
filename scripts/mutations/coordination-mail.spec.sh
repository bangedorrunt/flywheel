# Sourced by scripts/verify-mutations.sh. bd-v7ucf: a coordination warning
# must reach the holder as mail, with a captain copy and a dedupe window.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(a_foreign_lease_mails_the_holder_once_and_copies_captain a_coordination_notice_is_repeated_after_the_window)

mutation "V1  the lease refusal never mails the holder" \
    file=src/cmd/commit.rs \
    expect=a_foreign_lease_mails_the_holder_once_and_copies_captain \
    find=$'        if let Some(coord) = &ctx.coord {\n            notify_holder(coord, holder, paths);\n        }' \
    replace='        let _ = &ctx.coord;'

mutation "V2  no copy to the captain" \
    file=src/cmd/commit.rs \
    expect=a_foreign_lease_mails_the_holder_once_and_copies_captain \
    find='    for to in [holder, coord.captain.as_str()] {' \
    replace='    for to in [holder] {'

mutation "V3  the dedupe window is ignored" \
    file=src/cmd/commit.rs \
    expect=a_foreign_lease_mails_the_holder_once_and_copies_captain \
    find=$'    if seen\n        .iter()\n        .any(|(k, ts)| k == &key && coord.now_secs.saturating_sub(*ts) < NOTICE_WINDOW_SECS)\n    {\n        return;\n    }' \
    replace='    let _ = &seen;'
