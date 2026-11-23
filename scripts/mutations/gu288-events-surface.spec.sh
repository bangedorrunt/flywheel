#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# The events client must speak toron's real subscribe/poll surface.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(real_surface)

mutation "G1  subscribe reverts to the string delivery shape" \
    file=src/events_client.rs \
    expect=real_surface_stream_feeds_one_typed_urgent_notice_to_the_plugin \
    find='                "arguments": {"ttl_us": self.ttl_us},
                "delivery": {"mode": "stream"},' \
    replace='                "arguments": {},
                "delivery": "stream",
                "ttl_us": self.ttl_us,'

mutation "G2  the server refreshBefore field is ignored" \
    file=src/events_client.rs \
    expect=real_surface_poll_fallback_feeds_one_typed_urgent_notice \
    find='v.get("refreshBefore")' \
    replace='v.get("never_matches")'
