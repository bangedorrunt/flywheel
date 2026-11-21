# Sourced by scripts/verify-mutations.sh.
# bd-vk0do: a newer brief on the same bead withdraws the live one, the open
# list hides withdrawn briefs, and assign refuses a bead another pin holds.
# Dropping any fence must turn the matching test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(crew_briefs vk0do_assign_refuses)

mutation "B1  a newer brief never withdraws" \
    file=src/crew_briefs.rs \
    expect=a_newer_brief_on_the_same_bead_withdraws_the_live_one \
    find='                        if b.bead.as_deref() == Some(bead)
                            && b.closed_ts_us.is_none()
                            && b.withdrawn_ts_us.is_none()' \
    replace='                        if false'

mutation "B2  the open list shows withdrawn briefs" \
    file=src/crew_briefs.rs \
    expect=a_newer_brief_on_the_same_bead_withdraws_the_live_one \
    find='            .filter(|b| pin.is_none_or(|p| b.pin == p))
            .filter(|b| b.closed_ts_us.is_none() && b.withdrawn_ts_us.is_none())' \
    replace='            .filter(|b| pin.is_none_or(|p| b.pin == p))
            .filter(|b| b.closed_ts_us.is_none())'

mutation "B3  the gate never sees a lease" \
    file=src/main.rs \
    expect=vk0do_assign_refuses_a_bead_another_pin_holds \
    find='        if lease.reason == bead
            && !targets.iter().any(|t| t == &lease.holder)
            && lease.expires_us.is_none_or(|e| e > now_us)' \
    replace='        if false'

mutation "B4  the gate blocks only its own target" \
    file=src/main.rs \
    expect=vk0do_assign_refuses_a_bead_another_pin_holds \
    find='    if let Some((pin, id)) = live_brief
        && !targets.iter().any(|t| t == pin)' \
    replace='    if let Some((pin, id)) = live_brief
        && targets.iter().any(|t| t == pin)'
