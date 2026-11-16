# Sourced by scripts/verify-mutations.sh.
# bd-8q3qf: the ledger's SHA rule searches for the citing commit, and a bead's
# work can live in another repo (bd-nwfj7 closed against a beads-fork sha).
# `--repo` moves the SEARCH, never the gate rows. Each mutant restores the
# pre-fix shape and the witness must go RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(derive_head_sha_in_searches_the_work_repo_for_the_citing_commit)

mutation "R1  the log search ignores the repo it was given" \
    file=src/verdict.rs \
    expect=derive_head_sha_in_searches_the_work_repo_for_the_citing_commit \
    find='            &format!("--grep={bead_id}"),
            "--format=%H%x1f%s",
        ])
        .current_dir(search)' \
    replace='            &format!("--grep={bead_id}"),
            "--format=%H%x1f%s",
        ])
        .current_dir(cwd)'

mutation "R2  an empty foreign commit can bind the bead" \
    file=src/verdict.rs \
    expect=derive_head_sha_in_searches_the_work_repo_for_the_citing_commit \
    find='        .filter(|sha| !commit_changes_nothing(search, sha).unwrap_or(false))' \
    replace='        .filter(|sha| !commit_changes_nothing(cwd, sha).unwrap_or(false))'
