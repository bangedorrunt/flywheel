//! bd-znczi: static contract for the gated live events-delivery witness (P1).
//!
//! The live file cannot run in a proof (no rex session, bd-rrlo2 not landed
//! yet). These checks run ALWAYS: they pin the gate, the ignore discipline and
//! the e2e runner wiring so the live test cannot silently rot into an
//! ungated test that boots live state during a normal `cargo test`.

fn root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

#[test]
fn live_events_delivery_is_gated_and_every_test_is_ignored() {
    let src = std::fs::read_to_string(format!("{}/tests/live_events_delivery.rs", root()))
        .expect("tests/live_events_delivery.rs");
    assert!(
        src.contains("FLYWHEEL_LIVE_E2E"),
        "the live file must gate on FLYWHEEL_LIVE_E2E"
    );
    let mut tests = 0usize;
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[test]" {
            tests += 1;
            let next = lines.get(i + 1).unwrap_or(&"").trim().to_string();
            assert!(
                next.starts_with("#[ignore"),
                "every #[test] in a live file must be #[ignore]d; line {} is followed by {next:?}",
                i + 1
            );
        }
    }
    assert!(
        tests >= 1,
        "the live witness must declare at least one test"
    );
}

#[test]
fn e2e_live_five_runs_the_events_delivery_witness() {
    let runner = std::fs::read_to_string(format!("{}/scripts/e2e-live-five.sh", root()))
        .expect("scripts/e2e-live-five.sh");
    // The runner also names this file in a per-leg env branch, so a bare
    // contains() passes even when the TESTS array dropped the leg. Check the
    // array it actually iterates.
    let start = runner
        .find("TESTS=(")
        .expect("the runner must declare a TESTS array");
    let end = runner[start..]
        .find(')')
        .map(|i| start + i)
        .unwrap_or(runner.len());
    let list = &runner[start..end];
    assert!(
        list.lines()
            .any(|line| line.trim() == "tests/live_events_delivery.rs"),
        "the runner's TESTS array must include the events-delivery witness: {list}"
    );
}
