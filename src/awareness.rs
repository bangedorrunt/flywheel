//! The fleet awareness digest: the deterministic facts, no agent.
//!
//! The authored `awareness-digest` workflow dispatched a housekeeper every 15
//! minutes to re-read `flywheel status` and `__doorbell --report` with an LLM.
//! Every fact it reported is already local and free, so the cron host computes
//! it here instead: one line per fire in the host log, and a bead (which the
//! beads poller already rings the captain with) only when a conflict APPEARS.
//!
//! Silence is the healthy state. A periodic line with no reader is noise, and
//! a wake every 15 minutes for "all fine" costs tokens — the two things a
//! digest must never do. The rich on-demand views stay `flywheel status` and
//! `flywheel __doorbell --report`.
//!
//! Covered conflicts: dispatcher down with panes, an unanswered doorbell pin,
//! and two pins holding one reservation path. Request-release expiry lives in
//! `request_receipts` and is not folded here yet (shortcut: needs a receipt
//! scan, add it when an expired hold is seen in the wild).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schedule name. The cron host owns this one natively, like the doorbell
/// supervisor: no workflow body, no LLM.
pub const NAME: &str = "awareness-digest";

/// 5-field vixie schedule. Same cadence the authored cron used.
pub const SCHEDULE: &str = "*/15 * * * *";

/// The facts one fire reports. Built only from local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The one line: what an operator would read.
    pub line: String,
    /// `line` WITHOUT the clock. Change detection reads this: a line that
    /// carries `HH:MM` differs on every fire, so comparing `line` would make
    /// the digest speak every 15 minutes forever.
    pub fingerprint: String,
    /// Conflicts worth waking the captain for. Empty = healthy.
    pub conflicts: Vec<String>,
    /// Whether a swarm exists at all. No swarm = nothing to be aware of.
    pub swarm: bool,
}

/// What a fire decides to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing changed and nothing is wrong: write nothing.
    Silent,
    /// New facts, nothing broken: the line is the log entry.
    Log(String),
    /// A conflict appeared that the last fire did not report: log + bead.
    Escalate {
        line: String,
        conflicts: Vec<String>,
        signature: String,
    },
}

/// What the last fire left behind, per project.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Remembered {
    #[serde(default)]
    line: String,
    /// The last fire's timestamp-free fingerprint — what change detection reads.
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    bead: Option<String>,
    /// Whether the last fire saw a swarm. A swarm that VANISHES is news; a
    /// host that never had one is not.
    #[serde(default)]
    swarm: bool,
}

/// Compute the facts for one project from local state.
pub fn facts(cfg: &crate::config::Config, project: &str) -> Result<Facts> {
    let state_dir = crate::config::state_dir();
    let ws_label =
        crate::state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
    // Wave11: label-pinned entry (same single world read as assemble).
    let snap = crate::snapshot::assemble_with_label(cfg, project, &ws_label, unix_secs());

    let agents = snap.pins.len();
    let busy = snap
        .pins
        .iter()
        .filter(|p| p.verdict.starts_with("Busy"))
        .count();
    let idle = snap
        .pins
        .iter()
        .filter(|p| p.verdict.starts_with("Idle"))
        .count();
    let dead = snap
        .pins
        .iter()
        .filter(|p| p.verdict.starts_with("Dead"))
        .count();
    let unknown = agents.saturating_sub(busy + idle + dead);
    let unanswered =
        crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(&state_dir, &ws_label));
    let held: usize = snap.pins.iter().map(|p| p.reservations.len()).sum();
    let swarm = agents > 0;

    let mut conflicts = Vec::new();
    if agents > 0 && snap.dispatcher != "up" {
        conflicts.push(format!(
            "dispatcher {} with {agents} pane(s)",
            snap.dispatcher
        ));
    }
    if !unanswered.is_empty() {
        conflicts.push(format!("unanswered doorbell: {}", unanswered.join(",")));
    }
    for (path, holders) in overlaps(&snap.pins) {
        conflicts.push(format!(
            "reservation overlap: {path} held by {}",
            holders.join(",")
        ));
    }

    let doorbell = if unanswered.is_empty() {
        "ok".to_string()
    } else {
        format!("unanswered pin={}", unanswered.join(","))
    };
    let fingerprint = format!(
        "dispatcher: {} · doorbell: {doorbell} · \
         agents {agents} (busy {busy}, idle {idle}, dead {dead}, unknown {unknown}) · held {held}",
        snap.dispatcher,
    );
    let line = format!(
        "awareness {project} {} · {fingerprint}",
        crate::cron::clock_hhmm(unix_secs()),
    );
    Ok(Facts {
        line,
        fingerprint,
        conflicts,
        swarm,
    })
}

