//! bd-sfkm8: gated live proof that `workflow compose --join --run --yes`
//! dispatches a bound step to the LIVE pin the join chose, not to a fresh
//! profile. Run only through `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live-five.sh`,
//! which supplies a throwaway project and a real opencode seat. Every test is
//! `#[ignore]`d and gated, so a plain `cargo test` never runs it.

use std::process::Command;

const GATE: &str = "FLYWHEEL_LIVE_E2E";

fn enabled() -> bool {
    std::env::var(GATE).ok().as_deref() == Some("1")
}

fn live_var(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{GATE}=1 requires {key} to be set"))
}

fn flywheel() -> String {
    std::env::var("FLYWHEEL_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_flywheel").to_string())
}

fn run(bin: &str, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{bin} {}: {e}", args.join(" ")))
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[test]
#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]
fn composed_join_dispatches_the_bound_step_to_the_live_pin() {
    if !enabled() {
        eprintln!("{GATE} unset — live join dispatch witness skipped");
        return;
    }
    let project = live_var("FLYWHEEL_LIVE_PROJECT");
    let bin = flywheel();

    // The runner's real opencode seat is the only seat with role "opencode",
    // so a role:opencode step must bind to it.
    let status = run(&bin, &["swarm", "status", &project, "--json"]);
    assert!(status.status.success(), "status failed: {}", text(&status));
    let doc: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    let seats = doc
        .get("seats")
        .and_then(serde_json::Value::as_array)
        .expect("a seats array");
    let seat = seats
        .iter()
        .find(|s| {
            json_field(s, "harness").as_deref() == Some("opencode")
                && json_field(s, "pin").is_some()
                && json_field(s, "name").is_some()
        })
        .unwrap_or_else(|| panic!("the runner must leave one opencode seat: {seats:?}"));
    let pin = json_field(seat, "pin").expect("pin");
    let name = json_field(seat, "name").expect("name");

    // Real CLI path: compose a role:opencode step, join against live seats, run
    // it with consent. Nothing here calls apply_join_bindings directly.
    let out = run(
        &bin,
        &[
            "workflow",
            "--skip-preflight",
            "compose",
            "bd-sfkm8 live join dispatch",
            "--parts",
            "role:opencode:ce-work",
            "--join",
            "--run",
            "--yes",
            "--project",
            &project,
        ],
    );
    let log = text(&out);
    assert!(out.status.success(), "compose --join --run failed: {log}");
    assert!(
        log.contains("flywheel: join step "),
        "the join plan must be printed: {log}"
    );
    assert!(
        log.contains(&format!("flywheel: dispatch @opencode → {name}")),
        "bound step must dispatch to the chosen opencode seat {pin}/{name}: {log}"
    );
}
