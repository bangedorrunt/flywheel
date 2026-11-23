#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-cursor-reask-dropped-15ail: a re-ask from a cursor answer must be
# executed (and bounded), not dropped behind cursor_inflight forever.

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(toron_cursor_reask)

mutation "S1  a re-ask from the answer is dropped again" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=toron_cursor_reask_while_answering_is_executed \
    find='            answer = self
                .answer_cursor_reads(&effects)
                .into_iter()' \
    replace='            answer = self
                .answer_cursor_reads(&[])
                .into_iter()'

mutation "S2  the re-ask loop has no bound" \
    file=crates/doorbell-rt/src/brain.rs \
    expect=toron_cursor_reask_loop_is_bounded_and_loud \
    find='rounds >= 2' \
    replace='rounds >= u8::MAX'
