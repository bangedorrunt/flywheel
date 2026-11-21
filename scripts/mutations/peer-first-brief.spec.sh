# Sourced by scripts/verify-mutations.sh. bd-pezon: a brief names the peer
# holding each reserved path and asks the crew to mail them first.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(peer_section_names_holders_and_skips_self)

mutation "P1  the brief drops every peer contact" \
    file=src/durable.rs \
    expect=peer_section_names_holders_and_skips_self \
    find='            if holder == me || path.is_empty() {' \
    replace='            if true {'

mutation "P2  the peer notice stops naming the mail-first step" \
    file=crates/doorbell-core/src/agent_text.rs \
    expect=peer_section_names_holders_and_skips_self \
    find='        "{holder} holds {}: mail {holder} first (flywheel collab), and copy the captain only if they stay silent or the shared build breaks",' \
    replace='        "{holder} holds {}: tell the captain",'
