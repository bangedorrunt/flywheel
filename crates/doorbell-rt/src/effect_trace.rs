// governed-by: ADR-0012
//! Pure converter from ordered doorbell effect-journal rows to `everystate
//! conform` observations for the `DeferredQueue` model (bd-uawqo).
//!
//! The journal is already ordered and journaled BEFORE each effect runs, so it
//! is a faithful recording. Each row becomes zero, one or two observations
//! carrying the model's action name and the values of its variables. Nothing
//! here reads a clock, socket or env; the integration test supplies the rows.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

/// The two abstract messages of `DeferredQueue.cfg`: a brief and a peer wake.
fn message_of(line: &str) -> &'static str {
    if line.starts_with("⟦p2p:") {
        "peer"
    } else {
        "brief"
    }
}

/// A running conversion. The first observation is the initial state; each
/// subsequent one names the spec action and the full state after the step.
pub struct DeferredQueueTrace {
    pub observations: Vec<Value>,
    arrived: BTreeSet<String>,
    queued: BTreeSet<String>,
    rung: BTreeSet<String>,
    audited: BTreeSet<String>,
    working: bool,
    rings: BTreeMap<String, u64>,
}

impl Default for DeferredQueueTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredQueueTrace {
    pub fn new() -> Self {
        let mut this = Self {
            observations: Vec::new(),
            arrived: BTreeSet::new(),
            queued: BTreeSet::new(),
            rung: BTreeSet::new(),
            audited: BTreeSet::new(),
            working: true, // DeferredQueue Init: the wake lands mid-turn
            rings: BTreeMap::from([("brief".into(), 0), ("peer".into(), 0)]),
        };
        this.push(None);
        this
    }

    fn state(&self) -> Value {
        let set = |s: &BTreeSet<String>| -> Value {
            Value::Array(s.iter().cloned().map(Value::String).collect())
        };
        json!({
            "arrived": set(&self.arrived),
            "queued": set(&self.queued),
            "rung": set(&self.rung),
            "audited": set(&self.audited),
            "working": self.working,
            "preempted": Value::Array(vec![]),
            "rings": self.rings,
        })
    }

    fn push(&mut self, action: Option<&str>) {
        let mut obs = self.state();
        if let Some(action) = action {
            obs["action"] = json!(action);
        }
        self.observations.push(obs);
    }

    fn ring(&mut self, m: &str) {
        *self.rings.entry(m.to_string()).or_default() += 1;
    }

