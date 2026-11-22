//! bd-znczi: gated live proof of ADR-0039 "Events primary" (P1).
//!
//! Run only through `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live-five.sh`, which
//! supplies a THROWAWAY project and rex session. This file follows the live
//! carve-out: every test is `#[ignore]`d and gated on FLYWHEEL_LIVE_E2E, so a
//! plain `cargo test` never runs it. Unlike the older sandbox-based version,
//! commands here use the REAL environment: `flywheel init` needs `br`, and the
//! pi/opencode/Claude seats need their harness auth and mise trust.
//!
//! One throwaway session spawns a pi, an opencode and a Claude seat. Each gets
//! one addressed mail. A seat reached through MCP Events leaves an
//! `events-<pin>.jsonl` journal row and the doorbell records NO ring for it.
//! With the per-pin token mint disabled the events pump never starts and the
//! doorbell rings instead — this witness then fails naming that fallback.

use std::path::PathBuf;
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

fn json_field(v: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in keys {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::to_string)
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

/// Retire every seat this test created, even on assertion failure.
struct Cleanup {
    bin: String,
    project: String,
    pins: Vec<String>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pin in &self.pins {
            let _ = run(
                &self.bin,
                &["retire", pin, "--project", &self.project, "--force-working"],
            );
        }
    }
}

#[test]
#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]
fn one_addressed_mail_reaches_pi_opencode_and_claude_through_events_not_doorbell() {
    if !enabled() {
        eprintln!("{GATE} unset — live events witness skipped");
        return;
    }
    let project = live_var("FLYWHEEL_LIVE_PROJECT");
    let state_dir = PathBuf::from(live_var("FLYWHEEL_LIVE_STATE_DIR"));
    let bin = flywheel();
    let tag = format!("znczi-{}", std::process::id());

    // The project dir must exist before spawn. Real env: br is on PATH and the
    // harness sees its own auth, unlike the old TestSandbox version.
    let initiated = run(&bin, &["init", &project]);
    assert!(
        initiated.status.success(),
        "init failed: {}",
        text(&initiated)
    );

    // The runner's throwaway rex session is the only plane we spawn into.
    let _session = live_var("FLYWHEEL_LIVE_SESSION");
    let spawned = run(
        &bin,
        &[
            "spawn",
            &project,
            "--pi",
            "1",
            "--oc",
            "1",
            "--kind",
            "claude=1",
            "--model",
            "harness-default",
            "--escape",
            "--yes",
            "--retire-orphans",
            "--no-focus",
        ],
    );
    assert!(spawned.status.success(), "spawn failed: {}", text(&spawned));

    let status = run(&bin, &["swarm", "status", &project, "--json"]);
    assert!(status.status.success(), "status failed: {}", text(&status));
    let doc: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    let seats = doc
        .get("seats")
        .and_then(serde_json::Value::as_array)
        .expect("a seats array");
    assert_eq!(seats.len(), 3, "expected pi, opencode and claude seats");
    let pins: Vec<String> = seats
        .iter()
        .filter_map(|s| json_field(s, &["pin"]))
        .collect();
    assert_eq!(pins.len(), 3, "each seat must carry a pin");
    let _cleanup = Cleanup {
        bin: bin.clone(),
        project: project.clone(),
        pins: pins.clone(),
    };

    // bd-rrlo2 guard: the spawn loop must have minted a 0600 per-pin token
    // before launch. No token means every seat is on the doorbell fallback.
    for pin in &pins {
        let token = state_dir
            .join("events-tokens")
            .join(format!("{project}-{pin}.token"));
        assert!(
            token.is_file(),
            "{pin}: no per-pin events token — spawn did not mint (bd-rrlo2)"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&token)
                .expect("token metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{pin}: token file must be 0600");
        }
    }

    let doorbell_journal_dir = state_dir.join("doorbell").join("journals");
    for (i, pin) in pins.iter().enumerate() {
        let marker = format!("{tag}-{i}");
        let sent = run(&bin, &["send", &project, &marker, "--to", pin, "--raw"]);
        assert!(sent.status.success(), "send failed: {}", text(&sent));

        let events_journal = state_dir
            .join("doorbell")
            .join(format!("events-{pin}.jsonl"));
        let events_ok = poll(90, || {
            std::fs::read_to_string(&events_journal)
                .map(|s| s.lines().any(|l| l.contains("\"op\":\"fetch\"")))
                .unwrap_or(false)
        });
        assert!(
            events_ok,
            "{pin}: no events-journal rows — MCP Events is inert. With the per-pin token mint disabled the seat falls back to the DOORBELL (bd-rrlo2); this witness requires the events path."
        );

        let mut rings = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&doorbell_journal_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.to_string_lossy().contains("doorbell-journal-") {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&path) {
                    for line in body.lines() {
                        if line.contains(&format!("\"pin\":\"{pin}\"")) && line.contains(&marker) {
                            rings.push(line.to_string());
                        }
                    }
                }
            }
        }
        assert!(
            rings.is_empty(),
            "{pin}: the doorbell rang for {marker} — the seat fell back to the DOORBELL instead of the events path: {rings:?}"
        );
    }
}
