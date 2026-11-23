//! bd-znczi/bd-rrlo2: gated live proof that ADR-0039 "Events primary" works.
//!
//! Run only through `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live-five.sh`, which
//! supplies a throwaway project and state dir. This leg owns a fake no-door
//! seat: it seeds one registry row and one 0600 token, then starts its own
//! doorbell host so the daemon events pump arms for that pin. No harness kind
//! (grok, gemini, antigravity) and no runner env branch are needed.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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

fn bin(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
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

fn state_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("flywheel")
}

/// Kill the test-owned doorbell child even if an assertion fails.
struct PumpHost(Child);
impl Drop for PumpHost {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]
fn one_addressed_mail_reaches_the_events_pump_not_the_doorbell() {
    if !enabled() {
        eprintln!("{GATE} unset — live events witness skipped");
        return;
    }
    let project = live_var("FLYWHEEL_LIVE_PROJECT");
    let fw = bin("FLYWHEEL_BIN", env!("CARGO_BIN_EXE_flywheel"));
    let toron = bin("TORON_BIN", "toron");
    let state = state_dir();
    let tag = format!("znczi-{}", std::process::id());
    let probe = format!("{project}-probe");
    let pin = format!("EventsProbe{}", std::process::id());

    // One fake no-door seat, owned by this test. The pump only needs the
    // registry row and its token; no harness process is involved.
    let agents = state.join("agents");
    std::fs::create_dir_all(&agents).expect("agents dir");
    let registry = agents.join(format!("{probe}.json"));
    let row = serde_json::json!([{
        "pane_id": "block:fake-events-seat",
        "name": "fake-events-seat",
        "harness": "gemini",
        "mail_identity": pin,
        "skills_injected": false,
        "incarnation": 0
    }]);
    std::fs::write(&registry, row.to_string()).expect("fake seat row");

    let minted = run(
        &toron,
        &[
            "events",
            "token",
            "mint",
            "--pin",
            &pin,
            "--project",
            &probe,
            "--ttl-secs",
            "600",
        ],
    );
    assert!(
        minted.status.success(),
        "token mint failed: {}",
        text(&minted)
    );
    let minted_json: serde_json::Value =
        serde_json::from_str(std::str::from_utf8(&minted.stdout).expect("mint utf8"))
            .expect("mint json");
    let token = minted_json
        .get("token")
        .and_then(serde_json::Value::as_str)
        .expect("mint token");
    let tokens = state.join("events-tokens");
    std::fs::create_dir_all(&tokens).expect("token dir");
    let token_file = tokens.join(format!("{probe}-{pin}.token"));
    std::fs::write(&token_file, token).expect("token file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("token mode");
    }

    // Arm the pump with a test-owned doorbell host. spawn_events_pumps reads
    // the registry at host start, so the row above must exist first.
    let workspace = format!("session:events-probe-{}", std::process::id());
    let host = Command::new(&fw)
        .args(["__doorbell", "--project", &probe, "--workspace", &workspace])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test-owned doorbell host");
    let _host = PumpHost(host);

    let journal = state.join("doorbell").join(format!("events-{pin}.jsonl"));
    let pumped = poll(90, || {
        std::fs::read_to_string(&journal)
            .map(|s| s.lines().any(|l| l.contains("\"op\":\"fetch\"")))
            .unwrap_or(false)
    });
    assert!(
        pumped,
        "{pin}: no events-journal fetch rows — the daemon pump did not arm: {}",
        journal.display()
    );

    let sent = run(
        &toron,
        &[
            "mail",
            "send",
            "--project",
            &probe,
            "--as",
            &pin,
            "--to",
            &pin,
            "--subject",
            &tag,
            "--body",
            &tag,
        ],
    );
    assert!(sent.status.success(), "mail send failed: {}", text(&sent));

    let doorbell_journal_dir = state.join("journals");
    let mut rings = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&doorbell_journal_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.to_string_lossy().contains("doorbell-journal-") {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&path) {
                for line in body.lines() {
                    if line.contains(&format!("\"pin\":\"{pin}\"")) && line.contains(&tag) {
                        rings.push(line.to_string());
                    }
                }
            }
        }
    }
    assert!(
        rings.is_empty(),
        "{pin}: the doorbell rang for {tag} instead of the events path: {rings:?}"
    );

    let _ = std::fs::remove_file(&registry);
    let _ = std::fs::remove_file(&token_file);
}
