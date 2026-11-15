# Sourced by scripts/verify-mutations.sh.
#
# bd-m5afp (epic bd-jp9z1): `judgment pack seed --overwrite all` is the one way
# an operator refreshes the user tier after a bundled pack changes.
#
#   scripts/verify-mutations.sh scripts/mutations/seed-all.spec.sh --cargo mbx
#
# What this does not prove, stated so nobody assumes it does: the
# `!user_newer` half of the All arm is NOT covered. A newer user pack is kept
# because its file NAME differs (reclassify.v9.yaml vs the bundled
# reclassify.v3.yaml), so `dst.exists()` decides before `user_newer` is read.
# The guard only bites when the same name carries a higher version inside, and
# no test builds that file. That is a named gap, not a pass.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(seed_all_)

JUDGMENT=src/judgment.rs

mutation "S1  All stops replacing a drifted same-version pack" \
    file=$JUDGMENT \
    expect=seed_all_replaces_a_same_version_pack_whose_bytes_differ \
    find='            PackOverwrite::All => dst.exists() && !identical && !user_newer,' \
    replace='            PackOverwrite::All => false,'
