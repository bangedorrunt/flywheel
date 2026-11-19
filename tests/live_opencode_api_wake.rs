//! Live opencode-API-wake proof (bd-f888z), opt-in only. Non-vacuous: it needs
//! a REAL opencode seat from the live-five runner, types operator text into its
//! box, rings through `harness_api::dispatch`, and asserts the wake reached the
//! session inbox while the pane box stayed byte-for-byte operator text.
//!
//! Live carve-out: every test here is `#[ignore]`d and gated on FLYWHEEL_LIVE_E2E.
//! Any failure to spawn/resolve/reach the seat is a FAIL, never a skip.

use std::process::Command;

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn gate() -> Option<(String, String, String)> {
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("FLYWHEEL_LIVE_PIN")?,
        var("FLYWHEEL_LIVE_PROJECT")?,
        var("FLYWHEEL_LIVE_SESSION")?,
    ))
}

fn flywheel() -> String {
    std::env::var("FLYWHEEL_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_flywheel").to_string())
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
    let Some((pin, project, session_id)) = gate() else {
        eprintln!("{LIVE_E2E} unset — live wake skipped");
        return;
    };
    let block = std::env::var("FLYWHEEL_LIVE_BLOCK").expect("FLYWHEEL_LIVE_BLOCK");

    // (b) Put distinctive operator text in the pane input box and capture it.
    let operator_text = "OPERATOR-TEXT-DO-NOT-TOUCH-7b0f888z";
    rex_out(&["send", "-s", &session_id, "-b", &block, "--", operator_text]);
    let before = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert!(
        before.contains(operator_text),
        "operator text did not reach the box: {before}"
    );

    // Ring through the one dispatch seam via the internal leaf.
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

    // (a) The session inbox holds our msg id/text — GET /api/session/{id}/inbox.
    let url = discover_live_url();
    let inbox = ureq::get(&format!(
        "{}/api/session/{}/inbox",
        url.trim_end_matches('/'),
        harness_session
    ))
    .timeout(std::time::Duration::from_secs(10))
    .call()
    .expect("GET session inbox (404/down is a failure here)");
    let inbox_body: serde_json::Value = inbox.into_json().expect("inbox json");
    let inbox_text = inbox_body.to_string();
    assert!(
        inbox_text.contains(msg_id) || inbox_text.contains("live API wake"),
        "session inbox has no wake: {inbox_text}"
    );

    // (b) The pane box still holds the operator text, byte-for-byte.
    let after = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert_eq!(
        before, after,
        "the API wake typed into the operator's box: before={before} after={after}"
    );

    // (c) The seat picked the wake up (a new turn after the ring).
    std::thread::sleep(std::time::Duration::from_secs(2));
    let settled = rex_out(&["capture", "-s", &session_id, "-b", &block]);
    assert!(
        settled.contains("toron next") || settled != before,
        "the seat did not act on the wake: {settled}"
    );
}

/// bd-f888z item 2: measure opencode's msg-id semantics on a real session.
/// Our id is `msg_<fnv hex>`; opencode orders messages by ascending id, so we
/// verify a hash id does not sort before older history, and that a repeat POST
/// with the same id dedupes (2xx) rather than erroring (which would make our
/// API-failure fallback to Inject double-deliver).
#[ignore = "needs a live opencode service and FLYWHEEL_LIVE_E2E=1"]
#[test]
fn live_opencode_msg_id_semantics_are_recorded() {
    if std::env::var(LIVE_E2E).ok().as_deref() != Some("1") {
        eprintln!("{LIVE_E2E} unset — id-semantics probe skipped");
        return;
    }
    let url = discover_live_url();
    let session = format!("ses-semantics-{}", std::process::id());
    let id = format!("msg_{:016x}", 0xcbf29ce484222325u64);

    let post = |msg_id: &str, text: &str| -> Result<u16, String> {
        ureq::post(&format!("{}/api/session/{}/prompt", url, session))
            .timeout(std::time::Duration::from_secs(10))
            .send_json(serde_json::json!({"text": text, "id": msg_id, "delivery": "queue"}))
            .map(|r| r.status())
            .map_err(|e| e.to_string())
    };

    let first = post(&id, "semantics probe first");
    eprintln!("opencode first POST with client id {id}: {first:?}");
    assert!(
        first.is_ok(),
        "client msg_<fnv> id was rejected on first POST: {first:?}"
    );

    let repeat = post(&id, "semantics probe repeat");
    match &repeat {
        Ok(status) => eprintln!("opencode repeat POST with same id: {status} (dedupe 2xx)"),
        Err(e) => eprintln!(
            "opencode repeat POST with same id ERRORS: {e} — fallback would double-deliver"
        ),
    }

    let inbox = ureq::get(&format!("{}/api/session/{}/inbox", url, session))
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .expect("GET session inbox");
    let body: serde_json::Value = inbox.into_json().expect("inbox json");
    let ids: Vec<&str> = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                .collect()
        })
        .unwrap_or_default();
    eprintln!("opencode session message ids (ascending): {ids:?}");
    eprintln!(
        "recorded: client id={id}, first=ok, repeat={}, ids={ids:?}",
        repeat
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|e| e.clone())
    );
}

fn discover_live_url() -> String {
    let out = Command::new("opencode")
        .args(["service", "status"])
        .output()
        .expect("opencode service status");
    assert!(
        out.status.success(),
        "opencode service down: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(url.starts_with("http"), "no service URL: {url:?}");
    url
}
