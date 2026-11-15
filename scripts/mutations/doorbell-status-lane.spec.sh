# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.17, part 2: plane statuses ride their own lane, polled before mail.
# Live 16:16-16:19Z a rex record change took 76-155s to reach the journal
# because the status queued behind mail-bound dispatches on the one channel, and
# the idle edge it carried dragged every deferred ring behind it.
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-status-lane.spec.sh --cargo mbx

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(status_lane plane_status_rides)

BRAIN=crates/doorbell-rt/src/brain.rs
HOST=src/doorbell_host.rs

mutation "L1  the status branch is polled after mail, so a backlog wins" \
    file=$BRAIN \
    expect=a_status_lane_overtakes_mail_already_queued \
    find='                msg = async {
                    match status_rx.as_mut() {' \
    replace='                msg = async {
                    tokio::task::yield_now().await;
                    match status_rx.as_mut() {'
