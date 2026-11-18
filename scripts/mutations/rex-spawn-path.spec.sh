# Sourced by scripts/verify-mutations.sh.
# bd-xpuky: a rex server started by launchd has PATH=/usr/bin:/bin:... and
# cannot resolve a mise-installed harness. Dropping PATH from the shared pane
# env makes every rex spawn (and hc split) launch with the server's PATH.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(env_carries_the_spawners_path_for_both_planes)

mutation "P1  the launch env forgets the spawner PATH" \
    file=src/hc.rs \
    expect=env_carries_the_spawners_path_for_both_planes \
    find='        "PATH",' \
    replace='        "NOTPATH",'
