# Sourced by scripts/verify-mutations.sh.
# bd-yc6jf: the doorbell's plane-P ring must skip mail at or below the pin's
# pull cursor — one notion of consumed with the captain watch.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(a_pulled_mail_event_never_re_rings a_cli_pull_at_or_after_created_at_suppresses_the_next_ring toron_cursor_stale_answer_rings_instead_of_reasking)

mutation "D1  the doorbell rings pulled mail again" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_pulled_mail_event_never_re_rings \
    find='                    if fresh || decide_only {' \
    replace='                    if false {'

mutation "D2  the skipped pulled wake is silent" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_pulled_mail_event_never_re_rings \
    find='                                    kind: "already-pulled".into(),
                                    pin: pin.clone(),
                                    detail: format!(
                                        "mail at {created_us:?} is at or below the toron cursor"
                                    ),' \
    replace='                                    kind: "silent-drop".into(),
                                    pin: pin.clone(),
                                    detail: format!(
                                        "mail at {created_us:?} is at or below the toron cursor"
                                    ),'

mutation "D3  the toron-covered mail rings anyway" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_cli_pull_at_or_after_created_at_suppresses_the_next_ring \
    find='                            Some(tc) if us <= tc => {' \
    replace='                            Some(_) if false => {'

mutation "D4  the stale answer parks behind a second ask" \
    file=crates/doorbell-core/src/lib.rs \
    expect=toron_cursor_stale_answer_rings_instead_of_reasking \
    find='                let (m, a) = tick(&next, &DoorbellMsg::WatchDecideOnly { line });' \
    replace='                let (m, a) = tick(&next, &DoorbellMsg::WatchNdjson { line });'

