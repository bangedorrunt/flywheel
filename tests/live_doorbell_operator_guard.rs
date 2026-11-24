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

/// bd-live-guard-target-readiness: is the NAMED pin idle/done? A whole-project
/// substring check answers for the fleet, not for the pane this leg types into.
pub fn pin_idle(status_json: &str, pin: &str) -> Option<bool> {
    let doc: serde_json::Value = serde_json::from_str(status_json).ok()?;
    let row = doc
        .get("seats")?
        .as_array()?
        .iter()
        .find(|r| r.get("pin").and_then(serde_json::Value::as_str) == Some(pin))?;
    Some(matches!(
        row.get("status").and_then(serde_json::Value::as_str),
        Some("idle" | "done")
    ))
}

/// bd-oo7zi: the live leg exercises `src/cmd/send.rs`'s door
/// (`pane_read::defer_if_operator_typing`) against a REAL screen, addressed by
/// pane id, which is a documented `--to` form. It deliberately skips pin -> pane
/// resolution (spawn rows) and the toron mail half: the strict mail-wake form
/// (`src/mail_wake.rs`, `defer_if_operator_typing_strict`) is covered by unit
/// tests, and a rex block id is not a mail recipient.
/// A freshly spawned seat's arrival turn can exceed two minutes under load.
/// Give it ten minutes before calling setup broken; a setup timeout is named
/// as such, never reported as a guard verdict.
const SETUP_POLL_SECS: u64 = 5;
const SETUP_MAX_POLLS: usize = 120;

#[derive(Debug, PartialEq, Eq)]
enum SetupWait {
    Idle { polls: usize },
    Timeout { polls: usize },
}

fn setup_wait_result(observations: &[Option<bool>]) -> SetupWait {
    for (i, observation) in observations.iter().take(SETUP_MAX_POLLS).enumerate() {
        if *observation == Some(true) {
            return SetupWait::Idle { polls: i + 1 };
        }
    }
    SetupWait::Timeout {
        polls: observations.len().min(SETUP_MAX_POLLS),
    }
}

/// A real harness pane can drift back to `working` between the two probes;
/// wait for idle before either send so the status gate is not what we test.
fn wait_idle(project: &str, pin: &str) -> bool {
    let mut observations = Vec::new();
    for _ in 0..SETUP_MAX_POLLS {
        let out = Command::new(env!("CARGO_BIN_EXE_flywheel"))
            .args(["swarm", "status", project, "--json"])
            .output()
            .expect("flywheel swarm status");
        let body = String::from_utf8_lossy(&out.stdout);
        let idle = pin_idle(&body, pin);
        observations.push(idle);
        if let SetupWait::Idle { .. } = setup_wait_result(&observations) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_secs(SETUP_POLL_SECS));
    }
    eprintln!(
        "SETUP TIMEOUT: target pin {pin} did not become idle in {project} after {}s; \
         guard assertions skipped (this is setup, not a guard verdict)",
        SETUP_MAX_POLLS as u64 * SETUP_POLL_SECS
    );
    false
}

#[cfg(test)]
mod setup_wait_tests {
    use super::*;

    #[test]
    fn first_turn_past_the_old_bound_still_reaches_the_guard() {
        let mut observations = vec![Some(false); 30]; // 30 * 5s = 2.5 min
        observations.push(Some(true));
        assert_eq!(
            setup_wait_result(&observations),
            SetupWait::Idle { polls: 31 }
        );
    }

    #[test]
    fn a_real_setup_timeout_is_named_as_setup_not_a_guard_failure() {
        let observations = vec![Some(false); SETUP_MAX_POLLS];
        assert_eq!(
            setup_wait_result(&observations),
            SetupWait::Timeout {
                polls: SETUP_MAX_POLLS
            }
        );
    }
}

