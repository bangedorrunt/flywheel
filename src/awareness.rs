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
    let snap = crate::snapshot::assemble(cfg, project, &ws_label, unix_secs());

    let agents = snap.pins.len();
    let busy = snap.pins.iter().filter(|p| p.verdict.starts_with("Busy")).count();
    let idle = snap.pins.iter().filter(|p| p.verdict.starts_with("Idle")).count();
    let dead = snap.pins.iter().filter(|p| p.verdict.starts_with("Dead")).count();
    // A plane that publishes no verdict reads Unknown; say so instead of
    // printing three zeroes beside a non-zero agent count.
    let unknown = agents.saturating_sub(busy + idle + dead);
    let unanswered =
        crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(&state_dir, &ws_label));
    let held: usize = snap.pins.iter().map(|p| p.reservations.len()).sum();
    let swarm = agents > 0 || snap.dispatcher == "up";

    let mut conflicts = Vec::new();
    if agents > 0 && snap.dispatcher != "up" {
        conflicts.push(format!("dispatcher {} with {agents} pane(s)", snap.dispatcher));
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
    let line = format!(
        "awareness {project} {} · dispatcher: {} · doorbell: {doorbell} · \
         agents {agents} (busy {busy}, idle {idle}, dead {dead}, unknown {unknown}) · held {held}",
        crate::cron::clock_hhmm(unix_secs()),
        snap.dispatcher,
    );
    Ok(Facts {
        line,
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
            by_path.entry(path.as_str()).or_default().push(pin.pin.as_str());
        }
    }
    by_path
        .into_iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(path, mut holders)| {
            holders.sort_unstable();
            (path.to_string(), holders.into_iter().map(String::from).collect())
        })
        .collect()
}

/// Decide what this fire does. Pure: the state file is the only input beyond
/// the facts, so a test can pin every arm without a store.
pub fn decide(facts: &Facts, remembered: &Remembered) -> Action {
    let mut conflicts = facts.conflicts.clone();
    if remembered.swarm && !facts.swarm {
        // All panes gone is the loudest thing a digest can say, and it used to
        // be the quietest: "no swarm" was treated as idle.
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
        // Nothing is spawned: "dispatcher down" here is the normal idle state,
        // not a fault. Say nothing rather than wake anyone every 15 minutes.
        return Action::Silent;
    }
    if facts.line == remembered.line {
        return Action::Silent;
    }
    Action::Log(facts.line.clone())
}

/// One digest fire: compute, decide, remember, and escalate a new conflict.
pub async fn tick(cfg: &crate::config::Config, project: &str) -> Result<Option<String>> {
    // `snapshot::assemble` reads mail/reservations through `block_on`; on a
    // tokio worker that panics ("Cannot start a runtime from within a
    // runtime"). The whole gather is synchronous I/O, so it belongs on the
    // blocking pool.
    let gather_cfg = cfg.clone();
    let gather_project = project.to_string();
    let facts = tokio::task::spawn_blocking(move || facts(&gather_cfg, &gather_project))
        .await
        .map_err(|e| anyhow::anyhow!("awareness gather task: {e}"))??;
    let mut remembered = load(project);
    let action = decide(&facts, &remembered);
    match action {
        Action::Silent => Ok(None),
        Action::Log(line) => {
            remembered.line = line.clone();
            remembered.swarm = facts.swarm;
            // A cleared conflict ends the episode, so the same fault can wake
            // the captain again if it comes back.
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
            let bead = file_conflict(project, &line, &conflicts).await?;
            remembered.line = line.clone();
            remembered.swarm = facts.swarm;
            remembered.signature = signature;
            remembered.bead = bead.clone();
            save(project, &remembered)?;
            println!(
                "cron {NAME}: {line} — CONFLICT {} (bead {})",
                conflicts.join("; "),
                bead.as_deref().unwrap_or("unfiled")
            );
            Ok(Some(line))
        }
    }
}

