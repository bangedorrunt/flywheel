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
                age=$((now - $(stat -f %m "$dir" 2>/dev/null || echo 0)))
                [ "$age" -ge "$SWEEP_AGE_SECS" ] || continue
                ;;
        esac' \
    replace='        : # bd-250g4 mutation: sweep-all, ignoring owner and age'