/// Resolve the throwaway block's concrete id from rex's block list, matching
/// by id OR label. Ambiguity/zero matches is a typed failure: a live leg that
/// cannot prove WHICH block it addresses must not type into an arbitrary pane.
fn resolve_block_id(blocks_json: &str, block: &str) -> Result<String, String> {
    let doc: serde_json::Value = serde_json::from_str(blocks_json)
        .map_err(|e| format!("rex block list is not json: {e}"))?;
    let rows = doc
        .get("blocks")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "rex block list has no blocks array".to_string())?;
    let hits: Vec<&str> = rows
        .iter()
        .filter_map(|row| {
            let id = row.get("block_id")?.as_str()?;
            let label = row
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if id == block || label == block {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    match hits.as_slice() {
        [only] => Ok((*only).to_string()),
        [] => Err(format!("no live block matches {block:?}")),
        many => Err(format!("block {block:?} is ambiguous: {many:?}")),
    }
}

/// The send result carries the pane it actually dispatched to. Require it to
/// be the block this test created, so a misdelivery fails loudly instead of
/// showing up as a confusing screen assertion later.
fn assert_send_target(send_stdout: &str, expected_block_id: &str) -> Result<(), String> {
    let doc: serde_json::Value = serde_json::from_str(send_stdout.trim())
        .map_err(|e| format!("send result is not json: {e}"))?;
    // `flywheel send` prints `{"via":"inbox","sent":[rows]}` (comms_send::
    // rows_json); older forms are a bare row or an array. Find the dispatched
    // row's pane_id in any of them.
    let rows: Vec<&serde_json::Value> = if let Some(sent) = doc
        .get("sent")
        .and_then(serde_json::Value::as_array)
        .or_else(|| doc.get("results").and_then(serde_json::Value::as_array))
    {
        sent.iter().collect()
    } else if let Some(arr) = doc.as_array() {
        arr.iter().collect()
    } else {
        vec![&doc]
    };
    let pane = rows
        .iter()
        .filter(|r| {
            r.get("dispatched")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .find_map(|r| r.get("pane_id").and_then(serde_json::Value::as_str))
        .or_else(|| {
            rows.iter()
                .find_map(|r| r.get("pane_id").and_then(serde_json::Value::as_str))
        })
        .ok_or_else(|| format!("send result has no pane_id: {doc}"))?;
    if pane != expected_block_id {
        return Err(format!(
            "send dispatched to pane {pane}, not the test block {expected_block_id}"
        ));
    }
    Ok(())
}

/// What is actually ON the pane, as opposed to send.rs's agent-settle receipt
/// (a bare zsh pane never settles an agent, so `landed:false` says nothing
/// about whether the text reached the screen).
fn pane_capture(session: &str, block_id: &str) -> String {
    rex(&["capture", "-s", session, "-b", block_id])
}

/// bd-live-guard-target-readiness: a failed assertion must not leave a
/// half-typed line for the next live leg sharing this pane.
struct TypedLineCleanup<'a> {
    session: &'a str,
    block: &'a str,
}
impl Drop for TypedLineCleanup<'_> {
    fn drop(&mut self) {
        let _ = Command::new("rex")
            .args([
                "send-key",
                "-s",
                self.session,
                "--block",
                self.block,
                "ctrl+u",
            ])
            .output();
    }
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

    // 0. Prove WHICH block we are about to type into and dispatch to. A stale
    // label or a duplicate block is a hard failure, never a guessed pane.
    let blocks = rex(&["block", "ls", "-s", &session, "--json"]);
    let block_id = resolve_block_id(&blocks, &block)
        .unwrap_or_else(|e| panic!("bd-0kxzw: cannot resolve test block: {e}"));
    eprintln!("bd-0kxzw: session={session} block_arg={block} block_id={block_id}");

    // 1. The operator has half a line: type it, never press Enter.
    rex(&[
        "send",
        "-s",
        &session,
        "--block",
        &block_id,
        "half typed line",
    ]);
    let _typed = TypedLineCleanup {
        session: &session,
        block: &block_id,
    };
    std::thread::sleep(std::time::Duration::from_millis(800));

    // 2. The send door must DEFER — nothing is typed into their line.
    if !wait_idle(&project, &pin) {
        return;
    }
    let deferred = send_probe(&pin, &project, "live probe: deferral");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&deferred.stdout),
        String::from_utf8_lossy(&deferred.stderr)
    );
    // A deferred send has EMPTY stdout (no JSON dispatch row) and a typed
    // refusal on stderr; the pane capture below is its proof that nothing
    // landed. The dispatched pane id is asserted only on the send that does
    // dispatch.
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
    let before = pane_capture(&session, &block_id);
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
    rex(&["send-key", "-s", &session, "--block", &block_id, "ctrl+u"]);
    std::thread::sleep(std::time::Duration::from_millis(800));
    if !wait_idle(&project, &pin) {
        return;
    }
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
    assert_send_target(&String::from_utf8_lossy(&landed.stdout), &block_id)
        .unwrap_or_else(|e| panic!("bd-0kxzw: ring send: {e}"));
    // `landed:false` is send.rs's agent-settle receipt (GS-037); a bare zsh
    // pane never settles an agent. The screen is the proof the text landed.
    // A live agent keeps writing to its own pane, so the wake can be scrolled
    // off by the next prose burst: poll briefly instead of a one-shot capture.
    let mut after = String::new();
    for _ in 0..5 {
        after = pane_capture(&session, &block_id);
        if after.contains("live probe: ring") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    assert!(
        after.contains("live probe: ring"),
        "the wake must be typed onto block {block_id}: {after}"
    );
}

#[cfg(test)]
mod send_door_tests {
    use super::{assert_send_target, pin_idle, resolve_block_id, send_probe_with};

    /// bd-live-guard-target-readiness: a working target and an idle OTHER seat
    /// must NOT make the target ready. The old whole-project substring check
    /// answered true here and let the leg type into a busy pane.
    #[test]
    fn readiness_targets_the_named_pin_not_any_seat() {
        let two_seat = r#"{"seats":[
            {"pin":"Target","status":"working"},
            {"pin":"Other","status":"idle"}]}"#;
        assert_eq!(pin_idle(two_seat, "Target"), Some(false));
        assert_eq!(pin_idle(two_seat, "Other"), Some(true));
        assert_eq!(pin_idle(two_seat, "Missing"), None);
        assert_eq!(pin_idle("not json", "Target"), None);
        let done = r#"{"seats":[{"pin":"Target","status":"done"}]}"#;
        assert_eq!(pin_idle(done, "Target"), Some(true));
    }

    /// bd-0kxzw: the capture target must resolve to exactly one live block; a
    /// stale label or duplicate must fail before any text is typed.
    #[test]
    fn the_capture_block_must_resolve_to_one_live_block() {
        let blocks = r#"{"blocks":[
            {"block_id":"block:a","label":"AmberProbe"},
            {"block_id":"block:b","label":"other"}
        ]}"#;
        assert_eq!(
            resolve_block_id(blocks, "AmberProbe").unwrap(),
            "block:a",
            "a label resolves to its block id"
        );
        assert_eq!(
            resolve_block_id(blocks, "block:b").unwrap(),
            "block:b",
            "an id resolves to itself"
        );
        assert!(
            resolve_block_id(blocks, "missing").is_err(),
            "a stale block must fail loudly"
        );
        let dupe = r#"{"blocks":[
            {"block_id":"block:a","label":"pin"},
            {"block_id":"block:b","label":"pin"}
        ]}"#;
        assert!(
            resolve_block_id(dupe, "pin").is_err(),
            "an ambiguous block must fail loudly"
        );
    }

    /// bd-0kxzw: a send dispatched to a different pane is a misdelivery; the
    /// live leg must name it instead of blaming the screen assertion.
    #[test]
    fn a_send_to_the_wrong_pane_fails_loudly() {
        let good = r#"{"target":"AmberProbe","pane_id":"block:a","dispatched":true}"#;
        assert!(assert_send_target(good, "block:a").is_ok());
        // The real comms shape is {"via":"inbox","sent":[rows]}.
        let wrapped = r#"{"via":"inbox","sent":[{"target":"AmberProbe","pane_id":"block:b","dispatched":false},{"target":"AmberProbe","pane_id":"block:a","dispatched":true}]}"#;
        assert!(assert_send_target(wrapped, "block:a").is_ok());
        let wrong = r#"{"target":"AmberProbe","pane_id":"block:b","dispatched":true}"#;
        let err = assert_send_target(wrong, "block:a").unwrap_err();
        assert!(err.contains("block:b") && err.contains("block:a"), "{err}");
        assert!(
            assert_send_target("not json", "block:a").is_err(),
            "a non-json send result must fail loudly"
        );
    }

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