/// One reservation path claimed by two or more pins, sorted for a stable
/// signature.
fn overlaps(pins: &[crate::snapshot::PinRow]) -> Vec<(String, Vec<String>)> {
    let mut by_path: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for pin in pins {
        for path in &pin.reservations {
            by_path
                .entry(path.as_str())
                .or_default()
                .push(pin.pin.as_str());
        }
    }
    by_path
        .into_iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(path, mut holders)| {
            holders.sort_unstable();
            (
                path.to_string(),
                holders.into_iter().map(String::from).collect(),
            )
        })
        .collect()
}

/// Decide what this fire does. Pure: the state file is the only input beyond
/// the facts, so a test can pin every arm without a store.
pub fn decide(facts: &Facts, remembered: &Remembered) -> Action {
    let mut conflicts = facts.conflicts.clone();
    let prior_swarm = remembered.swarm && !remembered.fingerprint.is_empty();
    if prior_swarm && !facts.swarm {
        conflicts.push("swarm vanished: 0 agents, dispatcher not up".to_string());
    }
    let signature = conflicts.join(" | ");
    if !signature.is_empty() && signature != remembered.signature {
        return Action::Escalate {
            line: facts.line.clone(),
            conflicts,
            signature,
        };
    }
    if !facts.swarm && signature.is_empty() {
        return Action::Silent;
    }
    if facts.fingerprint == remembered.fingerprint {
        return Action::Silent;
    }
    Action::Log(facts.line.clone())
}

/// One digest fire: compute, decide, remember, and escalate a new conflict.
pub async fn tick(cfg: &crate::config::Config, project: &str) -> Result<Option<String>> {
    let gather_cfg = cfg.clone();
    let gather_project = project.to_string();
    let facts = tokio::task::spawn_blocking(move || facts(&gather_cfg, &gather_project))
        .await
        .map_err(|e| anyhow::anyhow!("awareness gather task: {e}"))??;
    let mut remembered = load(project);
    let action = decide(&facts, &remembered);
    match action {
        Action::Silent => {
            remembered.line = facts.line.clone();
            remembered.fingerprint = facts.fingerprint.clone();
            remembered.swarm = facts.swarm;
            save(project, &remembered)?;
            Ok(None)
        }
        Action::Log(line) => {
            remembered.line = line.clone();
            remembered.fingerprint = facts.fingerprint.clone();
            remembered.swarm = facts.swarm;
            remembered.signature.clear();
            save(project, &remembered)?;
            println!("cron {NAME}: {line}");
            Ok(Some(line))
        }
        Action::Escalate {
            line,
            conflicts,
            signature,
        } => {
            // One bead per CONDITION, not per episode: a digest whose conflict
            // set reshuffles must not file the same condition again (bd-i4fqc).
            let mut ids = Vec::new();
            for conflict in &conflicts {
                if let Some(id) = ensure_conflict_bead(project, conflict, &line).await? {
                    ids.push(id);
                }
            }
            let bead = ids.last().cloned();
            remembered.line = line.clone();
            remembered.fingerprint = facts.fingerprint.clone();
            remembered.swarm = facts.swarm;
            remembered.signature = signature;
            remembered.bead = bead.clone();
            save(project, &remembered)?;
            println!(
                "cron {NAME}: {line} — CONFLICT {} (bead {})",
                conflicts.join("; "),
                if ids.is_empty() {
                    "unfiled".to_string()
                } else {
                    ids.join(",")
                }
            );
            Ok(Some(line))
        }
    }
}

/// The durable identity of one conflict: its kind and its subject, with digits
/// collapsed so a drifting count is not mistaken for a new condition.
fn condition_of(conflict: &str) -> String {
    let (kind, detail) = conflict.split_once(':').unwrap_or((conflict, ""));
    let normalize = |text: &str| {
        text.trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect::<String>()
    };
    format!("{}|{}", normalize(kind), normalize(detail))
}

