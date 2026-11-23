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
    expect=the_cache_key_scopes_to_the_mutated_and_witness_fns \
    find='        add(h, "mutated-fn:" + mid + ":" + path, enclosing_fn(read(path), needle))' \
    replace='        (0)'

mutation "C2  the key ignores the file that declares the expect test" \
    file=$CACHE \
    expect=the_cache_key_scopes_to_the_mutated_and_witness_fns \
    find='        add(h, "test-fn:" + name + ":" + path, body)' \
    replace='        (0)'

mutation "C3  the key ignores the mutation harness" \
    file=$CACHE \
    expect=the_cache_key_scopes_to_the_mutated_and_witness_fns \
    find='    add(h, "harness", read("scripts/verify-mutations.sh"))' \
    replace='    (0)'

mutation "C4  a sibling pin moves the key again" \
    file=$CACHE \
    expect=the_cache_key_scopes_to_the_mutated_and_witness_fns \
    find='    for name in missing:' \
    replace='    for name, rev in siblings_from_proof_toml(read):
        add(h, "sibling:" + name, rev.encode())
    for name in missing:'

mutation "C7  whole-file hashing is restored" \
    file=$CACHE \
    expect=the_cache_key_scopes_to_the_mutated_and_witness_fns \
    find='                return text[start:i + 1].encode("utf-8")' \
    replace='                return data'

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
    find='      mut_key="$(python3 scripts/mutation-cache.py key --proven "$PROVEN_SHA" --repo "$ROOT" \
        --spec "$spec" --driver "${CLEAN_PROOF_DRIVER_SHA:-}" 2>/dev/null || true)"' \
    replace='      mut_key=""'

mutation "C10  the proof build accepts a stale Cargo.lock" \
    file=$PROOF \
    expect=clean_proof_builds_with_a_locked_lockfile \
    find='build --workspace --all-targets --locked' \
    replace='build --workspace --all-targets'

mutation "C9  the driver fixture inherits the running proof's environment" \
    file=src/cmd/clean_proof.rs \
    expect=a_sha_proof_runs_the_revisions_own_driver \
    find='        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path().join("home"))' \
    replace='        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path().join("home"))'
