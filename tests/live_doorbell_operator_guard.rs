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

/// bd-oo7zi: the throwaway pin must be a REGISTERED toron identity in the
/// e2e project before any wake tries to address it by name. The pane exists
/// in rex; its mail identity does not exist in toron until bootstrap runs.
fn bootstrap_pin(pin: &str, project: &str) {
    let out = Command::new("toron")
        .args(["agent", "bootstrap", "--project", project, "--as", pin])
        .output()
        .expect("toron agent bootstrap");
    assert!(
        out.status.success(),
        "register {pin} in {project} first: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

    bootstrap_pin(&pin, &project);

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

#[cfg(test)]
mod registration_tests {
    use super::bootstrap_pin;

    /// bd-oo7zi: the live leg registers its throwaway pin BEFORE the wake,
    /// and the DEFAULT pin passes toron's REAL validator (the live rerun
    /// failed at line 63 because "LiveFivePin" is three words: my first
    /// fake-toron witness never ran that validator, so it could not see it).
    /// A fake `toron` records the invocation; the validator is the sibling
    /// crate's, not a copy of the rule.
    #[test]
    fn a_wake_registers_a_valid_pin_first() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap();
        let script = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/e2e-live-five.sh"
        ))
        .expect("e2e-live-five.sh");
        let marker = "FLYWHEEL_LIVE_PIN:-";
        let at = script.find(marker).expect("a default pin in the script") + marker.len();
        let pin: String = script[at..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        toron_core::nostr::identity::validate_agent_pin(&pin)
            .expect("the live e2e default must be a real Adjective+Noun pin");
        let dir = std::env::temp_dir().join(format!("fw-pin-register-{}", std::process::id()));
        let bin = dir.join("bin");
        let log = dir.join("calls.log");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            bin.join("toron"),
            format!(
                "#!/usr/bin/env bash\necho \"toron $*\" >>{}\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(bin.join("toron")).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(bin.join("toron"), p).unwrap();
        }
        let old_path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", format!("{}:{}", bin.display(), old_path)) };
        bootstrap_pin(&pin, "e2e-live-five");
        unsafe { std::env::set_var("PATH", old_path) };
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(&format!(
                "toron agent bootstrap --project e2e-live-five --as {pin}"
            )),
            "the pin must be registered before the wake: {calls}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