    /// Convert one journal row. `rt_kind` and the serialized effects are the
    /// only inputs; a row that carries no queue decision is an `Idle` step.
    pub fn observe(&mut self, row: &Value) {
        let rt = row["msg"]["rt_kind"].as_str().unwrap_or("");
        let effects = row["actions"].as_array().cloned().unwrap_or_default();
        let prompts: Vec<&str> = effects
            .iter()
            .filter_map(|e| e.get("Prompt")?.get("line")?.as_str())
            .collect();
        let audits: Vec<(&str, &str)> = effects
            .iter()
            .filter_map(|e| {
                let a = e.get("Audit")?;
                Some((a.get("kind")?.as_str()?, a.get("detail")?.as_str()?))
            })
            .collect();
        let digests: Vec<&str> = effects
            .iter()
            .filter_map(|e| e.get("Digest")?.get("line")?.as_str())
            .collect();

        // A status change to idle is two spec steps: the turn ends
        // (`ReachPrompt`) and then the queue drains in ONE ring.
        if rt == "PlaneStatus" && row["msg"]["status"].as_str() == Some("idle") {
            if self.working {
                self.working = false;
                self.push(Some("ReachPrompt"));
            }
            if !prompts.is_empty() {
                for line in &prompts {
                    let m = message_of(line);
                    self.rung.insert(m.to_string());
                    self.queued.remove(m);
                    self.ring(m);
                }
                self.push(Some("Ring"));
            } else {
                self.push(Some("Idle"));
            }
            return;
        }

        // A wake queued while the pane is working is an arrival.
        for line in &digests {
            let m = message_of(line);
            if self.arrived.insert(m.to_string()) {
                self.queued.insert(m.to_string());
                self.push(Some("Arrive"));
            } else {
                self.push(Some("RereadByTheIdleEdge"));
            }
        }

        // The second producer's copy of an already accounted wake changes
        // nothing observable — the idle-edge re-read / dedup step.
        for (kind, detail) in &audits {
            match *kind {
                // A working pane queues the wake: that is an Arrive in the
                // model (the newer class may supersede, still one arrival).
                "queued" | "merged" | "superseded" => {
                    let m = message_of(detail);
                    if self.arrived.insert(m.to_string()) {
                        self.queued.insert(m.to_string());
                        self.push(Some("Arrive"));
                    } else {
                        self.push(Some("RereadByTheIdleEdge"));
                    }
                }
                "already_rung" | "dedup" | "deferred" => {
                    self.push(Some("RereadByTheIdleEdge"));
                }
                "system-notice" => {
                    let m = message_of(detail);
                    if self.arrived.insert(m.to_string()) {
                        self.audited.insert(m.to_string());
                        self.push(Some("Audit"));
                    } else {
                        self.push(Some("Idle"));
                    }
                }
                _ => self.push(Some("Idle")),
            }
        }

        if digests.is_empty() && audits.is_empty() {
            // Died/RestartWatch, Resume, PlaneStatus working, ...: no queue
            // decision, so the spec is allowed to stutter.
            self.push(Some("Idle"));
        }
    }

    /// All observations as JSON Lines, one per line.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for obs in &self.observations {
            out.push_str(&obs.to_string());
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_observation_is_the_models_init_state() {
        let trace = DeferredQueueTrace::new();
        let first = &trace.observations[0];
        assert!(
            first.get("action").is_none(),
            "the first row is the initial state"
        );
        assert_eq!(first["working"], true);
        assert_eq!(first["queued"], json!([]));
        assert_eq!(first["rings"]["brief"], 0);
    }

    #[test]
    fn a_deferral_then_idle_drain_rings_once() {
        let mut trace = DeferredQueueTrace::new();
        trace.observe(&json!({
            "msg":{"rt_kind":"Watch"},
            "actions":[{"Digest":{"pin":"P","line":"[doorbell] kind 9. Pull: toron next","why":"working"}}],
            "executed":{}
        }));
        trace.observe(&json!({
            "msg":{"rt_kind":"PlaneStatus","status":"idle"},
            "actions":[{"Prompt":{"pin":"P","line":"[doorbell] kind 9. Pull: toron next"}}],
            "executed":{}
        }));
        let actions: Vec<&str> = trace
            .observations
            .iter()
            .skip(1)
            .filter_map(|o| o["action"].as_str())
            .collect();
        assert_eq!(
            actions,
            vec!["Arrive", "ReachPrompt", "Ring"],
            "{actions:?}"
        );
        let last = trace.observations.last().unwrap();
        assert_eq!(last["rings"]["brief"], 1);
        assert_eq!(last["queued"], json!([]));
        assert_eq!(last["rung"], json!(["brief"]));
    }

    #[test]
    fn a_second_producers_copy_is_a_no_op_step() {
        let mut trace = DeferredQueueTrace::new();
        trace.observe(&json!({
            "msg":{"rt_kind":"Watch"},
            "actions":[{"Digest":{"pin":"P","line":"[doorbell] kind 9. Pull: toron next","why":"working"}}],
            "executed":{}
        }));
        trace.observe(&json!({
            "msg":{"rt_kind":"Watch"},
            "actions":[{"Audit":{"kind":"already_rung","pin":"P","detail":"[doorbell] kind 9"}}],
            "executed":{}
        }));
        let last = trace.observations.last().unwrap();
        assert_eq!(last["action"], "RereadByTheIdleEdge");
        assert_eq!(last["rings"]["brief"], 0, "a repeated wake never rings");
    }
}
