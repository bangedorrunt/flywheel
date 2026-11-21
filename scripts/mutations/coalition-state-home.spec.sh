# Sourced by scripts/verify-mutations.sh. proof78 flake: a coalition test that
# forms a coalition writes a run log under config::state_dir(); without the
# test_state_home guard it borrows another test's ambient XDG dir.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(every_coalition_forming_test_takes_the_state_home_guard)

mutation "G1  the leaf-result test drops its state-home guard" \
    file=src/coalition.rs \
    expect=every_coalition_forming_test_takes_the_state_home_guard \
    find=$'    fn leaf_result_lands_in_coalition_channel() {\n        let _home = crate::config::test_state_home();' \
    replace='    fn leaf_result_lands_in_coalition_channel() {'

mutation "G2  the xproj test drops its state-home guard" \
    file=src/coalition.rs \
    expect=every_coalition_forming_test_takes_the_state_home_guard \
    find=$'    fn xproj_coalition_scopes_intents_per_project_and_disband_cleans_both() {\n        let _home = crate::config::test_state_home();' \
    replace='    fn xproj_coalition_scopes_intents_per_project_and_disband_cleans_both() {'
