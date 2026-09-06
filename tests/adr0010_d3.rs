//! ADR-0010 D3: Busy|Idle|Unknown|Dead verdicts; kind-9 sparse verbs.
//! Binary-only crate: structural assertions scan source (D0 pattern);
//! behavioral property tests live in src/workspace.rs unit tests.
//! Test names: adr0010_d3_* — filtered by `mbx +nightly-aarch64-apple-darwin test adr0010_d3`.

const WORKSPACE: &str = include_str!("../src/workspace.rs");
const MAIN: &str = include_str!("../src/main.rs");
const TPL: &str = include_str!("../integrations/peer-aware.md.tpl");

#[test]
fn adr0010_d3_presence_is_sourced_verdict_enum() {
    // Exact verdicts with source; old flat states gone as variants.
    for variant in ["Busy", "Idle", "Unknown", "Dead"] {
        assert!(
            WORKSPACE.contains(variant),
            "workspace.rs missing verdict {variant}"
        );
    }
    assert!(
        WORKSPACE.contains("source"),
        "verdicts must carry a source field"
    );
    for gone in ["    Working,", "    Blocked,", "    Done,"] {
        assert!(
            !WORKSPACE.contains(gone),
            "flat state {gone:?} must not survive as a variant"
        );
    }
}

#[test]
fn adr0010_d3_no_consumer_branch_reachable_from_unknown() {
    // The prompt gate never prompts from Unknown; herdr is evidence, never authority.
    assert!(
        WORKSPACE.contains("Some(Presence::Idle { .. }) => true"),
        "prompt gate must prompt only on exact Idle"
    );
    assert!(
        WORKSPACE.contains("| None => false"),
        "prompt gate must refuse Unknown/missing outright, never fall back to herdr"
    );
    assert!(
        WORKSPACE.contains("fn verdict_for"),
        "verdict_for(presence, herdr_status, endpoint_alive) must exist"
    );
    assert!(
        WORKSPACE.contains("herdr-evidence"),
        "herdr-derived verdicts must be labeled evidence, not authority"
    );
    assert!(
        WORKSPACE.contains("never trusted"),
        "native idle distrust must be stated at the verdict site"
    );
}

#[test]
fn adr0010_d3_status_prints_verdict_plus_source() {
    assert!(
        MAIN.contains("status_token()") || MAIN.contains("verdict"),
        "cmd_status must render verdict per pin"
    );
    assert!(
        WORKSPACE.contains("fn status_token"),
        "Presence::status_token must render verdict(source)"
    );
}

#[test]
fn adr0010_d3_template_has_four_verbs_no_progress_coaching() {
    for verb in ["blocked:", "paused:", "needs-decision", "resolved"] {
        assert!(TPL.contains(verb), "template missing verb {verb}");
    }
    assert!(
        TPL.contains("wake log"),
        "template must name kind-9 as wake log"
    );
    assert!(
        !TPL.contains("80% done"),
        "template must not coach prose progress posts"
    );
}
