//! `flywheel __wave-gate` — GATE as an ENGINE step (ADR-0021 D4.5, bd-jhz8x.2).
//!
//! The stage text asked a planner seat to run preflight, scope `br ready` to a
//! wave, hold on a lower open wave, and write the binding artifact for
//! DISPATCH. That is a loop the engine can run: preflight is a `verify` step in
//! the workflow, and this command owns the gate itself, so the planner seat
//! only plans.
//!
//! Contract (the same one the stage carried):
//! - a lower open wave HOLDS: every blocker is named, the artifact records it,
//!   and the step fails;
//! - an EMPTY ready with beads of this wave IN FLIGHT is work in flight:
//!   `held: false`, `in_flight` named, the step succeeds;
//! - an EMPTY ready with nothing in flight is a LOUD failure naming the zero
//!   match — never a green wave that dispatched nothing.

use serde_json::{Value, json};

use crate::durable::{ReadyBead, bead_wave};

/// One gate decision. Pure: the rows and the label are the whole input.
#[derive(Debug, Clone)]
pub struct Gate {
    pub held: bool,
    pub blockers: Vec<String>,
    pub ready: Vec<ReadyBead>,
    pub in_flight: Vec<String>,
}

fn wave_number(label: &str) -> Result<u32, String> {
    label
        .strip_prefix("wave-")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| format!("--label must be `wave-<N>` (got `{label}`)"))
}

/// Decide one wave's gate. `Err` is the loud empty-match failure.
pub fn gate(ready: &[ReadyBead], inprogress: &[ReadyBead], label: &str) -> Result<Gate, String> {
    let n = wave_number(label)?;
    let blockers: Vec<String> = ready
        .iter()
        .chain(inprogress.iter())
        .filter(|b| bead_wave(b).is_some_and(|w| w < n))
        .map(|b| b.id.clone())
        .collect();
    let in_flight: Vec<String> = inprogress
        .iter()
        .filter(|b| bead_wave(b) == Some(n))
        .map(|b| b.id.clone())
        .collect();
    if !blockers.is_empty() {
        return Ok(Gate {
            held: true,
            blockers,
            ready: Vec::new(),
            in_flight,
        });
    }
    let rows: Vec<ReadyBead> = ready
        .iter()
        .filter(|b| bead_wave(b) == Some(n))
        .cloned()
        .collect();
    if rows.is_empty() && in_flight.is_empty() {
        return Err(format!("[gate] blocked: wave {label} matched zero beads"));
    }
    Ok(Gate {
        held: false,
        blockers: Vec::new(),
        ready: rows,
        in_flight,
    })
}

impl Gate {
    /// The binding artifact DISPATCH reads. `learnings` is the bounded GATE
    /// grounding read (ADR-0010 D12): the projected chiebukuro paths the
    /// wave carries as constraints. Absent when the corpus projects nothing.
    pub fn artifact(&self, label: &str, learnings: &[String]) -> Value {
        let mut artifact = json!({
            "wave_label": label,
            "held": self.held,
            "blockers": self.blockers,
            "in_flight": self.in_flight,
            "ready": self.ready.iter().map(|b| json!({
                "id": b.id,
                "title": b.title,
                "verify": b.verify,
            })).collect::<Vec<_>>(),
        });
        if !learnings.is_empty() {
            artifact["learnings"] = json!(learnings);
        }
        artifact
    }
}

/// The gate's grounding read: the SAME bounded projection `__gate-learnings`
/// prints, projected to the paths the artifact carries. A missing corpus is not
/// a gate failure — it is an empty read, said out loud.
pub fn learnings(root: &str, budget: usize) -> Vec<String> {
    match crate::cmd::internal::project_gate_learnings(root, budget, None, None) {
        Ok((_, projected)) => projected.iter().map(|e| e.path.clone()).collect(),
        Err(e) => {
            eprintln!("flywheel: wave gate could not read {root} for the grounding read: {e:#}");
            Vec::new()
        }
    }
}