/// File the escalation bead. The beads poller rings an idle captain on a ready
/// bead, so this is the one wake path that costs no tokens.
async fn file_conflict(
    project: &str,
    line: &str,
    conflicts: &[String],
) -> Result<Option<String>> {
    use std::process::Command;
    let title = format!("awareness: {} in {project}", conflicts.join("; "));
    let description = format!(
        "The native awareness digest saw a conflict.\n\n{line}\n\n\
         conflicts:\n- {}\n\n\
         This bead is filed once per conflict episode: while the same conflict \
         persists the digest stays quiet, and a cleared conflict resets it.\n\n\
         Diagnose before acting: `flywheel status {project}` and \
         `flywheel __doorbell --project {project} --report`.",
        conflicts.join("\n- ")
    );
    let out = Command::new("br")
        .args([
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
            "--json",
        ])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            eprintln!("cron {NAME}: br unavailable ({e}); conflict not filed");
            return Ok(None);
        }
    };
    if !out.status.success() {
        eprintln!(
            "cron {NAME}: br create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&out.stdout);
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
        eprintln!("cron {NAME}: br create answered without an id: {}", raw.trim());
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
            conflicts: conflicts.iter().map(|c| c.to_string()).collect(),
            swarm,
        }
    }

    /// A healthy swarm is logged once, then stays quiet until the line moves.
    #[test]
    fn unchanged_facts_stay_silent() {
        let facts = facts_of("awareness p · dispatcher: up", &[], true);
        assert_eq!(decide(&facts, &Remembered::default()), Action::Log(facts.line.clone()));
        let remembered = Remembered {
            line: facts.line.clone(),
            swarm: true,
            ..Default::default()
        };
        assert_eq!(decide(&facts, &remembered), Action::Silent);
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
        let facts = facts_of("awareness p · doorbell: unanswered pin=X", &["unanswered doorbell: X"], true);
        let Action::Escalate { signature, .. } = decide(&facts, &Remembered::default()) else {
            panic!("first sighting must escalate");
        };
        let remembered = Remembered {
            line: facts.line.clone(),
            signature,
            bead: Some("bd-1".into()),
            swarm: true,
        };
        assert_eq!(decide(&facts, &remembered), Action::Silent);
        // A second, different conflict is a new episode.
        let worse = facts_of(
            "awareness p · dispatcher: down",
            &["unanswered doorbell: X", "dispatcher down with 2 pane(s)"],
            true,
        );
        assert!(matches!(decide(&worse, &remembered), Action::Escalate { .. }));
    }

    /// A swarm that disappears is an escalation, not "idle host, say nothing".
    #[test]
    fn a_vanished_swarm_rings_once() {
        let gone = facts_of("awareness p · agents 0", &[], false);
        let had_swarm = Remembered {
            line: "awareness p · agents 3".into(),
            swarm: true,
            ..Default::default()
        };
        let Action::Escalate { conflicts, .. } = decide(&gone, &had_swarm) else {
            panic!("a vanished swarm must escalate");
        };
        assert!(conflicts.iter().any(|c| c.contains("swarm vanished")));
        // A host that never had a swarm stays silent on the same facts.
        assert_eq!(decide(&gone, &Remembered::default()), Action::Silent);
    }

    /// A cleared conflict forgets the signature, so a recurrence rings again.
    #[test]
    fn a_cleared_conflict_can_ring_again() {
        let facts = facts_of("awareness p · ok", &[], true);
        let remembered = Remembered {
            line: "awareness p · stale".into(),
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
        };
        let pins = vec![pin("A", &["src/lib.rs"]), pin("B", &["src/lib.rs", "docs/"])];
        let overlap = overlaps(&pins);
        assert_eq!(
            overlap,
            vec![("src/lib.rs".to_string(), vec!["A".to_string(), "B".to_string()])]
        );
        assert!(overlaps(&pins[..1]).is_empty());
    }
}
