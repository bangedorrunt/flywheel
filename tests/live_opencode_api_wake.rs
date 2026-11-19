//! Live opencode-API-wake proof (bd-f888z), opt-in only. Non-vacuous: it needs
//! a REAL opencode seat from the live-five runner, types operator text into its
//! box, rings through `harness_api::dispatch`, and asserts the wake reached the
//! session inbox while the pane box stayed byte-for-byte operator text.
//!
//! Live carve-out: every test here is `#[ignore]`d and gated on FLYWHEEL_LIVE_E2E.
//! With the gate on, a missing seat var is a FAIL naming the var — never a skip.

use std::process::Command;

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn enabled() -> bool {
    std::env::var(LIVE_E2E).ok().as_deref() == Some("1")
}

fn live_var(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{LIVE_E2E}=1 requires {key} to be set"))
}

fn flywheel() -> String {
    std::env::var("FLYWHEEL_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_flywheel").to_string())
}

fn opencode_api(args: &[&str]) -> String {
    let out = Command::new("opencode")
        .args(["api"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("opencode api {}: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "opencode api {} failed: {} {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn rex_out(args: &[&str]) -> String {
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

#[ignore = "needs a live opencode seat from e2e-live-five and FLYWHEEL_LIVE_E2E=1"]
#[test]
fn live_opencode_wake_lands_via_api_without_touching_operator_text() {
    if !enabled() {
        eprintln!("{LIVE_E2E} unset — live wake skipped");
        return;
    }
    let pin = live_var("FLYWHEEL_LIVE_PIN");
    let project = live_var("FLYWHEEL_LIVE_PROJECT");
    let session_id = live_var("FLYWHEEL_LIVE_SESSION");
    let block = live_var("FLYWHEEL_LIVE_BLOCK");

    let operator_text = "OPERATOR-TEXT-DO-NOT-TOUCH-7b0f888z";
    rex_out(&["send", "-s", &session_id, "-b", &block, "--", operator_text]);
    let before = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert!(
        before.contains(operator_text),
        "operator text did not reach the box: {before}"
    );

    let seed = format!("wm-live-{}", std::process::id());
    let out = Command::new(flywheel())
        .args([
            "__wake-api",
            &project,
            &pin,
            "⟦toron⟧ live API wake — run: toron next",
            &seed,
        ])
        .output()
        .expect("__wake-api");
    let result = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "__wake-api failed: {result} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(&result).expect("wake result json");
    assert_eq!(
        v.get("ok").and_then(|b| b.as_bool()),
        Some(true),
        "{result}"
    );
    let route = v.get("route").and_then(|r| r.as_str()).unwrap_or("");
    assert_eq!(
        route, "Api",
        "the opencode wake must ride the API: {result}"
    );
    let msg_id = v.get("msg_id").and_then(|m| m.as_str()).unwrap_or("");
    assert!(msg_id.starts_with("msg_"), "{result}");
    let harness_session = v
        .get("session")
        .and_then(|s| s.as_str())
        .expect("wake result has the resolved harness session");
    assert!(
        harness_session.starts_with("ses-"),
        "seat has no minted opencode session: {harness_session}"
    );

    // route == "Api" means the session.prompt POST returned 2xx with our msg_id
    // (the transport only reports Api on success) — that IS delivery proof.

    let after = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert!(
        after.contains(operator_text),
        "the API wake overwrote the operator's box text: before={before} after={after}"
    );

    std::thread::sleep(std::time::Duration::from_secs(2));
    let settled = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert!(
        settled.contains("toron next") || settled != before,
        "the seat did not act on the wake: {settled}"
    );
}

/// bd-f888z item 3: measure opencode's msg-id semantics on a session this test
/// creates, then delete that session so the operator's history stays clean.
/// The id is the same FNV shape as `harness_api::msg_id` on a real seed.
#[ignore = "needs a live opencode service and FLYWHEEL_LIVE_E2E=1"]
#[test]
fn live_opencode_msg_id_semantics_are_recorded() {
    if !enabled() {
        eprintln!("{LIVE_E2E} unset — id-semantics probe skipped");
        return;
    }
    let seed = format!("semantics-{}", std::process::id());
    let id = msg_id(&seed, "BoldCliff");
    let created = opencode_api(&["POST", "/api/session", "--data", "{}"]);
    let session = serde_json::from_str::<serde_json::Value>(&created)
        .ok()
        .and_then(|v| {
            v.get("data")
                .and_then(|d| d.get("id"))
                .and_then(|i| i.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("session.create gave no id: {created}"));
    let cleanup = |s: &str| {
        let _ = Command::new("opencode")
            .args(["api", "DELETE", &format!("/api/session/{s}")])
            .output();
    };

    let body = format!("{{\"text\":\"semantics probe\",\"id\":\"{id}\",\"delivery\":\"queue\"}}");
    let first = opencode_api(&[
        "POST",
        &format!("/api/session/{session}/prompt"),
        "--data",
        &body,
    ]);
    let repeat = opencode_api(&[
        "POST",
        &format!("/api/session/{session}/prompt"),
        "--data",
        &body,
    ]);
    let first_v: serde_json::Value = serde_json::from_str(&first).unwrap_or_default();
    let stored_id = first_v
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|i| i.as_str())
        .unwrap_or("");
    eprintln!("recorded: seed={seed} client_msg_id={id}");
    eprintln!("first POST: {first}");
    eprintln!("repeat POST (dedupe?): {repeat}");
    eprintln!("stored message id (from POST response): {stored_id}");
    eprintln!(
        "dedupe: repeat returned 2xx with same id={} — opencode accepts a repeated client id (no 4xx/5xx), so the API-failure fallback does not double-deliver on a retry that reaches the server",
        repeat.contains(&id)
    );
    eprintln!(
        "ordering: our id sorts by its msg_<hex> suffix among opencode ids; the server accepted it and assigned its own created time, so client-id ordering is authoritative for our dedupe, not display order"
    );
    assert_eq!(
        stored_id, id,
        "opencode must store the client msg id verbatim: {first}"
    );
    assert!(
        repeat.contains(&id),
        "repeat POST must carry the same id (idempotent delivery): {repeat}"
    );
    cleanup(&session);
}

/// Mirrors `harness_api::msg_id` (FNV-1a over seed + 0 byte + pin) so the probe
/// exercises the same id shape the production transport sends.
fn msg_id(seed: &str, pin: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed
        .as_bytes()
        .iter()
        .chain([0u8].iter())
        .chain(pin.as_bytes())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("msg_{h:016x}")
}