/// `flywheel __wave-gate --label wave-N --project <p> [--json]`.
pub(crate) fn cmd(cfg: &crate::config::Config, project: &str, label: &str) -> anyhow::Result<()> {
    let ready = crate::load_ready_beads(cfg, project)?;
    let inprogress = crate::load_inprogress_beads(cfg, project)?;
    let decided = gate(&ready, &inprogress, label).map_err(anyhow::Error::msg)?;
    let projected = learnings("chiebukuro", crate::cmd::internal::default_gate_budget());
    let artifact = decided.artifact(label, &projected);
    let path = crate::project_state::wave_dir(&crate::project_dir(cfg, project))
        .join(format!("{label}-ready.json"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::project_state::write_atomic(&path, artifact.to_string().as_bytes())?;
    // stdout IS the artifact: the workflow consumes it as
    // `{{steps.gate.json.ready}}`, so a wrapper would put the data one level
    // too deep (bd-jhz8x.2).
    crate::output::print_json(&json!({
        "schema": "flywheel-wave-gate.v1",
        "project": project,
        "artifact": path.display().to_string(),
        "held": artifact["held"],
        "blockers": artifact["blockers"],
        "in_flight": artifact["in_flight"],
        "ready": artifact["ready"],
        "learnings": artifact["learnings"],
    }));
    if decided.held {
        anyhow::bail!(
            "[gate] held: lower wave still open/in_progress: {}",
            decided.blockers.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(id: &str, wave: u32) -> ReadyBead {
        ReadyBead {
            id: id.to_string(),
            title: format!("bead {id}"),
            wave: Some(wave),
            verify: Some("true".to_string()),
            ..Default::default()
        }
    }

    /// A lower open wave holds the gate and names every blocker.
    #[test]
    fn a_lower_open_wave_holds_and_names_the_blockers() {
        let ready = vec![bead("bd-a", 1), bead("bd-b", 2)];
        let g = gate(&ready, &[], "wave-2").expect("a hold is not an error");
        assert!(g.held, "a lower wave must hold");
        assert_eq!(g.blockers, vec!["bd-a".to_string()]);
        assert!(g.ready.is_empty(), "nothing is dispatchable while held");
        let artifact = g.artifact("wave-2", &[]);
        assert_eq!(artifact["held"], true);
        assert_eq!(artifact["blockers"][0], "bd-a");
    }

    /// An empty ready with beads of THIS wave in flight is work in flight, not
    /// a failure — the false-green this gate exists to prevent is the other
    /// case.
    #[test]
    fn an_empty_ready_with_this_wave_in_flight_is_work_in_flight() {
        let inprog = vec![bead("bd-x", 3)];
        let g = gate(&[], &inprog, "wave-3").expect("in flight is not a failure");
        assert!(!g.held);
        assert_eq!(g.in_flight, vec!["bd-x".to_string()]);
        assert!(g.ready.is_empty());
    }

    /// An empty ready with NOTHING in flight is a loud failure naming the zero
    /// match.
    #[test]
    fn an_empty_ready_with_nothing_in_flight_is_a_loud_failure() {
        let err = gate(&[], &[], "wave-4").expect_err("a zero match must fail");
        assert!(err.contains("matched zero beads"), "{err}");
        assert!(err.contains("wave-4"), "{err}");
    }

    /// ADR-0010 D12: the artifact carries the bounded GATE grounding read —
    /// the projected learnings paths — and omits the key when nothing projects.
    #[test]
    fn the_artifact_carries_the_projected_learnings() {
        let g = gate(&[bead("bd-a", 2)], &[], "wave-2").expect("a ready wave");
        let empty = g.artifact("wave-2", &[]);
        assert!(
            empty.get("learnings").is_none(),
            "no projection = no key, not an empty list: {empty}"
        );
        let paths = vec!["chiebukuro/inbox/x.md".to_string()];
        let artifact = g.artifact("wave-2", &paths);
        assert_eq!(artifact["learnings"][0], "chiebukuro/inbox/x.md");
    }

    /// Only this wave's rows are dispatchable.
    #[test]
    fn only_this_waves_rows_are_ready() {
        let ready = vec![bead("bd-b", 2), bead("bd-c", 2), bead("bd-d", 3)];
        let g = gate(&ready, &[], "wave-2").expect("gate");
        assert_eq!(
            g.ready.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(),
            vec!["bd-b", "bd-c"],
            "a LATER wave is not this gate's business and an earlier one would hold"
        );
    }
}
