# Sourced by scripts/verify-mutations.sh.
# bd-b3j7x: the proof_steps witnesses must never reach the live toron daemon.
# Dropping the fake toron makes the red-mail assertion fail while AGENT_NAME
# and TORON_MCP_URL stay cleared, so no live mail is sent.

CARGO_ARGS=(--test proof_steps)
TEST_FILTER=(a_stale_lockfile_fails_the_proof_build_step)

mutation "P1  proof_steps sandbox drops the fake toron" \
    file=tests/proof_steps.rs \
    expect=a_stale_lockfile_fails_the_proof_build_step \
    find='let fake = sandbox.bin_dir().join("toron");' \
    replace='let fake = sandbox.bin_dir().join("toron-disabled");'
