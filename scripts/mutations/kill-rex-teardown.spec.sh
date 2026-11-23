# Sourced by scripts/verify-mutations.sh. bd-t38jb: kill teardown must ask rex
# for the one close scope it supports. A narrow close is refused before any
# effect, leaving registry rows and the session behind.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(kill_teardown_closes_the_whole_rex_session)

mutation "K1  kill asks rex for a narrow close it refuses" \
    file=src/main.rs \
    expect=kill_teardown_closes_the_whole_rex_session \
    find='backend.workspace_close(ws_id, true)?;' \
    replace='backend.workspace_close(ws_id, false)?;'
