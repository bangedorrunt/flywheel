//! Captain classifier (pre-invoke). Pure: no IO.
//!
//! Picks ONE primitive from `target` + `snap` using the captain-selector
//! table (docs/agent-instructions/orchestration.md), evaluated in table
//! order — first match wins. Engine control-flow and the durable loop stay
//! untouched. `flywheel decide` prints the Decision; the captain runs argv.
//!
//! governed-by: ADR-0005/0010/0011/0012, bd-5inr

/// Live facts the classifier may read. Absent facts stay zero/false.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub ready_bead_count: u32,
    pub profiled_panes: u32,
    pub launch_pending: u32,
    pub contention: bool,
}

/// One classified command. `primitive` is the stable id; `argv` is what to run.
/// Angle-bracket argv slots (`<role>`, `<full-name>`) are filled by the caller
/// (captain / `flywheel decide`); the classifier cannot know them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub primitive: &'static str,
    pub reason: String,
    pub argv: Vec<String>,
}

fn has(t: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| t.contains(n))
}

fn dec(primitive: &'static str, reason: &str, argv: &[&str]) -> Decision {
    Decision {
        primitive,
        reason: reason.to_string(),
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Classify `target` + `snap` into one primitive. No harness special case.
/// Table order (first match wins):
///   1 audit|read-only|analyze (not implement/fix) → captain-audit
///   2 ready beads + empty|work|loop|beads (not implement/fix) → loop-work
///   3 implement/fix + beads|tracker|gate (not "no tracker") → steer
///   4 implement/fix + no tracker|direct → steer-direct
///   5 compose|catalog miss|parts: → compose-parts
///   6 shape unknown|improvise|author workflow → compose-agent
///   7 contention|coalition|shared-file (target or snap) → coalition-form
///   8 one pane|watch me|single agent → spawn-send
///   9 cron|schedule|nightly → cron-start
/// Unmatched targets fall back to steer (the catalog loudly rejects unknown
/// shapes — never a silent no-op).
pub fn decide(target: &str, snap: &Snapshot) -> Decision {
    let t = target.to_ascii_lowercase();
    let implement = has(&t, &["implement", "fix"]);

    // 1. Read-only analysis wins over everything (never for implement/fix).
    if has(&t, &["audit", "read-only", "analyze"]) && !implement {
        return dec(
            "captain-audit",
            "read-only analysis — captain-audit headless",
            &[
                "flywheel",
                "workflow",
                "run",
                "captain-audit",
                "--execute",
                "--mode",
                "headless",
            ],
        );
    }
    // 2. Ready beads + work-ish intent → run the loop, not hand-rolled sends.
    if snap.ready_bead_count > 0
        && !implement
        && (t.trim().is_empty() || has(&t, &["work", "loop", "beads"]))
    {
        return dec(
            "loop-work",
            &format!("{} ready beads — loop run --work", snap.ready_bead_count),
            &["flywheel", "loop", "run", "--work"],
        );
    }
    // 3. Implement/fix with a tracker/gate → steer (plan gate).
    if implement && has(&t, &["beads", "tracker", "gate"]) && !has(&t, &["no tracker"]) {
        return dec(
            "steer",
            "implement + tracker/gate — steer",
            &["flywheel", "steer", target],
        );
    }
    // 4. Implement/fix, no tracker / direct → steer --direct.
    if implement && has(&t, &["no tracker", "direct"]) {
        return dec(
            "steer-direct",
            "implement, no tracker — steer --direct",
            &["flywheel", "steer", target, "--direct"],
        );
    }
    // 5. Catalog cannot express it → compose --parts.
    if has(&t, &["compose", "catalog miss", "parts:"]) {
        return dec(
            "compose-parts",
            "catalog cannot express the named parts — compose --parts",
            &["flywheel", "workflow", "compose", target, "--parts"],
        );
    }
    // 6. Shape unknown → planner arm (lint is the gate).
    if has(&t, &["shape unknown", "improvise", "author workflow"]) {
        return dec(
            "compose-agent",
            "shape unknown — planner arm, lint is the gate",
            &["flywheel", "workflow", "compose", target, "--agent"],
        );
    }
    // 7. Shared-file contention → coalition (pane-crew-only).
    if snap.contention || has(&t, &["contention", "coalition", "shared-file"]) {
        return dec(
            "coalition-form",
            "file contention — coalition, not a solo dispatch",
            &["flywheel", "coalition", "form"],
        );
    }
    // 8. One pane, captain watches → spawn --profile; NEVER --oc; --to full name.
    if has(&t, &["one pane", "watch me", "single agent"]) {
        return dec(
            "spawn-send",
            "one pane — spawn --profile <role> --count 1, then send --to <full-name> --wait (never --oc)",
            &[
                "flywheel",
                "spawn",
                "--profile",
                "<role>",
                "--count",
                "1",
                "send",
                "--to",
                "<full-name>",
                "--wait",
            ],
        );
    }
    // 9. Scheduled → cron YAML + cron start (cron run is dry-run).
    if has(&t, &["cron", "schedule", "nightly"]) {
        return dec(
            "cron-start",
            "scheduled work — cron YAML + cron start",
            &["flywheel", "cron", "start"],
        );
    }
    // Fallback: forward to the catalog; it rejects unknown shapes loudly.
    dec(
        "steer",
        "default — steer from catalog",
        &["flywheel", "steer", target],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot::default()
    }

    fn with_beads(n: u32) -> Snapshot {
        Snapshot {
            ready_bead_count: n,
            ..Snapshot::default()
        }
    }

    // Row 1 — audit/read-only/analyze (not implement/fix) → captain-audit.
    #[test]
    fn row1_audit_is_captain_audit() {
        for t in [
            "audit the mailbox layer",
            "read-only review of the loop",
            "analyze the spawn backlog",
        ] {
            // Beads ready must not win over a read-only audit.
            let d = decide(t, &with_beads(5));
            assert_eq!(d.primitive, "captain-audit", "target: {t}");
            assert!(
                d.argv.contains(&"captain-audit".to_string()),
                "argv: {:?}",
                d.argv
            );
            assert!(
                d.argv.contains(&"--mode".to_string()) && d.argv.contains(&"headless".to_string()),
                "headless workflow run: {:?}",
                d.argv
            );
        }
    }

    // Row 1 boundary — audit + implement/fix must NOT classify as audit.
    #[test]
    fn row1_audit_plus_implement_is_steer_not_audit() {
        let d = decide("audit then fix the tracker", &snap());
        assert_ne!(d.primitive, "captain-audit");
        assert_eq!(d.primitive, "steer");
    }

    // Row 2 — ready beads + empty|work|loop|beads → loop-work.
    #[test]
    fn row2_ready_beads_is_loop_work() {
        for t in ["", "work the ready beads", "loop", "beads"] {
            let d = decide(t, &with_beads(3));
            assert_eq!(d.primitive, "loop-work", "target: {t:?}");
            assert!(
                d.argv.windows(2).any(|w| w == ["loop", "run"]),
                "argv: {:?}",
                d.argv
            );
            assert!(d.argv.contains(&"--work".to_string()));
        }
    }

    // Row 2 boundary — no ready beads → not loop-work even for "work".
    #[test]
    fn row2_no_ready_beads_is_not_loop_work() {
        let d = decide("work", &snap());
        assert_ne!(d.primitive, "loop-work");
    }

    // Row 3 — implement/fix + beads|tracker|gate → steer.
    #[test]
    fn row3_implement_with_tracker_is_steer() {
        // Beads ready must not hijack an implement request into loop-work.
        let d = decide("implement the parser with a plan gate", &with_beads(3));
        assert_eq!(d.primitive, "steer");
        assert!(d.argv.contains(&"steer".to_string()));
        assert!(d.argv.contains(&"implement the parser with a plan gate".to_string()));
    }

    // Row 4 — implement/fix + no tracker|direct → steer-direct.
    #[test]
    fn row4_implement_direct_is_steer_direct() {
        for t in ["implement this direct", "fix the widget no tracker"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "steer-direct", "target: {t}");
            assert!(d.argv.contains(&"--direct".to_string()), "argv: {:?}", d.argv);
        }
    }

    // Row 5 — compose|catalog miss|parts: → compose-parts.
    #[test]
    fn row5_compose_parts_is_compose_parts() {
        for t in [
            "compose a pipeline",
            "catalog miss on this",
            "compose parts: scout:fresh-eyes",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "compose-parts", "target: {t}");
            assert!(d.argv.contains(&"--parts".to_string()), "argv: {:?}", d.argv);
        }
    }

    // Row 6 — shape unknown|improvise|author workflow → compose-agent.
    #[test]
    fn row6_shape_unknown_is_compose_agent() {
        for t in ["shape unknown here", "improvise it", "author workflow for ingest"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "compose-agent", "target: {t}");
            assert!(d.argv.contains(&"--agent".to_string()), "argv: {:?}", d.argv);
        }
    }

    // Row 7 — contention|coalition|shared-file (target or snap) → coalition-form.
    #[test]
    fn row7_contention_is_coalition_form() {
        for t in [
            "shared-file contention across crews",
            "coalition for the migration",
            "contention on state.rs",
        ] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "coalition-form", "target: {t}");
        }
        let s = Snapshot {
            contention: true,
            ..snap()
        };
        assert_eq!(decide("ship it", &s).primitive, "coalition-form");
    }

    // Row 8 — one pane|watch me|single agent → spawn-send.
    #[test]
    fn row8_one_pane_is_spawn_send() {
        for t in ["one pane for the oracle", "watch me", "single agent job"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "spawn-send", "target: {t}");
            // MUST be --profile; NEVER add --oc; --to carries a full name.
            assert!(
                d.argv.contains(&"--profile".to_string()),
                "--profile MUST be present: {:?}",
                d.argv
            );
            assert!(
                !d.argv.iter().any(|a| a.contains("--oc")),
                "NEVER add --oc: {:?}",
                d.argv
            );
            let to = d
                .argv
                .iter()
                .position(|a| a == "--to")
                .expect("--to full name present");
            let name = &d.argv[to + 1];
            assert!(
                name.starts_with('<') && name.ends_with('>'),
                "--to must carry a full-name slot: {name}"
            );
        }
    }

    // Row 9 — cron|schedule|nightly → cron-start.
    #[test]
    fn row9_scheduled_is_cron_start() {
        for t in ["cron the weekly report", "schedule the nightly sync", "nightly housekeeping"] {
            let d = decide(t, &snap());
            assert_eq!(d.primitive, "cron-start", "target: {t}");
            assert_eq!(d.argv, vec!["flywheel", "cron", "start"]);
        }
    }

    // Fallback — unmatched targets forward to steer (catalog rejects loudly).
    #[test]
    fn unmatched_target_falls_back_to_steer() {
        let d = decide("zzz something odd", &snap());
        assert_eq!(d.primitive, "steer");
        assert!(d.argv.contains(&"zzz something odd".to_string()));
    }
}
