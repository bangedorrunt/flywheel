# Sourced by scripts/verify-mutations.sh.
# The journal lock is real: a second appender WAITS for it (bd-ksbtp).

CARGO_ARGS=(-p doorbell-rt --lib)
TEST_FILTER=(append_waits_for_the_journal_lock_then_lands_exactly_once)

mutation "D2  a held lock does not make the appender wait" \
    file=crates/doorbell-rt/src/store.rs \
    expect=append_waits_for_the_journal_lock_then_lands_exactly_once \
    find=$'                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {\n                    std::thread::sleep(std::time::Duration::from_millis(2));\n                }' \
    replace=$'                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {\n                    return Ok(f(&file));\n                }'

mutation "D3  the flock is never taken (append and rotation race)" \
    file=crates/doorbell-rt/src/store.rs \
    expect=concurrent_appends_lose_no_rows \
    find='            match file.try_lock() {' \
    replace='            match Ok::<(), std::fs::TryLockError>(()) {'
