#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-jp9z1.2: the verdict cache is honest only if its key covers every input a
# verdict depends on and a hit still needs a green witness.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(clean_proof)

CACHE=scripts/mutation-cache.py
PROOF=scripts/clean-proof.sh

mutation "C1  the key ignores the target file" \
    file=$CACHE \
    expect=the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings \
    find='        add(h, "target:" + path, read(path))' \
    replace='        (0)'

mutation "C2  the key ignores the file that declares the expect test" \
    file=$CACHE \
    expect=the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings \
    find='        add(h, "testfile:" + path, read(path))' \
    replace='        (0)'

mutation "C3  the key ignores the mutation harness" \
    file=$CACHE \
    expect=the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings \
    find='    add(h, "harness", read("scripts/verify-mutations.sh"))' \
    replace='    (0)'

mutation "C4  the key ignores the driver scripts" \
    file=$CACHE \
    expect=the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings \
    find='    add(h, "driver:clean-proof.sh", read("scripts/clean-proof.sh"))' \
    replace='    (0)'

mutation "C5  a missing witness still counts as a hit" \
    file=$CACHE \
    expect=a_cached_spec_hits_only_on_the_same_key_with_a_green_witness \
    find='    except (OSError, ValueError):' \
    replace='    except ValueError:'

mutation "C6  a cached verdict need not match what the spec expects" \
    file=$CACHE \
    expect=a_cached_spec_hits_only_on_the_same_key_with_a_green_witness \
    find='        if row.get("verdict") != (DEAD if green else RED):' \
    replace='        if False:'

mutation "C8  the proof stops computing a cache key" \
    file=$PROOF \
    expect=clean_proof_computes_and_passes_a_cache_key \
    find='    mut_key=""
    if [ "$NO_CACHE" -eq 0 ] && [ "$MUTATIONS_MODE" != full ]; then' \
    replace='    mut_key=""
    if false; then'

mutation "C9  the driver fixture inherits the running proof's environment" \
    file=src/cmd/clean_proof.rs \
    expect=a_sha_proof_runs_the_revisions_own_driver \
    find='        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path().join("home"))' \
    replace='        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path().join("home"))'
