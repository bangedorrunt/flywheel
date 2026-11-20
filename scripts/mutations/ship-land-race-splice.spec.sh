# Sourced by scripts/verify-mutations.sh.
# bd-kz3sc rework 2: the splice must be recomputed on each CAS retry against the
# moved parent, or a neighbour that lands in between is silently reverted.

CARGO_ARGS=(-p flywheel --test ship_land)
TEST_FILTER=(a_racer_landing_between_build_and_update_ref_is_not_reverted)

mutation "R1  reuse the first splice on every CAS retry" \
    file=src/cmd/commit.rs \
    expect=a_racer_landing_between_build_and_update_ref_is_not_reverted \
    find='    let sha = commit_tree(repo, paths, message, |private, parent| {
        let blobs = splice(parent)?;
        build_blobs_tree(private, &blobs, parent)
    })?;' \
    replace='    let mut first: Option<Vec<(String, Vec<u8>)>> = None;
    let sha = commit_tree(repo, paths, message, |private, parent| {
        if first.is_none() {
            first = Some(splice(parent)?);
        }
        let blobs = first.clone().unwrap();
        build_blobs_tree(private, &blobs, parent)
    })?;'