/// The condition an awareness bead was filed for:
/// `awareness: <conflict> in <project>`.
fn title_condition(title: &str, project: &str) -> Option<String> {
    let rest = title.strip_prefix("awareness: ")?;
    let rest = rest.strip_suffix(&format!(" in {project}"))?;
    Some(condition_of(rest))
}

/// The OPEN awareness bead already filed for this condition, if one exists.
fn open_bead_for(rows: &[Value], project: &str, condition: &str) -> Option<String> {
    rows.iter().find_map(|row| {
        let title = row.get("title")?.as_str()?;
        if title_condition(title, project).as_deref() != Some(condition) {
            return None;
        }
        row.get("id")?.as_str().map(String::from)
    })
}

/// File the escalation bead for ONE conflict, or update the open bead that
/// already covers it. The beads poller rings an idle captain on a ready bead,
/// so this is the one wake path that costs no tokens; a repeat observation is a
/// comment, not a second bead.
async fn ensure_conflict_bead(project: &str, conflict: &str, line: &str) -> Result<Option<String>> {
    let condition = condition_of(conflict);
    if let Ok(raw) = crate::br::run_br(None, &["list", "--json", "--status", "open"])
        && let Ok(rows) = crate::br::parse_issue_rows_json(&raw, "awareness list")
        && let Some(id) = open_bead_for(&rows, project, &condition)
    {
        let note = format!("Still observed for the same condition.\n\n{line}");
        if let Err(e) = crate::br::run_br(None, &["comments", "add", &id, "--message", &note]) {
            eprintln!("cron {NAME}: could not update bead {id} ({e})");
        }
        return Ok(Some(id));
    }
    let title = format!("awareness: {conflict} in {project}");
    let description = format!(
        "The native awareness digest saw a conflict.\n\n{line}\n\n\
         conflict: {conflict}\n\n\
         This bead is filed once per CONDITION: while the same condition \
         persists the digest comments here instead of filing another bead, and \
         a cleared conflict resets it.\n\n\
         Diagnose before acting: `flywheel status {project}` and \
         `flywheel __doorbell --project {project} --report`."
    );
    let raw = match crate::br::run_br(
        None,
        &[
            "create",
            &title,
            "--type",
            "bug",
            "--priority",
            "1",
            "--description",
            &description,
            "--verify",
            &format!("flywheel __doorbell --project {project} --report"),
            "--principle",
            "wake-on-conflict — a deaf fleet must ring once, not log every 15 minutes",
            "--labels",
            &format!("awareness,{project}"),
        ],
    ) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("cron {NAME}: br unavailable ({e}); conflict not filed");
            return Ok(None);
        }
    };
    let id = serde_json::from_str::<serde_json::Value>(raw.trim())
        .ok()
        .and_then(|v| {
            v.get("issue")
                .and_then(|i| i.get("id"))
                .or_else(|| v.get("id"))
                .and_then(|i| i.as_str())
                .map(String::from)
        });
    if id.is_none() {
        eprintln!(
            "cron {NAME}: br create answered without an id: {}",
            raw.trim()
        );
    }
    Ok(id)
}

fn state_file(project: &str) -> std::path::PathBuf {
    crate::config::state_dir()
        .join("awareness")
        .join(format!("{project}.json"))
}

