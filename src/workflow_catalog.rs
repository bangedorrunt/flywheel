//! Unified workflow catalog (ADR-0005 Spec 2 / P3.2).
//!
//! One vocabulary, one engine. This module is the flywheel-side catalog that
//! feeds `WorkflowEngine::invoke` (the P3.1 cargo edge):
//!
//! - **Canonical home:** `~/.config/flywheel/workflows/*.yaml` — unified
//!   toron-workflow schema (parse via `toron_workflow::parse_workflow`).
//! - **Legacy absorbed, not deprecated:** `workflows.toml`
//!   (`WorkflowStep::Send`/`Wait`) + `pipelines.toml` (a chain of workflow
//!   names) are read through the SAME catalog surface. `Send{to,stage}` →
//!   a `dispatch_job` step; `Wait{until}` → a wait step; a pipeline → a
//!   workflow of `call_workflow` steps.
//!
//! **Honesty rule (P3.2):** if a legacy `WorkflowStep` variant does not map
//! 1:1 to a unified step, [`absorb`] records a `MappingNote` — it is NEVER
//! silently dropped. The current mapping:
//!
//! | legacy | unified step | 1:1? |
//! |---|---|---|
//! | `Send{to, stage, msg}` | `dispatch_job` (assignee=`to`, spec carries the resolved stage prompt / raw msg) | YES (herdr-direct semantics preserved via the engine's injected runner) |
//! | `Wait{until}` | `delay` (no-op shell; the engine's `spawn{await}` binding is P4) | NO — flagged, run continues |
//! | pipeline (chain) | workflow of `call_workflow` steps | NO — `call_workflow` is a P5 control-flow action; flagged |
//!
//! `WorkflowEngine::invoke` is the single execution door: flywheel's legacy
//! `run_workflow`/`run_pipeline` become thin callers of it (main.rs wiring is
//! coordinated with the P1 lane — see AGENT notes).
//!
//! governed-by: ADR-0005 Spec 2, P3.2

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use toron_workflow::schema::{Action, InputDecl, Step, Workflow, parse_workflow};

use crate::config::config_dir;

/// The legacy `workflows.toml` step shapes (mirrored from flywheel's
/// `config::WorkflowStep` — the catalog parses the legacy file itself so the
/// absorber is self-contained, no dependency on the live config struct).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacyStep {
    Send {
        to: String,
        #[serde(default)]
        stage: Option<String>,
        #[serde(default)]
        msg: Option<String>,
    },
    Wait {
        until: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

/// Legacy `workflows.toml`: name → ordered steps.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyWorkflows {
    #[serde(flatten)]
    workflows: BTreeMap<String, Vec<LegacyStep>>,
}

/// Legacy `pipelines.toml`: name → ordered workflow names.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyPipelines {
    #[serde(flatten)]
    pipelines: BTreeMap<String, Vec<String>>,
}

/// A mapping caveat for a legacy step that does not map 1:1 to a unified
/// step. The catalog NEVER silently drops legacy behavior — every such step
/// produces a note surfaced to the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingNote {
    /// Legacy source: `workflows.toml:<workflow>[:<index>]` or
    /// `pipelines.toml:<pipeline>[:<index>]`.
    pub source: String,
    /// What the legacy step was.
    pub legacy: String,
    /// Why the mapping is not 1:1.
    pub reason: String,
}

/// The unified catalog view: canonical workflows + absorbed legacy, plus any
/// mapping notes.
#[derive(Debug, Clone, Default)]
pub struct WorkflowCatalog {
    /// Unified workflows (canonical YAML + absorbed legacy), name → workflow.
    pub workflows: BTreeMap<String, Workflow>,
    /// Non-1:1 mapping caveats (never silently dropped).
    pub notes: Vec<MappingNote>,
}

impl WorkflowCatalog {
    /// Load the full catalog from the default config dir (canonical
    /// `workflows/*.yaml` + absorbed legacy `workflows.toml` /
    /// `pipelines.toml`).
    pub fn load() -> Result<Self> {
        Self::load_from(config_dir())
    }

