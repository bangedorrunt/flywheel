# Sourced by scripts/verify-mutations.sh.
# bd-4bxtl: a toron daemon that refuses connections must fail stack status
# --check. The reachability term is the whole fix — ignoring it must go RED.

CARGO_ARGS=(-p flywheel --test stack_status_reachability)
TEST_FILTER=(stack_check_fails_when_the_toron_address_refuses_connections)

mutation "R1  stack check ignores daemon reachability" \
    file=src/cmd/stack.rs \
    expect=stack_check_fails_when_the_toron_address_refuses_connections \
    find=$'        || toron_restart_required\n        || !toron_reachable_now;' \
    replace=$'        || toron_restart_required\n        || false;'
