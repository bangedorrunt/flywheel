#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-0uzrq.20: an unroutable pin's rows must not be refetched every sweep; the
# pin is probed once per backoff window and its row set is reported once.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(bd20_)

mutation "B1  the unroutable pin is fetched every sweep again" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=bd20_unroutable_pin_is_fetched_once_per_backoff_window \
    find='            if !self.host.has_route(&pin) {' \
    replace='            if false && !self.host.has_route(&pin) {'

mutation "B2  the same row set is reported on every probe" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=bd20_unroutable_pin_is_fetched_once_per_backoff_window \
    find='            if self.unroutable_reported.get(&pin) != Some(&rows_digest) {' \
    replace='            if true {'

mutation "B3  the report loses its unroutable type" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=bd20_unroutable_pin_is_fetched_once_per_backoff_window \
    find='                    kind: "unroutable".into(),' \
    replace='                    kind: "already_rung".into(),'

mutation "B4  the backoff is applied to routable pins too" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=bd20_routable_pin_still_fetches_every_sweep \
    find='            if !self.host.has_route(&pin) {' \
    replace='            if true {'
