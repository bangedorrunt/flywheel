# Sourced by scripts/verify-mutations.sh.
# bd-yc6jf: the doorbell's plane-P ring must skip mail at or below the pin's
# pull cursor — one notion of consumed with the captain watch.

CARGO_ARGS=(-p doorbell-core --lib)
TEST_FILTER=(a_pulled_mail_event_never_re_rings)

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
    find='                        kind: "already-pulled".into(),' \
    replace='                        kind: "silent-drop".into(),'
