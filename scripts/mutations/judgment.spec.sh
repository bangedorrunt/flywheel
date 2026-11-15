# Sourced by scripts/verify-mutations.sh.
#
# The pack seed replaces changed same-version packs from the HIGHEST bundled
# version (bd-0op96).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(pack)

JUDGMENT=src/judgment.rs

mutation "J1  All replaces on existence alone" \
    file=$JUDGMENT \
    expect=seed_all_replaces_a_same_version_pack_whose_bytes_differ \
    find='            PackOverwrite::All => dst.exists() && !identical && !user_newer,' \
    replace='            PackOverwrite::All => dst.exists(),'

mutation "J2  the first bundled file wins, not the highest version" \
    file=$JUDGMENT \
    expect=bundled_pack_picks_the_highest_version_per_id \
    find='        .max_by_key(|(name, _)| pack_entry(Path::new(name)).map_or(0, |(_, v)| v))' \
    replace='        .next()'
