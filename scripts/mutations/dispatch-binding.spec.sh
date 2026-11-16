# Sourced by scripts/verify-mutations.sh.
# bd-6setu.2 (V10): a review step's authors are the pins the journal bound to
# the steps its stage reads, in the same run. B1 lets the binder borrow the
# author; B2 stops the scoped pane dispatch writing the row at all.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(
    review_bind_refuses_every_journal_author
    only_the_scoped_pane_dispatch_writes_a_binding
)

PANE=src/pane.rs

mutation "B1  the binder borrows the author the journal names" \
    file=$PANE \
    expect=review_bind_refuses_every_journal_author \
    find='    let mut authors: std::collections::BTreeSet<String> = journal_authors.clone();' \
    replace='    let mut authors: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();'

mutation "B2  the scoped pane dispatch writes no binding row" \
    file=$PANE \
    expect=only_the_scoped_pane_dispatch_writes_a_binding \
    find='        crate::dispatch_bindings::append(
            project,
            ctx.scope.run_id,
            ctx.scope.node_path,
            step,
            &pin,
        )?;' \
    replace='        let _ = (project, ctx.scope.run_id, ctx.scope.node_path, step, pin);'
