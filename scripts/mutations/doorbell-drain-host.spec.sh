#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-ba4nl: the host-level fake-toron drain test must prove the ask AND the
# answer path; break either and the drop case goes RED.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(host_drain_)

mutation "H1  the drain parks the queue but never asks toron" \
    file=crates/doorbell-core/src/lib.rs \
    expect=host_drain_read_past_the_row_drops_the_mail_wake \
    find='                    if next.cursor_inflight.insert(pin.clone()) {
                        actions.push(DoorbellAction::ReadToronCursor { pin: pin.clone() });
                    }
                    next.draining.insert(' \
    replace='                    let _ = next.cursor_inflight.insert(pin.clone());
                    next.draining.insert('

mutation "H2  the brain journals the drain ask but never answers it" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=host_drain_read_past_the_row_drops_the_mail_wake \
    find='if let Some(cursor_us) = self.host.read_toron_cursor(pin) {' \
    replace='if let Some(cursor_us) = self.host.read_toron_cursor(pin).filter(|_| false) {'
