# Sourced by scripts/verify-mutations.sh.
# bd-ndbnv / bd-97uwg: the memory lesson loop's load-bearing decisions.
# Each mutant must turn its named memory_loop test RED.

CARGO_ARGS=(-p flywheel --test memory_loop)
TEST_FILTER=(
    migrated_lessons_come_back_through_check_trap_alone
    malformed_check_trap_output_is_a_loud_failure
    repeated_lessons_are_deduplicated
    recording_with_as_is_retrievable
    no_test_shells_the_real_chie
)

MEM=src/cmd/memory.rs

mutation "M1  the trap read goes through recall instead of check-trap" \
    file=$MEM \
    expect=migrated_lessons_come_back_through_check_trap_alone \
    find='    cmd.args(["memory", "check-trap"]);' \
    replace='    cmd.args(["memory", "recall"]);'

mutation "M2  malformed check-trap JSON silently becomes empty" \
    file=$MEM \
    expect=malformed_check_trap_output_is_a_loud_failure \
    find='            Err(e) => CallOutcome::Failed(format!("chie check-trap returned invalid JSON: {e}")),' \
    replace='            Err(_) => CallOutcome::Found(Vec::new()),'

mutation "M3  repeated lessons are not deduplicated" \
    file=$MEM \
    expect=repeated_lessons_are_deduplicated \
    find='    seen.insert(lesson_core(raw))' \
    replace='    true || seen.insert(lesson_core(raw))'

mutation "M4  record-lesson sends the pin as the authoring peer" \
    file=$MEM \
    expect=recording_with_as_is_retrievable \
    find='        args.push("--pin".to_string());' \
    replace='        args.push("--peer".to_string());'

mutation "N1  an inline memory test drops the fake chie" \
    file=tests/memory_loop.rs \
    expect=no_test_shells_the_real_chie \
    find='.env("FLYWHEEL_CHIE_BIN", sandbox.bin_dir().join("fake-chie"))' \
    replace='.env("FLYWHEEL_UNUSED", "x")'
