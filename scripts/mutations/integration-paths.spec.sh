# Sourced by scripts/verify-mutations.sh.
# bd-mdpfr: integration paths resolve from injected HOME/CODEX_HOME and refuse
# a missing config dir.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(
    install_writes_under_temp_home_and_is_idempotent
    install_refuses_when_the_config_dir_is_missing
)

mutation "P1  grok ignores the injected HOME" \
    file=src/cmd/integration.rs \
    expect=install_writes_under_temp_home_and_is_idempotent \
    find='    p.home.join(".grok/hooks/flywheel.json")' \
    replace='    std::path::PathBuf::from("/tmp/not-the-seat-home").join("flywheel.json")'

mutation "P2  install creates a missing config dir instead of refusing" \
    file=src/cmd/integration.rs \
    expect=install_refuses_when_the_config_dir_is_missing \
    find='        bail!("agent config dir {} does not exist", parent.display());' \
    replace='        let _ = std::fs::create_dir_all(parent);'
