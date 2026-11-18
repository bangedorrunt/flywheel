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
//! The block must be a THROWAWAY crew pane. This test addresses it by PANE ID,
//! deliberately skipping pin -> pane resolution (spawn rows), which the real
//! fleet exercises on every wake; its subject is the operator-typing guard on a
//! real screen. The pin is still bootstrapped in toron for the mail half.

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

/// bd-oo7zi: the live leg exercises `src/cmd/send.rs`'s door
/// (`pane_read::defer_if_operator_typing`) against a REAL screen, addressed by
/// pane id, which is a documented `--to` form. It deliberately skips pin -> pane
/// resolution (spawn rows) and the toron mail half: the strict mail-wake form
/// (`src/mail_wake.rs`, `defer_if_operator_typing_strict`) is covered by unit
/// tests, and a rex block id is not a mail recipient.
/// A real harness pane can drift back to `working` between the two probes;
/// wait for idle before either send so the status gate is not what we test.
fn wait_idle(project: &str) {
    for _ in 0..24 {
        let out = Command::new(env!("CARGO_BIN_EXE_flywheel"))
            .args(["swarm", "status", project, "--json"])
            .output()
            .expect("flywheel swarm status");
        let body = String::from_utf8_lossy(&out.stdout);
        if body.contains("\"status\": \"idle\"") || body.contains("\"status\": \"done\"") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    panic!("the e2e pane never became idle: {project}");
}

/// What is actually ON the pane, as opposed to send.rs's agent-settle receipt
/// (a bare zsh pane never settles an agent, so `landed:false` says nothing
/// about whether the text reached the screen).
fn pane_capture(session: &str, block: &str) -> String {
    rex(&["capture", "-s", session, "-b", block])
}

fn send_probe_with(
    bin: &std::path::Path,
    pane: &str,
    project: &str,
    text: &str,
) -> std::process::Output {
    Command::new(bin)
        .args(["send", project, text, "--to", pane])
        .output()
        .expect("spawn flywheel send")
}

fn send_probe(pane: &str, project: &str, text: &str) -> std::process::Output {
    send_probe_with(
        std::path::Path::new(env!("CARGO_BIN_EXE_flywheel")),
        pane,
        project,
        text,
    )
}

/// The one command the captain runs after install (bd-0uzrq.3, live leg).
#[test]
#[ignore = "live: needs FLYWHEEL_LIVE_E2E + an explicit throwaway pane"]
fn a_typed_line_defers_the_wake_and_clearing_it_rings() {
    let Some((session, block, pin, project, _as)) = gate() else {
        eprintln!(
            "live_doorbell_operator_guard: skipped — set FLYWHEEL_LIVE_E2E=1 and the \
             FLYWHEEL_LIVE_SESSION/BLOCK/PROJECT variables to run it"
        );
        return;
    };

    // 1. The operator has half a line: type it, never press Enter.
    rex(&["send", "-s", &session, "--block", &block, "half typed line"]);
    std::thread::sleep(std::time::Duration::from_millis(800));

    // 2. The send door must DEFER — nothing is typed into their line.
    wait_idle(&project);
    let deferred = send_probe(&pin, &project, "live probe: deferral");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&deferred.stdout),
        String::from_utf8_lossy(&deferred.stderr)
    );
    // The send door returns a typed deferral (it does not silently queue like
    // the mail-wake door): nothing was typed, and the command exits non-zero.
    assert!(
        text.contains("deferred: operator is typing"),
        "the send door must refuse an operator's half-typed line: {text}"
    );
    assert!(
        !deferred.status.success(),
        "a deferred send must not report dispatch: {text}"
    );
    let before = pane_capture(&session, &block);
    if !before.contains("half typed line") || before.contains("live probe: deferral") {
        eprintln!("=== bd-e4gal SCREEN AT SEND ===\n{before}\n=== END SCREEN ===");
        let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            format!("{}/.local/share", std::env::var("HOME").unwrap_or_default())
        });
        let path = format!("{base}/flywheel/agents/{project}.json");
        eprintln!(
            "=== bd-e4gal REGISTRY AT SEND ({path}) ===\n{}",
            std::fs::read_to_string(&path).unwrap_or_else(|e| format!("<{e}>"))
        );
    }
    assert!(
        before.contains("half typed line") && !before.contains("live probe: deferral"),
        "the operator's half line stayed intact and the wake never landed on screen: {before}"
    );

    // 3. Clear the line (ctrl+u) and the same send lands.
    rex(&["send-key", "-s", &session, "--block", &block, "ctrl+u"]);
    std::thread::sleep(std::time::Duration::from_millis(800));
    wait_idle(&project);
    let landed = send_probe(&pin, &project, "live probe: ring");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&landed.stdout),
        String::from_utf8_lossy(&landed.stderr)
    );
    assert!(
        !text.contains("deferred") && text.contains("\"dispatched\": true"),
        "the guard ALLOWS the send once the line is clear: {text}"
    );
    assert!(landed.status.success(), "the send must be accepted: {text}");
    // `landed:false` is send.rs's agent-settle receipt (GS-037); a bare zsh
    // pane never settles an agent. The screen is the proof the text landed.
    let after = pane_capture(&session, &block);
    assert!(
        after.contains("live probe: ring"),
        "the wake must be typed onto the actual pane: {after}"
    );
}

#[cfg(test)]
mod send_door_tests {
    use super::send_probe_with;

    /// bd-oo7zi: flywheel names a rex pane by its BLOCK LABEL, so the harness
    /// must rename the throwaway block to the pin before `send --to PIN` works.
    #[test]
    fn the_harness_names_the_throwaway_block_after_the_pin() {
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/e2e-live-five.sh"
        ));
        assert!(
            script.contains("block rename -s \"$SESSION\" \"$BLOCK_ID\" \"$PIN\""),
            "the block (not the window) must be renamed to the pin"
        );
    }

    /// bd-oo7zi: the live wake drives the `send.rs` door, by pane id, with no
    /// mail half. A fake binary records the exact argv; no live plane needed.
    #[test]
    fn the_live_wake_uses_send_with_a_pane_id_target() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("fw-send-target-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("flywheel");
        let calls = dir.join("calls.log");
        std::fs::write(
            &bin,
            format!(
                "#!/usr/bin/env bash\necho \"fw $*\" >>{}\nexit 0\n",
                calls.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&bin).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&bin, p).unwrap();
        }
        let out = send_probe_with(&bin, "block:01live", "e2e-live-five", "live probe: ring");
        assert!(out.status.success());
        let call = std::fs::read_to_string(&calls).unwrap();
        assert!(
            call.contains("send e2e-live-five live probe: ring --to block:01live"),
            "the send door is targeted at the pane id: {call}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
