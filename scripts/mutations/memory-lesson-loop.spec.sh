# Sourced by scripts/verify-mutations.sh.
# bd-ndbnv / bd-97uwg: the memory lesson loop's four load-bearing decisions.
# Each mutant must turn its named memory_loop test RED.

CARGO_ARGS=(-p flywheel --test memory_loop)
TEST_FILTER=(
    migrated_lessons_are_check_trap_findable_by_natural_query
    malformed_recall_output_is_a_loud_failure
    recalled_lessons_are_deduplicated
    recording_with_as_is_retrievable
)

MEM=src/cmd/memory.rs

mutation "M1  check-trap does not recall Rule/Trap lessons" \
    file=$MEM \
    expect=migrated_lessons_are_check_trap_findable_by_natural_query \
    find='        for line in lesson_recall(q, project_dir)? {
            let line = normalize_line(&line);
            if !line.is_empty() {
                out.insert(line.chars().take(240).collect());
            }
        }' \
    replace='        let _ = lesson_recall(q, project_dir)?;'

mutation "M2  malformed recall JSON silently becomes empty" \
    file=$MEM \
    expect=malformed_recall_output_is_a_loud_failure \
    find='        .map_err(|e| anyhow::anyhow!("chie recall returned invalid JSON: {e}"))?;' \
    replace='        .unwrap_or_default();'

mutation "M3  recalled lessons are not deduplicated" \
    file=$MEM \
    expect=recalled_lessons_are_deduplicated \
    find='    seen.insert(lesson_core(raw))' \
    replace='    true || seen.insert(lesson_core(raw))'

mutation "M4  record-lesson passes chie the invalid --pin flag" \
    file=$MEM \
    expect=recording_with_as_is_retrievable \
    find='        cmd.args(["--peer", &format!("agent:{p}")]);' \
    replace='        cmd.args(["--pin", p]);'
