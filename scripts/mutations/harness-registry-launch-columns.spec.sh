# Sourced by scripts/verify-mutations.sh.
# bd-6setu.1 (ADR-0022 D3): the launch columns are registry facts. The mutant
# changes a row's model flag, so an adapter that still answered from a second
# match arm would keep the old argv while the row says otherwise.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_row_answers_the_launch_columns)

mutation "L1  the model flag stops being the row's fact" \
    file=src/headless.rs \
    expect=every_row_answers_the_launch_columns \
    find='    let mut args = vec![flag.to_string(), model.to_string()];' \
    replace='    let mut args = vec!["--model".to_string(), model.to_string()];'
