//! bd-sfkm8: gated live proof that `workflow compose --join --run --yes`
//! dispatches a bound step to the LIVE pin the join chose, not to a fresh
//! profile. Run only through `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live-five.sh`,
//! which supplies a throwaway project and a real opencode seat. Every test is
//! `#[ignore]`d and gated, so a plain `cargo test` never runs it.

use std::process::Command;
use std::time::{Duration, Instant};

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

fn poll(secs: u64, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

fn seats(bin: &str, project: &str) -> serde_json::Value {
    let status = run(bin, &["swarm", "status", project, "--json"]);
    assert!(status.status.success(), "status failed: {}", text(&status));
    serde_json::from_slice(&status.stdout).expect("status JSON")
}

fn find_seat<'a>(doc: &'a serde_json::Value, role: &str) -> Option<&'a serde_json::Value> {
    doc.get("seats")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find(|s| {
            json_field(s, "role").as_deref() == Some(role)
                && json_field(s, "pin").is_some()
                && json_field(s, "name").is_some()
        })
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

    // The runner's seats are harness-named ("opencode"/"grok"), which are not
    // profile catalog entries. Spawn one real coder seat so role:coder is both
    // a resolvable profile and a live join candidate.
    let spawned = run(
        &bin,
        &[
            "spawn",
            &project,
            "--profile",
            "coder",
            "--model",
            "harness-default",
            "--escape",
            "--yes",
            "--retire-orphans",
            "--no-focus",
        ],
    );
    assert!(spawned.status.success(), "spawn failed: {}", text(&spawned));

    let _idle = poll(120, || {
        find_seat(&seats(&bin, &project), "coder")
            .is_some_and(|s| matches!(json_field(s, "status").as_deref(), Some("idle" | "done")))
    });
    let doc = seats(&bin, &project);
    let seat =
        find_seat(&doc, "coder").unwrap_or_else(|| panic!("a live coder seat is required: {doc}"));
    let pin = json_field(seat, "pin").expect("pin");
    let name = json_field(seat, "name").expect("name");

    // Real CLI path: compose a role:coder step, join against live seats, run
    // it with consent. Nothing here calls apply_join_bindings directly.
    let out = run(
        &bin,
        &[
            "workflow",
            "--skip-preflight",
            "compose",
            "bd-sfkm8 live join dispatch",
            "--parts",
            "role:coder:ce-work",
            "--join",
            "--run",
            "--yes",
            "--retire-orphans",
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
        log.contains(&format!("flywheel: dispatch @coder → {name}")),
        "bound step must dispatch to the chosen coder seat {pin}/{name}: {log}"
    );
}
