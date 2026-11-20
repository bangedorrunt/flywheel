//! bd-o1z4m live witness: a fresh opencode seat wakes on one addressed mail
//! through its native events stream, exactly once (no doorbell double-ring).
//!
//! Live carve-out (AGENTS.md): every test is `#[ignore]`d AND this file is
//! gated on FLYWHEEL_LIVE_E2E, so it can never run by default. It uses a temp
//! HOME via TestSandbox and NEVER edits the operator's opencode config.
//!
//!   FLYWHEEL_LIVE_E2E=1 FLYWHEEL_LIVE_PROJECT=e2e-live-five \
//!     cargo test -p flywheel --test live_opencode_events -- --ignored --nocapture

use flywheel_test_sandbox::TestSandbox;
use std::process::Command;
use std::time::{Duration, Instant};

fn gate() -> Option<String> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let project = std::env::var("FLYWHEEL_LIVE_PROJECT").ok()?;
    (!project.trim().is_empty()).then_some(project)
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in keys {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
}

#[test]
#[ignore = "gated live probe: FLYWHEEL_LIVE_E2E=1 needs a live toron and a throwaway rex session"]
fn a_fresh_opencode_seat_receives_one_addressed_mail_natively() {
    let Some(project) = gate() else {
        eprintln!(
            "bd-o1z4m live witness skipped: set FLYWHEEL_LIVE_E2E=1 and FLYWHEEL_LIVE_PROJECT"
        );
        return;
    };
    let sandbox = TestSandbox::new();
    let bin = env!("CARGO_BIN_EXE_flywheel");
    let marker = format!("o1z4m-live-{}", std::process::id());

    // Spawn a throwaway seat. The pane env gets the per-pane events config
    // (experimental.mcp_events + the 0600 token path) from the spawn path.
    let spawned = sandbox
        .command(bin)
        .args([
            "spawn",
            &project,
            "--oc",
            "1",
            "--model",
            "harness-default",
            "--escape",
            "--yes",
        ])
        .output()
        .expect("spawn a throwaway opencode seat");
    assert!(spawned.status.success(), "spawn failed: {}", text(&spawned));

    let status = sandbox
        .command(bin)
        .args(["swarm", "status", &project, "--json"])
        .output()
        .expect("swarm status");
    assert!(status.status.success(), "status failed: {}", text(&status));
    let doc: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    let seat = doc
        .get("seats")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .expect("a spawned seat row");
    let pin = json_field(seat, &["pin"]).expect("seat pin");
    let pane_id = json_field(seat, &["pane_id"])
        .or_else(|| json_field(seat, &["pane"]))
        .expect("seat pane id");

    // Fail-safe cleanup: retire the seat even if an assertion panics.
    struct Retire<'a> {
        bin: &'a str,
        project: &'a str,
        pin: String,
        sandbox: &'a TestSandbox,
    }
    impl Drop for Retire<'_> {
        fn drop(&mut self) {
            let _ = self
                .sandbox
                .command(self.bin)
                .args([
                    "retire",
                    &self.pin,
                    "--project",
                    self.project,
                    "--force-working",
                ])
                .output();
        }
    }
    let _retire = Retire {
        bin,
        project: &project,
        pin: pin.clone(),
        sandbox: &sandbox,
    };

    // Send one addressed mail to the seat's pin.
    let sent = sandbox
        .command(bin)
        .args(["send", &project, &marker, "--to", &pin])
        .output()
        .expect("send addressed mail");
    assert!(sent.status.success(), "send failed: {}", text(&sent));

    // Poll the real pane capture until the hint appears; count one ring.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sightings = 0usize;
    while Instant::now() < deadline {
        let cap = Command::new("rex")
            .args(["capture", "--block", &pane_id])
            .output()
            .expect("rex capture");
        if cap.status.success() {
            let screen = String::from_utf8_lossy(&cap.stdout);
            sightings = screen.lines().filter(|line| line.contains(&marker)).count();
            if sightings > 0 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    assert_eq!(
        sightings, 1,
        "the native stream must deliver exactly one hint for {marker}"
    );
}
