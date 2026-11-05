//! bd-hv2j: cross-plane digest names `plane_mismatch`, not `no_route`.
//!
//! Witness with fake backends: an hc-selected host given only `block:`
//! rows reports `plane_mismatch` (never `no_route`), while the same host
//! with matching rows still reports `no_route` only when a pin truly has
//! no pane.

use doorbell_core::wake_core::{
    PanePlane, PresenceVerdict, RouteInput, pane_plane, plane_mismatch_reason,
    route_wake_with_plane, workspace_adopt_refused,
};

fn unroutable_input() -> RouteInput {
    RouteInput {
        pin: "SomePin".to_string(),
        line: "wake line".to_string(),
        urgent: true,
        presence: PresenceVerdict::Idle,
        has_route: false,
        budget_allows: true,
    }
}

#[test]
fn plane_mismatch_is_not_no_route() {
    assert_eq!(
        pane_plane("block:01a17136-a780-714f-9211-b78766ffd515"),
        Some(PanePlane::Rex)
    );
    assert_eq!(pane_plane("w5H:p3"), Some(PanePlane::Hc));
    assert_eq!(pane_plane("flywheel-flywheel-pi2"), None);
    assert_eq!(pane_plane(""), None);

    let rex_rows = ["block:aaa", "block:bbb"];
    let reason = plane_mismatch_reason(PanePlane::Hc, &rex_rows);
    let reason = reason.expect("all-rex rows under an hc view must mismatch");
    assert!(
        reason.contains("plane_mismatch"),
        "typed mismatch line, got: {reason}"
    );
    assert!(
        reason.contains("hc") && reason.contains("rex"),
        "names both planes, got: {reason}"
    );
    assert!(
        workspace_adopt_refused(PanePlane::Hc, &rex_rows),
        "host must refuse a workspace living wholly on another plane"
    );

    let d = route_wake_with_plane(&unroutable_input(), PanePlane::Hc, &rex_rows);
    match d {
        doorbell_core::wake_core::GateDecision::Digest { reason, .. } => {
            assert!(reason.contains("plane_mismatch"), "got: {reason}");
            assert!(
                !reason.contains("no_route") && !reason.contains("no route"),
                "must not blame the mail, got: {reason}"
            );
        }
        other => panic!("unroutable wake digests, got: {other:?}"),
    }

    let plane_rows = ["w5H:p3"];
    assert_eq!(plane_mismatch_reason(PanePlane::Hc, &plane_rows), None);
    assert!(!workspace_adopt_refused(PanePlane::Hc, &plane_rows));
    let d = route_wake_with_plane(&unroutable_input(), PanePlane::Hc, &plane_rows);
    match d {
        doorbell_core::wake_core::GateDecision::Digest { reason, .. } => {
            assert!(
                reason.contains("no route") || reason.contains("no_route"),
                "matching-plane pane-less pin keeps no-route, got: {reason}"
            );
        }
        other => panic!("unroutable wake digests, got: {other:?}"),
    }

    assert_eq!(plane_mismatch_reason(PanePlane::Hc, &[]), None);
}
