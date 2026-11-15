# Sourced by scripts/verify-mutations.sh.
#
# bd-0uzrq.17, part 2, the producer side: the plane poll must put a status on
# the status lane when the runner provided one. Sending it on the mail lane is
# the 76-155s lag itself (live 16:16-16:19Z).
#
#   scripts/verify-mutations.sh scripts/mutations/doorbell-status-lane-host.spec.sh --cargo mbx

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(plane_status_rides)

HOST=src/doorbell_host.rs

mutation "L2  the plane poll keeps sending statuses on the mail lane" \
    file=$HOST \
    expect=a_plane_status_rides_the_status_lane_when_one_is_set \
    find='            Some(tx) => tx.clone(),' \
    replace='            Some(_tx) => self.brain_tx.clone(),'