fn load(project: &str) -> Remembered {
    std::fs::read_to_string(state_file(project))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save(project: &str, remembered: &Remembered) -> Result<()> {
    let path = state_file(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(remembered)?)?;
    Ok(())
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn facts_of(line: &str, conflicts: &[&str], swarm: bool) -> Facts {
        Facts {
            line: line.to_string(),
            fingerprint: line.to_string(),
            conflicts: conflicts.iter().map(|c| c.to_string()).collect(),
            swarm,
        }
    }

    /// A healthy swarm is logged once, then stays quiet until the line moves.
    #[test]
    fn unchanged_facts_stay_silent() {
        let facts = facts_of("awareness p · dispatcher: up", &[], true);
        assert_eq!(
            decide(&facts, &Remembered::default()),
            Action::Log(facts.line.clone())
        );
        let remembered = Remembered {
            line: "a previous clock reading".into(),
            fingerprint: facts.fingerprint.clone(),
            swarm: true,
            ..Default::default()
        };
        assert_eq!(decide(&facts, &remembered), Action::Silent);
    }

    /// The clock in the line must not count as a fact change, or the digest
    /// speaks every 15 minutes for nothing.
    #[test]
    fn a_new_clock_with_the_same_facts_is_silent() {
        let first = facts_of("awareness p 08:15 · agents 1", &[], true);
        let mut later = facts_of("awareness p 08:30 · agents 1", &[], true);
        later.fingerprint = first.fingerprint.clone();
        let remembered = Remembered {
            line: first.line.clone(),
            fingerprint: first.fingerprint.clone(),
            swarm: true,
            ..Default::default()
        };
        assert_eq!(decide(&later, &remembered), Action::Silent);
    }

    /// No spawned swarm = no digest at all. An idle host is not an outage.
    #[test]
    fn an_idle_host_never_speaks() {
        let facts = facts_of("awareness p · dispatcher: absent", &[], false);
        assert_eq!(decide(&facts, &Remembered::default()), Action::Silent);
    }

    /// A new conflict escalates once; the same conflict on the next fire does
    /// not file a second bead, and a line change alone is only a log entry.
    #[test]
    fn a_conflict_escalates_once_per_episode() {
        let facts = facts_of(
            "awareness p · doorbell: unanswered pin=X",
            &["unanswered doorbell: X"],
            true,
        );
        let Action::Escalate { signature, .. } = decide(&facts, &Remembered::default()) else {
            panic!("first sighting must escalate");
        };
        let remembered = Remembered {
            line: facts.line.clone(),
            fingerprint: facts.fingerprint.clone(),
            signature,
            bead: Some("bd-1".into()),
            swarm: true,
        };
        assert_eq!(decide(&facts, &remembered), Action::Silent);
        let worse = facts_of(
            "awareness p · dispatcher: down",
            &["unanswered doorbell: X", "dispatcher down with 2 pane(s)"],
            true,
        );
        assert!(matches!(
            decide(&worse, &remembered),
            Action::Escalate { .. }
        ));
    }

    /// A swarm that disappears is an escalation, not "idle host, say nothing".
    #[test]
    fn a_vanished_swarm_rings_once() {
        let gone = facts_of("awareness p · agents 0", &[], false);
        let had_swarm = Remembered {
            line: "awareness p · agents 3".into(),
            fingerprint: "awareness p · agents 3".into(),
            swarm: true,
            ..Default::default()
        };
        let Action::Escalate { conflicts, .. } = decide(&gone, &had_swarm) else {
            panic!("a vanished swarm must escalate");
        };
        assert!(conflicts.iter().any(|c| c.contains("swarm vanished")));
        assert_eq!(decide(&gone, &Remembered::default()), Action::Silent);
    }

    /// A cleared conflict forgets the signature, so a recurrence rings again.
    #[test]
    fn a_cleared_conflict_can_ring_again() {
        let facts = facts_of("awareness p · ok", &[], true);
        let remembered = Remembered {
            line: "awareness p · stale".into(),
            fingerprint: "awareness p · stale".into(),
            signature: "unanswered doorbell: X".into(),
            bead: Some("bd-1".into()),
            swarm: true,
        };
        assert_eq!(decide(&facts, &remembered), Action::Log(facts.line.clone()));
    }

    /// Overlap needs two holders on one path; a single holder is not a fault.
    #[test]
    fn reservation_overlap_needs_two_holders() {
        let pin = |name: &str, paths: &[&str]| crate::snapshot::PinRow {
            pin: name.into(),
            verdict: "working".into(),
            source: "test".into(),
            since: String::new(),
            bead: None,
            reservations: paths.iter().map(|p| p.to_string()).collect(),
            pane_id: String::new(),
            kind: None,
            cwd: None,
            status: String::new(),
            pending: false,
            npub: None,
            last_active_ts_us: None,
            identity_pin: None,
            observed_pin: None,
        };
        let pins = vec![
            pin("A", &["src/lib.rs"]),
            pin("B", &["src/lib.rs", "docs/"]),
        ];
        let overlap = overlaps(&pins);
        assert_eq!(
            overlap,
            vec![(
                "src/lib.rs".to_string(),
                vec!["A".to_string(), "B".to_string()]
            )]
        );
        assert!(overlaps(&pins[..1]).is_empty());
    }
}

#[cfg(test)]
mod dedupe_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The same condition keeps one key across the noise a digest line adds:
    /// counts drift, spacing drifts, and the subject is the identity.
    #[test]
    fn a_condition_key_ignores_count_and_case_noise() {
        assert_eq!(
            condition_of("unanswered doorbell: SunnyMarsh"),
            condition_of("Unanswered Doorbell:  sunnyMarsh ")
        );
        assert_eq!(
            condition_of("swarm vanished: 6 agents"),
            condition_of("swarm vanished: 0 agents")
        );
        assert_ne!(
            condition_of("unanswered doorbell: SunnyMarsh"),
            condition_of("unanswered doorbell: QuietHeron")
        );
        assert_ne!(
            condition_of("unanswered doorbell: SunnyMarsh"),
            condition_of("swarm vanished: SunnyMarsh")
        );
    }

    /// Only an OPEN bead filed for this exact condition counts as the survivor.
    #[test]
    fn the_open_bead_for_a_condition_is_matched_by_title_and_project() {
        let rows = vec![
            serde_json::json!({"id":"bd-old","title":"awareness: unanswered doorbell: SunnyMarsh in flywheel"}),
            serde_json::json!({"id":"bd-other-project","title":"awareness: unanswered doorbell: SunnyMarsh in toron"}),
            serde_json::json!({"id":"bd-other-condition","title":"awareness: swarm vanished: 0 agents in flywheel"}),
        ];
        let key = condition_of("unanswered doorbell: SunnyMarsh");
        assert_eq!(
            open_bead_for(&rows, "flywheel", &key).as_deref(),
            Some("bd-old")
        );
        assert_eq!(
            open_bead_for(&rows[1..], "flywheel", &key),
            None,
            "another project or condition is not the survivor"
        );
    }
}

