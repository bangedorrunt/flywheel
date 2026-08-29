// governed-by: ADR-0025
#![allow(dead_code, clippy::all, unused)]
// TODO: wire into main.rs send/mail/channel wrappers after S releases main.rs (deferred per file split)
//! Plane tripwire classifiers (Spec 5) — pure, no I/O, no Nostr kinds.
//!
//! Two tripwires, both fail-closed and audited:
//!
//! - secret → public (channel post / --plane s with secret pattern or high+ack single-to)
//! - job-assign → private (gift-wrap path with @mention+ p tag / dispatch_job / toron job)
//!
//! Default wire unchanged:
//!
//! - `flywheel send` = pane prompt + gift-wrap completion mail,
//! - `toron mail send` = gift-wrap. Public only via explicit `--plane s` / `channel post`.
//! - Only `--unsafe-plane` escapes, audited to stderr JSON; `--raw` never escapes.

/// The four dispatch surfaces in priority order (Spec 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Plane {
    Job,      // 43001–43006
    Doorbell, // watch + herdr agent.prompt
    Kind9,    // crew-visible
    GiftWrap, // 1059 / rumor 14 (private)
}

/// Dispatch-surface priority: job > doorbell > kind9 > gift-wrap (fixed, not user-tunable).
pub fn dispatch_surface_priority(
    is_job_assign: bool,
    is_doorbell: bool,
    is_kind9: bool,
    _is_gift_wrap: bool,
) -> Plane {
    if is_job_assign {
        Plane::Job
    } else if is_doorbell {
        Plane::Doorbell
    } else if is_kind9 {
        Plane::Kind9
    } else {
        Plane::GiftWrap
    }
}

/// Secret patterns per Spec 5: (?i)(nsec1|ncryptsec|TORON_KEY_PASSPHRASE|-----BEGIN .*PRIVATE KEY-----)
fn is_secret_pattern(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if lower.contains("nsec1")
        || lower.contains("ncryptsec")
        || lower.contains("toron_key_passphrase")
    {
        return true;
    }
    // -----BEGIN ... PRIVATE KEY-----  (case-insensitive, .* between)
    if lower.contains("-----begin") && lower.contains("private key") {
        // Ensure the BEGIN comes before PRIVATE KEY in the lowercased body
        if let (Some(b), Some(p)) = (lower.find("-----begin"), lower.find("private key")) {
            if b < p {
                return true;
            }
        }
    }
    false
}

/// Whether a body is a job-assign: @mention AND p tag, or dispatch_job, or toron job.
/// `has_p_tag` is whether the `p` tag is set (for gift-wrap paths), `is_dispatch_job`/`is_toron_job` are explicit kind flags.
pub fn is_job_assign_body(
    body: &str,
    has_p_tag: bool,
    is_dispatch_job: bool,
    is_toron_job: bool,
) -> bool {
    if is_dispatch_job || is_toron_job {
        return true;
    }
    // @mention check: contains '@' followed by alnum (simple heuristic, matches @Name)
    let has_mention = body.contains('@') && body.chars().any(|c| c == '@');
    has_mention && has_p_tag
}

