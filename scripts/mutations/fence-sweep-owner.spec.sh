# Sourced by scripts/verify-mutations.sh.
# bd-250g4: the kept-export sweep must respect owner pins and live locks.

CARGO_ARGS=(--test fence_test_export)
TEST_FILTER=(fence_test_export_witness)

mutation "W1  the sweep deletes every fence export again" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='        case "$(basename "$dir")" in
            fence."$PIN".*) ;;
            *)
                age=$((now - $(stat -c %Y "$dir" 2>/dev/null || stat -f %m "$dir" 2>/dev/null || echo 0)))
                [ "$age" -ge "$SWEEP_AGE_SECS" ] || continue
                ;;
        esac' \
    replace='        : # bd-250g4 mutation: sweep-all, ignoring owner and age'

mutation "W2  failed proof exports are never swept" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='    for dir in "${TMPDIR:-/tmp}"/fence.* "${TMPDIR:-/tmp}"/fw-proof.*; do' \
    replace='    for dir in "${TMPDIR:-/tmp}"/fence.*; do'

mutation "W3  a failed fence never sweeps old exports" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='        sweep_kept_exports' \
    replace='        : # bd-p7oim mutation: failed fences keep every old export'

mutation "W4  a long proof's live lock is ignored" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='    [ -f "$dir/.proof-running.lock" ] && return 0' \
    replace='    : # bd-p7oim mutation: a long proof is swept while it runs'
