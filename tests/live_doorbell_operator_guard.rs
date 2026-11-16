//! LIVE leg for bd-0uzrq.3 — the operator-typing guard, on a real pane.
//!
//! The unit corpus proves the classifiers; this proves the wiring: a wake
//! typed while the operator has a half-typed line DEFERS, and the same wake
//! rings once the line is cleared.
//!
//! Live carve-out (AGENTS.md): every test here is `#[ignore]`d AND the file is
//! gated on `FLYWHEEL_LIVE_E2E`, so it can never run by default. It also needs
//! an explicit target — it refuses to guess a pane, because the whole point is
//! not to type into someone else's:
//!
//! ```text
//! FLYWHEEL_LIVE_E2E=1 \
//! FLYWHEEL_LIVE_SESSION=flywheel-flywheel \
//! FLYWHEEL_LIVE_BLOCK=<throwaway block label> \
//! FLYWHEEL_LIVE_PIN=<the pin running in that block> \
//! FLYWHEEL_LIVE_PROJECT=flywheel \
//! FLYWHEEL_LIVE_AS=<your pin> \
//!   cargo test -p flywheel --test live_doorbell_operator_guard -- --ignored --nocapture
//! ```
//!
//! The block must be a THROWAWAY crew pane. Use the block's pin, never the
//! captain's.

use std::process::Command;

fn gate() -> Option<(String, String, String, String, String)> {
    if std::env::var("FLYWHEEL_LIVE_E2E").ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("FLYWHEEL_LIVE_SESSION")?,
        var("FLYWHEEL_LIVE_BLOCK")?,
        var("FLYWHEEL_LIVE_PIN")?,
        var("FLYWHEEL_LIVE_PROJECT")?,
        var("FLYWHEEL_LIVE_AS")?,
    ))
}

fn rex(args: &[&str]) -> String {
    let out = Command::new("rex")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("rex {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "rex {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn crew_prompt(pin: &str, project: &str, as_pin: &str, text: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_flywheel"))
        .args([
            "crew",
            "prompt",
            pin,
            text,
            "--project",
            project,
            "--as",
            as_pin,
        ])
        .output()
        .expect("spawn flywheel crew prompt")
}

/// The one command the captain runs after install (bd-0uzrq.3, live leg).
#[test]
#[ignore = "live: needs FLYWHEEL_LIVE_E2E + an explicit throwaway pane"]
fn a_typed_line_defers_the_wake_and_clearing_it_rings() {
    let Some((session, block, pin, project, as_pin)) = gate() else {
        eprintln!(
            "live_doorbell_operator_guard: skipped — set FLYWHEEL_LIVE_E2E=1 and the \
             FLYWHEEL_LIVE_SESSION/BLOCK/PIN/PROJECT/AS variables to run it"
        );
        return;
    };

    // 1. The operator has half a line: type it, never press Enter.
    rex(&["send", "-s", &session, "--block", &block, "half typed line"]);
    std::thread::sleep(std::time::Duration::from_millis(800));

    // 2. A wake now must DEFER — nothing is typed into their line.
    let deferred = crew_prompt(&pin, &project, &as_pin, "live probe: deferral");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&deferred.stdout),
        String::from_utf8_lossy(&deferred.stderr)
    );
    assert!(
        text.contains("deferred"),
        "a wake into a half-typed line must refuse with a deferral: {text}"
    );
    assert!(
        !text.contains("half typed line"),
        "the refusal never echoes or rewrites their text: {text}"
    );

    // 3. Clear the line (ctrl+u) and the same wake rings.
    rex(&["send-key", "-s", &session, "--block", &block, "ctrl+u"]);
    std::thread::sleep(std::time::Duration::from_millis(800));
    let rang = crew_prompt(&pin, &project, &as_pin, "live probe: ring");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&rang.stdout),
        String::from_utf8_lossy(&rang.stderr)
    );
    assert!(
        !text.contains("deferred"),
        "with the line cleared the wake must ring, not defer: {text}"
    );
    assert!(
        rang.status.success(),
        "the wake must land and report it: {text}"
    );
}
