# Sourced by scripts/verify-mutations.sh.
#
# bd-5bno9: one JEV model pin. The bundled default packs and the const are one
# fact, and the test must see either side drift.
#
#   scripts/verify-mutations.sh scripts/mutations/jev-pin.spec.sh --cargo mbx
#
# What this does not prove: that the user tier (`~/.config/flywheel/
# judgment-packs`) was reseeded after a pin bump — that is the operator's
# `judgment pack seed --overwrite all` step, checked by reading the files.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_bundled_pack_pins_the_one_model)

mutation "J1  one bundled pack names a different model" \
    file=defaults/judgment-packs/wave-readiness.v1.yaml \
    expect=every_bundled_pack_pins_the_one_model \
    find='model: jev-preview' \
    replace='model: jev-latest'

mutation "J2  the pin moves and the packs do not" \
    file=src/judgment.rs \
    expect=every_bundled_pack_pins_the_one_model \
    find='pub const JEV_MODEL_PIN: &str = "jev-preview";' \
    replace='pub const JEV_MODEL_PIN: &str = "jev-next";'
