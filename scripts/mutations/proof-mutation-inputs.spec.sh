# Sourced by scripts/verify-mutations.sh.
# bd-b2yg7: a proof re-ran ~110 specs on every landing because the base was a
# stale pin and the cache key carried the proven commit sha. Every mutation run
# also cold-built its own target dir. These mutants restore each regression.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings clean_proof_scopes_mutations_and_shares_one_target_dir proof_green_base_accepts_iso_stamped_receipts)

mutation "B1  the cache key uses the commit sha again" \
    file=scripts/mutation-cache.py \
    expect=the_cache_key_covers_spec_target_test_harness_lock_driver_and_siblings \
    find='    add(h, "driver:clean-proof.sh", read("scripts/clean-proof.sh"))
    add(h, "driver:proof-lib.sh", read("scripts/proof-lib.sh"))' \
    replace='    add(h, "driver", driver.encode())'

mutation "B2  a stale base is pinned again" \
    file=proof.toml \
    expect=clean_proof_scopes_mutations_and_shares_one_target_dir \
    find='# base = "<sha>"' \
    replace='base = "d243d21948ed7d6c4b424b30455fa7dcd04c1001"'

mutation "B3  each mutation export cold-builds again" \
    file=scripts/clean-proof.sh \
    expect=clean_proof_scopes_mutations_and_shares_one_target_dir \
    find='"VERIFY_MUTATIONS_TARGET_DIR=$TMP/target"' \
    replace='"RUSTFLAGS="'

mutation "B4  a real ISO stamp is compared as an integer again" \
    file=scripts/proof-lib.sh \
    expect=proof_green_base_accepts_iso_stamped_receipts \
    find='    if [ "$digits" -gt "$best_stamp" ]; then' \
    replace='    if [ "$stamp" -gt "$best_stamp" ]; then'