#[cfg(test)]
mod dedupe_br_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const FAKE_BR: &str = r#"#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "@LOG@"
case "$1" in
  list)
    python3 - "@STATE@" <<'PY'
import json, pathlib, sys
rows = []
for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
    if not line.strip():
        continue
    title, eid = line.split("\t")
    rows.append({"id": eid, "title": title, "status": "open"})
print(json.dumps({"issues": rows}))
PY
    ;;
  create)
    printf '%s\tbd-new\n' "$2" >> "@STATE@"
    printf '{"id":"bd-new"}\n'
    ;;
  *) printf '{}\n' ;;
esac
"#;

    /// The bead's verify: emitting the same condition twice yields ONE open bead,
    /// and the second observation is a comment on it. The fake br reflects a
    /// `create` back into its own list, so the second call sees it as a real
    /// tracker would.
    #[tokio::test]
    async fn emitting_the_same_condition_twice_updates_one_open_bead() {
        let _env = crate::config::lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("beads.tsv");
        let log = dir.path().join("br.log");
        std::fs::write(&state, "").expect("state");
        let script = dir.path().join("br");
        std::fs::write(
            &script,
            FAKE_BR
                .replace("@LOG@", &log.display().to_string())
                .replace("@STATE@", &state.display().to_string()),
        )
        .expect("fake br");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", &script) };

        let first = ensure_conflict_bead(
            "flywheel",
            "unanswered doorbell: SunnyMarsh",
            "awareness flywheel: unanswered doorbell: SunnyMarsh",
        )
        .await
        .expect("first filing");
        let second = ensure_conflict_bead(
            "flywheel",
            "unanswered doorbell: SunnyMarsh",
            "awareness flywheel: unanswered doorbell: SunnyMarsh; swarm vanished",
        )
        .await
        .expect("second filing");
        unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") };

        assert_eq!(first.as_deref(), Some("bd-new"));
        assert_eq!(
            second.as_deref(),
            Some("bd-new"),
            "the same condition reuses the open bead"
        );
        let calls = std::fs::read_to_string(&log).expect("br log");
        assert_eq!(
            calls.lines().filter(|l| l.starts_with("create")).count(),
            1,
            "one bead, not one per episode: {calls}"
        );
        assert_eq!(
            calls
                .lines()
                .filter(|l| l.starts_with("comments add bd-new"))
                .count(),
            1,
            "the repeat is a comment on the survivor: {calls}"
        );
    }
}
