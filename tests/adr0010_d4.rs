//! ADR-0010 D4: reservation write-liveness plus 240/3600/3 inspect cadence.
//! Binary-only crate: structural assertions scan source (D0/D3 pattern);
//! behavioral tests live in src/write_liveness.rs unit tests.
//! Test names: adr0010_d4_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d4`.

const DURABLE: &str = include_str!("../src/durable.rs");
const LIVENESS: &str = include_str!("../src/write_liveness.rs");

#[test]
fn adr0010_d4_walk_knobs_present() {
    for knob in [
        "FLYWHEEL_WRITE_WALK_PRUNE",
        "FLYWHEEL_WRITE_WALK_MAXDEPTH",
        "FLYWHEEL_WRITE_WALK_TIMEOUT",
        "FLYWHEEL_STALE_ESCALATE_SECS",
        "FLYWHEEL_BUSY_TURN_MAX_SECS",
        "FLYWHEEL_WEDGE_INSPECT_COUNT",
    ] {
        assert!(
            LIVENESS.contains(knob),
            "write_liveness.rs missing knob {knob}"
        );
    }
    assert!(
        LIVENESS.contains("240"),
        "stale-escalate default 240 must appear"
    );
    assert!(LIVENESS.contains("3600"), "busy-turn 3600 must appear");
}

#[test]
fn adr0010_d4_drain_checks_writes_before_respawn() {
    // The about-to-respawn leaf (drain-expiry arm) walks the bead's
    // reservation globs first; fresh writes defer via re-arm, never kill.
    assert!(
        DURABLE.contains("write_liveness"),
        "drain arm must call into write_liveness"
    );
    assert!(
        LIVENESS.contains("demand-deep-inspection"),
        "wedge path must name demand-deep-inspection in the digest"
    );
    assert!(
        DURABLE.contains("defers"),
        "DrainState must count consecutive write-evidence defers"
    );
    // Failed/timed-out walk fails toward existing behavior (respawn).
    assert!(
        LIVENESS.contains("WalkFailed") || LIVENESS.contains("walk_failed"),
        "walk failure must be an explicit outcome that respawns"
    );
}

#[test]
fn adr0010_d4_never_auto_interrupt() {
    // The defer path skips the kill and re-arms grace; auto-kill stays
    // --force-working (kill keys, not this bead).
    assert!(
        DURABLE.contains("write-evidence"),
        "defer must emit a write-evidence digest, not silence"
    );
    assert!(
        !LIVENESS.contains("interrupt"),
        "write_liveness must not interrupt panes"
    );
}
