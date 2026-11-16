# Sourced by scripts/verify-mutations.sh.
# bd-6setu.16: H2 the binding store fails CLOSED and is durable before the
# prompt; H3 the reads are data; M1 the witness is execution, not source order.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(a_torn_row_refuses_instead_of_guessing the_binding_row_is_durable_before_the_prompt_port_runs the_reads_map_decides_the_authors_at_dispatch)

STORE=src/dispatch_bindings.rs
PANE=src/pane.rs

mutation "H2  a torn row answers empty again" \
    file=$STORE \
    expect=a_torn_row_refuses_instead_of_guessing \
    find='        let row = serde_json::from_str::<BindingRow>(line).with_context(|| {
            format!(
                "dispatch bindings torn at {} line {}: refusing to guess the author",
                bindings_file(project).display(),
                n + 1
            )
        })?;
        rows.push(row);' \
    replace='        if let Ok(row) = serde_json::from_str::<BindingRow>(line) {
            rows.push(row);
        }'

mutation "M1  the row is written after the prompt instead of before" \
    file=$PANE \
    expect=the_binding_row_is_durable_before_the_prompt_port_runs \
    find='        crate::dispatch_bindings::append(
            project,
            ctx.scope.run_id,
            ctx.scope.node_path,
            step,
            &pin,
        )?;' \
    replace='        let deferred = (project, ctx.scope.run_id, ctx.scope.node_path, step.clone(), pin.clone());'

mutation "H3  the dispatch ignores the reads map it was handed" \
    file=$PANE \
    expect=the_reads_map_decides_the_authors_at_dispatch \
    find='                .and_then(|stage| ctx.reads.get(stage).cloned())' \
    replace='                .and_then(|_stage| Some(Vec::<String>::new()))'
