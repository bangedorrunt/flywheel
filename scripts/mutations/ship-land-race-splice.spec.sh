# Sourced by scripts/verify-mutations.sh.
# bd-kz3sc rework 2: the splice must be recomputed on each CAS retry against the
# moved parent, or a neighbour that lands in between is silently reverted.

CARGO_ARGS=(-p flywheel --test ship_land)
TEST_FILTER=(a_racer_landing_between_build_and_update_ref_is_not_reverted)

mutation "R1  reuse the first splice on every CAS retry" \
    file=src/cmd/commit.rs \
    expect=a_racer_landing_between_build_and_update_ref_is_not_reverted \
    find=$'    let sha = commit_tree(\n        repo,\n        paths,\n        message,\n        &ReservationCtx::skip(),\n        ReservationMode::Skip,\n        |private, parent| {\n            let blobs = splice(parent)?;\n            build_blobs_tree(private, &blobs, parent)\n        },\n    )?;' \
    replace=$'    let mut first: Option<Vec<(String, Vec<u8>)>> = None;\n    let sha = commit_tree(\n        repo,\n        paths,\n        message,\n        &ReservationCtx::skip(),\n        ReservationMode::Skip,\n        |private, parent| {\n            if first.is_none() {\n                first = Some(splice(parent)?);\n            }\n            let blobs = first.clone().unwrap();\n            build_blobs_tree(private, &blobs, parent)\n        },\n    )?;'