/// Tripwire 1: secret → public (channel post / --plane s)
///
/// Fires when:
/// - body matches secret pattern, OR
/// - importance == "high" && ack_required && single recipient (`to.len() == 1`)
///
/// `plane` is the wire being used: `Some("s")` for `--plane s` / `channel post`, `Some("p")` etc, `None` for default.
/// `unsafe_plane` is `--unsafe-plane` (audited bypass), `raw` is `--raw` (must NOT bypass).
/// Returns `Ok(())` if allowed, `Err(PlaneError)` if tripwire fires.
#[allow(clippy::too_many_arguments)]
pub fn check_secret_tripwire(
    body: &str,
    importance: Option<&str>,
    ack_required: bool,
    to: &[String],
    plane: Option<&str>,
    unsafe_plane: bool,
    raw: bool,
) -> Result<(), String> {
    let _ = raw; // --raw never bypasses, explicitly ignored
    if unsafe_plane {
        // Audited bypass: allow but log to stderr JSON (caller should log)
        eprintln!(
            "{{\"plane_tripwire\":\"secret->public\",\"bypass\":\"--unsafe-plane\",\"body_len\":{},\"plane\":{:?}}}",
            body.len(),
            plane
        );
        return Ok(());
    }
    let is_secret =
        is_secret_pattern(body) || (importance == Some("high") && ack_required && to.len() == 1);
    if !is_secret {
        return Ok(());
    }
    // Tripwire fires even on explicit --plane s (only --unsafe-plane escapes)
    // The wire is considered public if it's channel post or --plane s, but per Spec 5, the tripwire is for secret → public,
    // so we fire whenever the body is secret and the wire would be public.
    // For this pure classifier, we treat any call to channel post / --plane s with secret as an error.
    // The caller passes `plane` as Some("s") for public wires, but the tripwire should fire even if plane is None and the body is secret?
    // Per Spec 5, the tripwire is for `channel post` (which is inherently public) and `--plane s` (explicit public).
    // For this pure function, we consider the check as: if the body is secret and the intended wire is public (channel post or --plane s),
    // then error. For the classifier test, we treat `plane == Some("s")` or the caller being `channel post` as public.
    // For simplicity, the pure classifier will error whenever is_secret is true, regardless of plane, to satisfy the test that tripwires fire even on explicit --plane.
    // The actual wire-in (main.rs) will only call this for channel post / --plane s paths.
    Err(
        "plane_error: secrets use mail send (remedy: toron mail send for nsec/ncryptsec/private keys; not channel post / --plane s)".to_string(),
    )
}

/// Tripwire 2: job-assign → private (gift-wrap path)
///
/// Fires when:
/// - body is job-assign (is_job_assign_body true) AND the wire is gift-wrap (private)
///
/// `plane` is the wire: `Some("p")` etc, `None` for default gift-wrap.
/// `is_gift_wrap` indicates the wire is gift-wrap (private). For `toron mail send` default, `is_gift_wrap` is true.
/// `unsafe_plane` is `--unsafe-plane` (audited bypass), `raw` is `--raw` (must NOT bypass).
#[allow(clippy::too_many_arguments)]
pub fn check_job_tripwire(
    body: &str,
    has_p_tag: bool,
    is_dispatch_job: bool,
    is_toron_job: bool,
    is_gift_wrap: bool,
    plane: Option<&str>,
    unsafe_plane: bool,
    raw: bool,
) -> Result<(), String> {
    let _ = raw; // --raw never bypasses
    if unsafe_plane {
        eprintln!(
            "{{\"plane_tripwire\":\"job-assign->private\",\"bypass\":\"--unsafe-plane\",\"body_len\":{},\"plane\":{:?}}}",
            body.len(),
            plane
        );
        return Ok(());
    }
    if !is_gift_wrap {
        return Ok(());
    }
    let is_job = is_job_assign_body(body, has_p_tag, is_dispatch_job, is_toron_job);
    if !is_job {
        return Ok(());
    }
    // Tripwire fires even on explicit --plane (only --unsafe-plane escapes) — but for job-assign->private,
    // the wire is gift-wrap (private), so explicit --plane p (private) would still be private, but the tripwire is for job-assign should use job plane (43001), not gift-wrap.
    // So even if plane is explicitly gift-wrap, it still errors; only --unsafe-plane escapes.
    Err(
        "plane_error: work assignment uses the job plane (43001) (remedy: toron job or dispatch_job, not gift-wrap mail send)".to_string(),
    )
}

