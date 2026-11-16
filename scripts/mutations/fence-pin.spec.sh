# Sourced by scripts/verify-mutations.sh.
# bd-gmrxv: fence.sh and clean-proof --sha read the sibling pins and the
# mutation base from the FENCED revision's proof.toml. Read the working tree
# instead and another lane's dirty pin silently changes what "GREEN at <sha>"
# covers.

CARGO_ARGS=(--test fence_pinned_siblings)
TEST_FILTER=(the_fence_reads_the_fenced_revisions_pins)

mutation "P1  the fence takes the pins from the working tree" \
    file=scripts/proof-lib.sh \
    expect=the_fence_reads_the_fenced_revisions_pins \
    find='    git -C "$root" show "$rev:proof.toml" 2>/dev/null || true' \
    replace='    cat "$root/proof.toml" 2>/dev/null || true'
