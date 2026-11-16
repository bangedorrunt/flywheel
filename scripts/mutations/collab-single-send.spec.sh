# Sourced by scripts/verify-mutations.sh.
# bd-0qsc1: two defects that compounded into eleven copies of one question.
# (a) a reply threaded on the message id instead of the parent thread, so the
#     collab waiter never saw the answer; (b) the waiter resent the full ask
#     every grace window, so each miss added another copy to the peer's inbox.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_reply_inherits_the_parent_thread collab_sends_once_and_waits_without_rerings)

MAIL=src/mail.rs
CMD=src/cmd/mail.rs

mutation "Q1  a reply threads on its own message id again" \
    file=$MAIL \
    expect=a_reply_inherits_the_parent_thread \
    find='fn reply_thread(parent: Option<&Value>, message_id: &str) -> String {
    parent
        .and_then(|r| r.get("thread_id"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .unwrap_or(message_id)
        .to_string()
}' \
    replace='fn reply_thread(_parent: Option<&Value>, message_id: &str) -> String {
    message_id.to_string()
}'

mutation "Q2  collab sends the question twice per invocation" \
    file=$CMD \
    expect=collab_sends_once_and_waits_without_rerings \
    find='    let sent = mail.send(' \
    replace='    let _echo = mail.send(&key, &targets, subject, body, "urgent", true, Some(&thread_id))?;
    let sent = mail.send('
