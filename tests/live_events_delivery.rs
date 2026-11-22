//! bd-znczi: gated live proof of ADR-0039 "Events primary" (P1).
//!
//! Run only through `FLYWHEEL_LIVE_E2E=1 bash scripts/e2e-live-five.sh`, which
//! supplies a THROWAWAY project, state dir and a real harness seat spawned in
//! that project. This file follows the live carve-out: every test is `#[ignore]`d
//! and gated on FLYWHEEL_LIVE_E2E, so a plain `cargo test` never runs it.
//! Commands use the REAL environment (no TestSandbox): the seat needs its
//! harness auth and mise trust, and `toron` needs the operator's local key.
//!
//! The runner's spawned seat reaches the mailbox through MCP Events with a
//! 0600 per-pin token (bd-rrlo2). This witness asserts that seat's events
//! journal records a fetch and that the doorbell records NO ring for the mail.
//! With the per-pin token mint disabled the pump never starts and the doorbell
//! rings instead — this witness then fails naming that fallback.

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

fn toron() -> String {
    std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string())
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

#[test]
#[ignore = "gated live: FLYWHEEL_LIVE_E2E=1, run only via scripts/e2e-live-five.sh"]
fn one_addressed_mail_reaches_the_seat_through_events_not_doorbell() {
    if !enabled() {
        eprintln!("{GATE} unset — live events witness skipped");
        return;
    }
    let project = live_var("FLYWHEEL_LIVE_PROJECT");
    let state_dir = PathBuf::from(live_var("FLYWHEEL_LIVE_STATE_DIR"));
    let bin = flywheel();
    let tag = format!("znczi-{}", std::process::id());

    // The runner spawned one real harness seat in this project. Discover it the
    // same way the runner does: a row with a pin and a live pane.
    let status = run(&bin, &["swarm", "status", &project, "--json"]);
    assert!(status.status.success(), "status failed: {}", text(&status));
    let doc: serde_json::Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    let seats = doc
        .get("seats")
        .and_then(serde_json::Value::as_array)
        .expect("a seats array");
    let pin = seats
        .iter()
        .find(|s| json_field(s, &["pin"]).is_some() && json_field(s, &["pane_id"]).is_some())
        .and_then(|s| json_field(s, &["pin"]))
        .unwrap_or_else(|| panic!("the runner must leave one harness seat: {seats:?}"));

    // bd-rrlo2 guard: the rex spawn path must have minted a 0600 per-pin token
    // before launch. No token means this seat is on the doorbell fallback.
    let flywheel_state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("flywheel");
    let token = flywheel_state
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

    // Addressed mail straight into the pin's mailbox: the seat's events pump
    // fetches it with its token. Sending as the seat itself needs no extra
    // operator registration and still exercises the same events door.
    let sent = run(
        &toron(),
        &[
            "mail",
            "send",
            "--project",
            &project,
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

    // (1) Events path active: the per-pin events journal got a fetch row.
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

    // (2) No doorbell ring for this pin bearing our marker.
    let doorbell_journal_dir = state_dir.join("journals");
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
        "{pin}: the doorbell rang for {tag} — the seat fell back to the DOORBELL instead of the events path: {rings:?}"
    );
}