    /// Load the full catalog from an explicit config root (the config dir
    /// containing `workflows/`, `workflows.toml`, `pipelines.toml`). Tests
    /// pass a hermetic temp dir — no process-global env mutation.
    pub fn load_from(config_root: PathBuf) -> Result<Self> {
        let mut catalog = Self {
            workflows: BTreeMap::new(),
            notes: Vec::new(),
        };
        catalog.load_canonical(&config_root)?;
        catalog.absorb(&config_root)?;
        Ok(catalog)
    }

    /// Load canonical `workflows/*.yaml` (unified schema).
    fn load_canonical(&mut self, config_root: &PathBuf) -> Result<()> {
        let dir = config_root.join("workflows");
        if !dir.is_dir() {
            return Ok(());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        entries.sort();
        for path in entries {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let wf = parse_workflow(&raw)
                .with_context(|| format!("parse workflow {}", path.display()))?;
            self.workflows.insert(wf.name.clone(), wf);
        }
        Ok(())
    }

    /// Absorb legacy `workflows.toml` (Send/Wait) + `pipelines.toml` (chains)
    /// through the SAME catalog surface. Legacy name collisions with canonical
    /// YAML are overwritten by the canonical file (canonical is the home);
    /// a mapping note records the shadowed legacy entry.
    fn absorb(&mut self, config_root: &PathBuf) -> Result<()> {
        // Legacy workflows.toml
        let wf_path = config_root.join("workflows.toml");
        if wf_path.exists() {
            let raw = std::fs::read_to_string(&wf_path)
                .with_context(|| format!("read {}", wf_path.display()))?;
            let legacy: LegacyWorkflows = toml::from_str(&raw)
                .with_context(|| format!("parse {}", wf_path.display()))?;
            for (name, steps) in legacy.workflows {
                if self.workflows.contains_key(&name) {
                    self.notes.push(MappingNote {
                        source: "workflows.toml".into(),
                        legacy: name.clone(),
                        reason: "shadowed by canonical workflows/*.yaml entry".into(),
                    });
                    continue;
                }
                let (wf, notes) = absorb_workflow_steps(&name, &steps);
                self.workflows.insert(name, wf);
                self.notes.extend(notes);
            }
        }

        // Legacy pipelines.toml
        let pipe_path = config_root.join("pipelines.toml");
        if pipe_path.exists() {
            let raw = std::fs::read_to_string(&pipe_path)
                .with_context(|| format!("read {}", pipe_path.display()))?;
            let legacy: LegacyPipelines = toml::from_str(&raw)
                .with_context(|| format!("parse {}", pipe_path.display()))?;
            for (name, chain) in legacy.pipelines {
                if self.workflows.contains_key(&name) {
                    self.notes.push(MappingNote {
                        source: "pipelines.toml".into(),
                        legacy: name.clone(),
                        reason: "shadowed by canonical workflows/*.yaml entry".into(),
                    });
                    continue;
                }
                let (wf, note) = absorb_pipeline(&name, &chain);
                self.workflows.insert(name, wf);
                if let Some(note) = note {
                    self.notes.push(note);
                }
            }
        }
        Ok(())
    }
}

/// Map a legacy `WorkflowStep` list into a unified `Workflow`. Steps that map
/// 1:1 become unified steps; non-1:1 steps become flagged shell steps + a
/// [`MappingNote`].
fn absorb_workflow_steps(name: &str, steps: &[LegacyStep]) -> (Workflow, Vec<MappingNote>) {
    let mut notes = Vec::new();
    let mut unified: Vec<Step> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let source = format!("workflows.toml:{name}:{i}");
        match step {
            LegacyStep::Send { to, stage, msg } => {
                // Send{to,stage,msg} -> dispatch_job. The stage prompt / raw
                // message is carried in the spec so the injected ActionRunner
                // (the herdr-direct surface) preserves semantics.
                let spec = serde_json::json!({
                    "to": to,
                    "stage": stage,
                    "msg": msg,
                });
                unified.push(Step {
                    name: Some(format!("send-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    action: Action::DispatchJob {
                        assignee: to.clone(),
                        spec,
                        channel: None,
                    },
                });
            }
            LegacyStep::Wait { until, timeout_ms } => {
                // Wait{until} -> the engine's spawn-await binding is a P4
                // subsystem. Until then a delay shell keeps the run advancing;
                // flagged, never dropped.
                notes.push(MappingNote {
                    source: source.clone(),
                    legacy: format!("wait {until}"),
                    reason: "Wait maps to the P4 spawn{await} binding; delay shell used until then".into(),
                });
                unified.push(Step {
                    name: Some(format!("wait-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    action: Action::Delay {
                        secs: timeout_ms.map(|ms| ms / 1000).unwrap_or(0),
                    },
                });
            }
        }
    }
    let wf = Workflow {
        name: name.to_string(),
        description: Some(format!("absorbed legacy workflows.toml workflow")),
        trigger: None,
        guard: None,
        steps: unified,
        graph: None,
        crew: None,
        policy: None,
        inputs: vec![InputDecl { name: "goal".into() }],
    };
    (wf, notes)
}

/// Map a legacy pipeline (chain of workflow names) into a workflow whose
/// steps are `call_workflow` (a P5 control-flow action). Until P5 lands, the
/// steps are flagged.
fn absorb_pipeline(name: &str, chain: &[String]) -> (Workflow, Option<MappingNote>) {
    let mut steps = Vec::new();
    for (i, wf_name) in chain.iter().enumerate() {
        steps.push(Step {
            name: Some(format!("call-{i}")),
            guard: None,
            on_error: Default::default(),
            // call_workflow is a P5 action; the engine currently rejects it.
            // The flag below is the honesty contract — the chain is listed but
            // not runnable until P5 lands.
            action: Action::DispatchJob {
                assignee: wf_name.clone(),
                spec: serde_json::json!({ "call_workflow": true }),
                channel: None,
            },
        });
    }
    let wf = Workflow {
        name: name.to_string(),
        description: Some("absorbed legacy pipelines.toml chain".into()),
        trigger: None,
        guard: None,
        steps,
        graph: None,
        crew: None,
        policy: None,
        inputs: vec![InputDecl { name: "goal".into() }],
    };
    let note = MappingNote {
        source: format!("pipelines.toml:{name}"),
        legacy: "pipeline chain".into(),
        reason: "pipeline maps to call_workflow steps (P5 control-flow); listed but not runnable until P5".into(),
    };
    (wf, Some(note))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A hermetic config root (no process-global env mutation).
    fn config_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fw-cat-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &std::path::Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Canonical `workflows/*.yaml` loads into the catalog.
    #[test]
    fn canonical_yaml_loads() {
        let dir = config_root("canonical");
        write(
            &dir.join("workflows/triage.yaml"),
            "name: triage\ntrigger:\n  type: message_posted\nsteps:\n  - name: ack\n    action: send_dm\n    to: pi\n    body: received\n",
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        assert!(catalog.workflows.contains_key("triage"));
        assert_eq!(catalog.workflows["triage"].name, "triage");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Legacy `workflows.toml` Send/Wait absorbs into unified steps.
    #[test]
    fn legacy_workflows_toml_absorbs() {
        let dir = config_root("legacy-wf");
        write(
            &dir.join("workflows.toml"),
            "[[daily]]\ntype = \"send\"\nto = \"oc\"\nstage = \"p1\"\n[[daily]]\ntype = \"wait\"\nuntil = \"done\"\n",
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        let wf = catalog.workflows.get("daily").expect("absorbed");
        assert_eq!(wf.steps.len(), 2, "Send + Wait absorbed as 2 steps");
        assert!(matches!(wf.steps[0].action, Action::DispatchJob { .. }));
        assert!(matches!(wf.steps[1].action, Action::Delay { .. }));
        assert!(
            catalog.notes.iter().any(|n| n.source.contains("daily") && n.legacy.contains("wait")),
            "Wait non-1:1 mapping is FLAGGED, never dropped: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Legacy `pipelines.toml` chain absorbs + is flagged (call_workflow is P5).
    #[test]
    fn legacy_pipeline_absorbs_with_flag() {
        let dir = config_root("legacy-pipe");
        write(
            &dir.join("workflows.toml"),
            "[[a]]\ntype = \"send\"\nto = \"oc\"\nmsg = \"hello\"\n",
        );
        write(
            &dir.join("pipelines.toml"),
            "chain = [\"a\"]\n",
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        let wf = catalog.workflows.get("chain").expect("pipeline absorbed");
        assert_eq!(wf.steps.len(), 1);
        assert!(
            catalog.notes.iter().any(|n| n.source.starts_with("pipelines.toml:chain")),
            "pipeline call_workflow flag present: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
