# Sourced by scripts/verify-mutations.sh. bd-pezon: a brief names the peer
# holding each reserved path and asks the crew to mail them first.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(peer_section_names_holders_and_skips_self a_brief_skips_the_target_pin_not_the_builder)

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

mutation "P3  a brief skips the builder pin instead of the target pin" \
    file=src/durable.rs \
    expect=a_brief_skips_the_target_pin_not_the_builder \
    find='.replace("{peers}", &render_peer_contacts(board, pin))' \
    replace='.replace("{peers}", &render_peer_contacts(board, &std::env::var("AGENT_NAME").unwrap_or_default()))'