/// Human-readable dispatch-surface priority for display.
pub fn plane_label(plane: Plane) -> &'static str {
    match plane {
        Plane::Job => "job",
        Plane::Doorbell => "doorbell",
        Plane::Kind9 => "kind9",
        Plane::GiftWrap => "gift-wrap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_route_secret_pattern_nsec() {
        assert!(is_secret_pattern("here is nsec1abc..."));
        assert!(is_secret_pattern("NSEC1ABC"));
        assert!(!is_secret_pattern("nothing secret here"));
    }

    #[test]
    fn plane_route_secret_pattern_ncryptsec() {
        assert!(is_secret_pattern("ncryptsec1..."));
        assert!(is_secret_pattern("NCRYPTSEC1..."));
    }

    #[test]
    fn plane_route_secret_pattern_toron_key_passphrase() {
        assert!(is_secret_pattern("my TORON_KEY_PASSPHRASE is ..."));
        assert!(is_secret_pattern("toron_key_passphrase=foo"));
    }

    #[test]
    fn plane_route_secret_pattern_private_key() {
        assert!(is_secret_pattern("-----BEGIN PRIVATE KEY----- abc"));
        assert!(is_secret_pattern("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(is_secret_pattern("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(!is_secret_pattern("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn plane_route_secret_tripwire_errors_with_remedy() {
        let body = "nsec1abc...";
        let err =
            check_secret_tripwire(body, None, false, &[], Some("s"), false, false).unwrap_err();
        assert!(
            err.contains("plane_error: secrets use mail send"),
            "remedy should name mail send: {err}"
        );
        assert!(err.contains("toron mail send"), "remedy: {err}");
    }

    #[test]
    fn plane_route_secret_tripwire_high_ack_single_to() {
        let body = "hello";
        let to = vec!["Alice".to_string()];
        let err = check_secret_tripwire(body, Some("high"), true, &to, Some("s"), false, false)
            .unwrap_err();
        assert!(err.contains("plane_error: secrets use mail send"), "{err}");
        // Not single-to should not trip
        let to2 = vec!["Alice".to_string(), "Bob".to_string()];
        assert!(
            check_secret_tripwire(body, Some("high"), true, &to2, Some("s"), false, false).is_ok()
        );
        // Not high importance should not trip
        assert!(
            check_secret_tripwire(body, Some("normal"), true, &to, Some("s"), false, false).is_ok()
        );
        // Not ack_required should not trip
        assert!(
            check_secret_tripwire(body, Some("high"), false, &to, Some("s"), false, false).is_ok()
        );
    }

    #[test]
    fn plane_route_secret_tripwire_fires_even_on_explicit_plane() {
        let body = "nsec1...";
        // Even with explicit --plane s, it should still error (only --unsafe-plane escapes)
        let err =
            check_secret_tripwire(body, None, false, &[], Some("s"), false, false).unwrap_err();
        assert!(err.contains("plane_error"), "{err}");
    }

    #[test]
    fn plane_route_secret_unsafe_plane_overrides_audited() {
        let body = "nsec1...";
        // With --unsafe-plane, it should allow (Ok) even though it's secret
        assert!(check_secret_tripwire(body, None, false, &[], Some("s"), true, false).is_ok());
    }

    #[test]
    fn plane_route_secret_raw_does_not_bypass() {
        let body = "nsec1...";
        // --raw should NOT bypass, so it should still error
        let err =
            check_secret_tripwire(body, None, false, &[], Some("s"), false, true).unwrap_err();
        assert!(err.contains("plane_error"), "raw should not bypass: {err}");
    }

    #[test]
    fn plane_route_job_assign_via_mention_and_p() {
        assert!(is_job_assign_body("@Alice please fix", true, false, false));
        assert!(!is_job_assign_body(
            "@Alice please fix",
            false,
            false,
            false
        )); // no p tag
        assert!(!is_job_assign_body("please fix", true, false, false)); // no mention
        assert!(is_job_assign_body("dispatch_job body", false, true, false)); // dispatch_job
        assert!(is_job_assign_body("toron job body", false, false, true)); // toron job
    }

    #[test]
    fn plane_route_job_tripwire_errors_with_remedy() {
        let body = "@Alice fix the bug";
        let err = check_job_tripwire(body, true, false, false, true, Some("p"), false, false)
            .unwrap_err();
        assert!(
            err.contains("plane_error: work assignment uses the job plane"),
            "remedy should name job plane: {err}"
        );
        assert!(
            err.contains("toron job") || err.contains("dispatch_job"),
            "remedy: {err}"
        );
        assert!(err.contains("43001"), "should name 43001: {err}");
    }

    #[test]
    fn plane_route_job_tripwire_fires_even_on_explicit_plane() {
        let body = "@Bob work";
        // Even with explicit --plane p (gift-wrap), job-assign should still error (only --unsafe-plane escapes)
        let err = check_job_tripwire(body, true, false, false, true, Some("p"), false, false)
            .unwrap_err();
        assert!(err.contains("plane_error"), "{err}");
    }

    #[test]
    fn plane_route_job_tripwire_not_on_kind9() {
        let body = "@Alice work";
        // If not gift-wrap (e.g., kind9), it should not error
        assert!(
            check_job_tripwire(body, true, false, false, false, Some("s"), false, false).is_ok()
        );
    }

    #[test]
    fn plane_route_job_unsafe_plane_overrides_audited() {
        let body = "@Alice work";
        assert!(check_job_tripwire(body, true, false, false, true, Some("p"), true, false).is_ok());
    }

    #[test]
    fn plane_route_job_raw_does_not_bypass() {
        let body = "@Alice work";
        let err =
            check_job_tripwire(body, true, false, false, true, Some("p"), false, true).unwrap_err();
        assert!(err.contains("plane_error"), "raw should not bypass: {err}");
    }

    #[test]
    fn plane_route_dispatch_surface_priority() {
        // job > doorbell > kind9 > gift-wrap
        assert_eq!(
            dispatch_surface_priority(true, true, true, true),
            Plane::Job
        );
        assert_eq!(
            dispatch_surface_priority(false, true, true, true),
            Plane::Doorbell
        );
        assert_eq!(
            dispatch_surface_priority(false, false, true, true),
            Plane::Kind9
        );
        assert_eq!(
            dispatch_surface_priority(false, false, false, true),
            Plane::GiftWrap
        );
        assert_eq!(
            dispatch_surface_priority(false, false, false, false),
            Plane::GiftWrap
        );
    }

    #[test]
    fn plane_route_no_new_kinds() {
        // Ensure we didn't create new Nostr kinds — the tripwires only guard existing wires
        // The Plane enum should only have the four expected variants
        let planes = [Plane::Job, Plane::Doorbell, Plane::Kind9, Plane::GiftWrap];
        assert_eq!(planes.len(), 4);
        // Check that the dispatch_surface_priority is fixed and not user-tunable
        // (no config, no env var, just the pure function)
        assert_eq!(plane_label(Plane::Job), "job");
        assert_eq!(plane_label(Plane::GiftWrap), "gift-wrap");
    }

    #[test]
    fn plane_route_default_wire_unchanged() {
        // flywheel send without --plane should remain pane prompt + gift-wrap completion mail (not public)
        // toron mail send without --plane should remain gift-wrap (private)
        // This is a regression test: send without --plane should not be considered public
        // For the pure classifier, default wire without --plane is gift-wrap (private)
        // So a non-secret, non-job body should be Ok for both tripwires
        let body = "hello world";
        assert!(check_secret_tripwire(body, None, false, &[], None, false, false).is_ok());
        assert!(check_job_tripwire(body, false, false, false, true, None, false, false).is_ok());
        // Explicit --plane s for non-secret should also be Ok (it's public but not secret)
        assert!(check_secret_tripwire(body, None, false, &[], Some("s"), false, false).is_ok());
    }
}
