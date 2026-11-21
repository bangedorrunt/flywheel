# Sourced by scripts/verify-mutations.sh.
# bd-yc6jf: the doorbell's plane-P ring must skip mail at or below the pin's
# pull cursor — one notion of consumed with the captain watch.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(a_pulled_mail_event_never_re_rings a_cli_pull_at_or_after_created_at_suppresses_the_next_ring)

mutation "D1  the doorbell rings pulled mail again" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_pulled_mail_event_never_re_rings \
    find='                if mail && created_us.is_some_and(|us| already_pulled(&next.pulled_us, &pin, us)) {' \
    replace='                if false
                    && created_us.is_some_and(|us| already_pulled(&next.pulled_us, &pin, us))
                {'

mutation "D2  the skipped pulled wake is silent" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_pulled_mail_event_never_re_rings \
    find='                        kind: "already-pulled".into(),
                        pin: pin.clone(),
                        detail: format!("mail at {created_us:?} is at or below the pull cursor"),' \
    replace='                        kind: "silent-drop".into(),
                        pin: pin.clone(),
                        detail: format!("mail at {created_us:?} is at or below the pull cursor"),'

mutation "D3  the toron-covered mail rings anyway" \
    file=crates/doorbell-core/src/lib.rs \
    expect=a_cli_pull_at_or_after_created_at_suppresses_the_next_ring \
    find='                            Some(tc) if us <= tc => {' \
    replace='                                Some(_) if false => {'

