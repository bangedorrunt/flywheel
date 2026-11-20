# Sourced by scripts/verify-mutations.sh.
# bd-5j8ri: the mutation harness must export pinned siblings, never link the
# live dirty ../toron, or another lane's WIP changes a flywheel mutant verdict.

CARGO_ARGS=(-p flywheel --test proof_steps)
TEST_FILTER=(proof_verify_mutations_exports_pinned_siblings)

mutation "P1  sibling export links the live tree again" \
    file=scripts/verify-mutations.sh \
    expect=proof_verify_mutations_exports_pinned_siblings \
    find='        proof_archive "$sib_repo" "$pin" "$export_root/$sib_name" || {
            echo "verify-mutations: cannot archive sibling $rel at $pin" >&2
            exit 2
        }' \
    replace='        ln -sfn "$sib_repo" "$export_root/$sib_name"'
