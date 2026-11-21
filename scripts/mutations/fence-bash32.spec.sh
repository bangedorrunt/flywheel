# Sourced by scripts/verify-mutations.sh.
# bd-250g4: the fence's changed-module walk must run on bash 3.2.

CARGO_ARGS=(--test fence_test_export)
TEST_FILTER=(fence_test_export_witness)

mutation "B1  restore mapfile (bash 4+ only)" \
    file=scripts/fence.sh \
    expect=fence_test_export_witness \
    find='        while IFS= read -r line; do
            [ -n "$line" ] && files+=("$line")
        done < <(git -C "$REPO" diff --cached --name-only --diff-filter=ACM -- '"'"'*.rs'"'"')' \
    replace='        mapfile -t files < <(git -C "$REPO" diff --cached --name-only --diff-filter=ACM -- '"'"'*.rs'"'"')'
