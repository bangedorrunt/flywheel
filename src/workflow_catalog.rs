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
use std::path::{Path, PathBuf};

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

    /// [`load`] plus the PROJECT tier (ADR-0022 §3.8 / W6):
    /// `<project>/.flywheel/workflows/*.yaml` shadows the user catalog,
    /// which shadows built-ins. `project_root` is the project directory
    /// (pass None where no project context exists).
    pub fn load_layered(project_root: Option<&Path>) -> Result<Self> {
        Self::load_layered_with(config_dir(), project_root)
    }

    /// [`load_layered`] against an explicit config root — the hermetic test
    /// surface (no process-global env mutation).
    pub fn load_layered_with(config_root: PathBuf, project_root: Option<&Path>) -> Result<Self> {
        let mut catalog = Self::load_from(config_root)?;
        if let Some(root) = project_root {
            let dir = root.join(".flywheel").join("workflows");
            if dir.is_dir() {
                // Reuse the canonical loader shape against the project dir:
                // parse each YAML and shadow same-named user entries.
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
                    catalog.workflows.insert(wf.name.clone(), wf);
                }
            }
        }
        Ok(catalog)
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
    fn load_canonical(&mut self, config_root: &Path) -> Result<()> {
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
    fn absorb(&mut self, config_root: &Path) -> Result<()> {
        // Legacy workflows.toml
        let wf_path = config_root.join("workflows.toml");
        if wf_path.exists() {
            let raw = std::fs::read_to_string(&wf_path)
                .with_context(|| format!("read {}", wf_path.display()))?;
            let legacy: LegacyWorkflows =
                toml::from_str(&raw).with_context(|| format!("parse {}", wf_path.display()))?;
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
            let legacy: LegacyPipelines =
                toml::from_str(&raw).with_context(|| format!("parse {}", pipe_path.display()))?;
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
                // (the herdr-direct surface) preserves semantics. `stage` is
                // a contract field; the legacy `to`/`msg` pair rides in
                // `extra` (bd-dispatch-spec-untyped-0iud).
                let spec = toron_workflow::schema::DispatchSpec {
                    stage: stage.clone(),
                    extra: [
                        ("to".to_string(), serde_json::Value::String(to.clone())),
                        (
                            "msg".to_string(),
                            msg.clone()
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                };
                unified.push(Step {
                    name: Some(format!("send-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    error_kinds: Vec::new(),
                    build_slot: false,
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
                    reason: "Wait maps to the P4 spawn{await} binding; delay shell used until then"
                        .into(),
                });
                unified.push(Step {
                    name: Some(format!("wait-{i}")),
                    guard: None,
                    on_error: Default::default(),
                    error_kinds: Vec::new(),
                    build_slot: false,
                    action: Action::Delay {
                        // Round UP (is1.5): integer division truncates a
                        // sub-second wait to 0s — a silent no-op. 500ms → 1s
                        // keeps the effective wait; only a 0/absent timeout
                        // maps to 0.
                        secs: timeout_ms.map(|ms| ms.div_ceil(1000)).unwrap_or(0),
                    },
                });
            }
        }
    }
    let wf = Workflow {
        name: name.to_string(),
        description: Some("absorbed legacy workflows.toml workflow".to_string()),
        trigger: None,
        guard: None,
        steps: unified,
        graph: None,
        crew: None,
        policy: None,
        inputs: vec![InputDecl {
            name: "goal".into(),
            default: None,
        }],
        principles: vec![],
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
            error_kinds: Vec::new(),
            build_slot: false,
            // call_workflow is a P5 action; the engine currently rejects it.
            // The flag below is the honesty contract — the chain is listed but
            // not runnable until P5 lands.
            action: Action::DispatchJob {
                assignee: wf_name.clone(),
                spec: toron_workflow::schema::DispatchSpec {
                    extra: [("call_workflow".to_string(), serde_json::Value::Bool(true))]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                },
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
        inputs: vec![InputDecl {
            name: "goal".into(),
            default: None,
        }],
        principles: vec![],
    };
    let note = MappingNote {
        source: format!("pipelines.toml:{name}"),
        legacy: "pipeline chain".into(),
        reason: "pipeline maps to call_workflow steps (P5 control-flow); listed but not runnable until P5".into(),
    };
    (wf, Some(note))
}

/// Write one workflow as canonical catalog YAML — the W6 shared writer
/// (§3.7 T3 / §3.8): `workflow new` and compose `--keep` produce IDENTICAL
/// file shapes, so both paths lint identically. Overwrites a same-name entry.
pub fn write_workflow_yaml(dir: &Path, wf: &Workflow) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.yaml", wf.name));
    let raw = serde_yaml::to_string(wf).context("serialize workflow yaml")?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
/// Parse the provenance tokens a kept workflow carries in its description
/// (`origin: composed; from: <parts>; first-run: <ts>; author: <pin>`).
/// Returns (origin, from, first_run, author) — None where absent. The
/// keep-without-green-run refusal + provenance tests ride on these tokens.
pub fn parse_provenance(description: &str) -> Option<(String, String, String, String)> {
    let start = description.find("[origin:")?;
    let end = description[start..].find(']')? + start;
    let body = &description[start + 1..end];
    let mut origin = None;
    let mut from = None;
    let mut first_run = None;
    let mut author = None;
    for part in body.split(';') {
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim() {
            "origin" => origin = Some(v),
            "from" => from = Some(v),
            "first-run" => first_run = Some(v),
            "author" => author = Some(v),
            _ => {}
        }
    }
    Some((
        origin?,
        from.unwrap_or_default(),
        first_run.unwrap_or_default(),
        author.unwrap_or_default(),
    ))
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

    /// is1.5: absorbed `Wait` timeouts round UP — a sub-second wait must not
    /// truncate to a 0s delay (silent no-op).
    #[test]
    fn absorbed_wait_timeout_rounds_up_not_truncates() {
        let dir = config_root("wait-round");
        write(
            &dir.join("workflows.toml"),
            r#"[[sub-second]]
type = "wait"
until = "ready"
timeout_ms = 500
"#,
        );
        // absorb directly (self-contained): the step shapes parse from a
        // legacy file written above.
        let raw = std::fs::read_to_string(dir.join("workflows.toml")).unwrap();
        let legacy: LegacyWorkflows = toml::from_str(&raw).unwrap();
        let steps = legacy.workflows.get("sub-second").unwrap();
        let (wf, _notes) = absorb_workflow_steps("sub-second", steps);
        match &wf.steps[0].action {
            Action::Delay { secs } => {
                assert_eq!(*secs, 1, "500ms rounds UP to 1s, never 0");
            }
            other => panic!("wait absorbed to Delay, got {other:?}"),
        }
        // exact-second and absent-timeout boundaries.
        let steps2 = vec![LegacyStep::Wait {
            until: "x".into(),
            timeout_ms: Some(1500),
        }];
        let (wf2, _) = absorb_workflow_steps("w2", &steps2);
        match &wf2.steps[0].action {
            Action::Delay { secs } => assert_eq!(*secs, 2, "1500ms -> 2s"),
            other => panic!("{other:?}"),
        }
        let steps3 = vec![LegacyStep::Wait {
            until: "x".into(),
            timeout_ms: None,
        }];
        let (wf3, _) = absorb_workflow_steps("w3", &steps3);
        match &wf3.steps[0].action {
            Action::Delay { secs } => assert_eq!(*secs, 0, "absent timeout -> 0 (no wait)"),
            other => panic!("{other:?}"),
        }
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
            catalog
                .notes
                .iter()
                .any(|n| n.source.contains("daily") && n.legacy.contains("wait")),
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
        write(&dir.join("pipelines.toml"), "chain = [\"a\"]\n");
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        let wf = catalog.workflows.get("chain").expect("pipeline absorbed");
        assert_eq!(wf.steps.len(), 1);
        assert!(
            catalog
                .notes
                .iter()
                .any(|n| n.source.starts_with("pipelines.toml:chain")),
            "pipeline call_workflow flag present: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 193.3: the legacy Send + Wait branches map to unified steps with the
    /// documented semantics — Send → dispatch_job (assignee + spec carries
    /// to/stage/msg), Wait → delay shell + a MappingNote (flagged, never
    /// dropped).
    #[test]
    fn absorb_send_and_wait_steps_map_to_unified() {
        let dir = config_root("sendwait");
        write(
            &dir.join("workflows.toml"),
            r#"[[daily]]
type = "send"
to = "oc"
stage = "p1"
msg = "hello"

[[daily]]
type = "wait"
until = "done"
timeout_ms = 500
"#,
        );
        let raw = std::fs::read_to_string(dir.join("workflows.toml")).unwrap();
        let legacy: LegacyWorkflows = toml::from_str(&raw).unwrap();
        let (wf, notes) = absorb_workflow_steps("daily", legacy.workflows.get("daily").unwrap());
        assert_eq!(wf.steps.len(), 2, "both legacy steps absorbed");
        match &wf.steps[0].action {
            Action::DispatchJob { assignee, spec, .. } => {
                assert_eq!(assignee, "oc");
                assert_eq!(
                    spec.extra.get("to").and_then(serde_json::Value::as_str),
                    Some("oc")
                );
                assert_eq!(spec.stage.as_deref(), Some("p1"));
                assert_eq!(
                    spec.extra.get("msg").and_then(serde_json::Value::as_str),
                    Some("hello")
                );
            }
            other => panic!("Send absorbed to DispatchJob, got {other:?}"),
        }
        match &wf.steps[1].action {
            Action::Delay { secs } => assert_eq!(*secs, 1, "500ms rounds up"),
            other => panic!("Wait absorbed to Delay, got {other:?}"),
        }
        assert!(
            notes.iter().any(|n| n.legacy.contains("wait done")),
            "Wait is flagged, never dropped: {notes:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 193.3: a legacy name colliding with a canonical YAML entry is
    /// SHADOWED — canonical is the home; a mapping note records the shadow.
    #[test]
    fn canonical_shadows_legacy_collision() {
        let dir = config_root("collide");
        write(
            &dir.join("workflows/triage.yaml"),
            "name: triage\ntrigger:\n  type: webhook\nsteps:\n  - name: a\n    action: send_dm\n    to: pi\n    body: x\n",
        );
        write(
            &dir.join("workflows.toml"),
            r#"[[triage]]
type = "send"
to = "oc"
msg = "legacy triage"
"#,
        );
        let catalog = WorkflowCatalog::load_from(dir.clone()).unwrap();
        assert!(catalog.workflows.contains_key("triage"));
        assert_eq!(
            catalog.workflows["triage"].steps[0].name.as_deref(),
            Some("a"),
            "canonical YAML wins the collision"
        );
        assert!(
            catalog
                .notes
                .iter()
                .any(|n| n.reason.contains("shadowed by canonical")),
            "collision recorded: {:?}",
            catalog.notes
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- ADR-0022 W6 (bd-adr22-w6-stage-registry-0jgr) ----

    /// W6 test helper: a minimal lint-clean workflow (send_dm steps).
    fn w6_wf(name: &str, description: &str) -> Workflow {
        Workflow {
            name: name.to_string(),
            description: Some(description.to_string()),
            trigger: None,
            guard: None,
            crew: None,
            policy: None,
            graph: None,
            principles: vec![],
            inputs: vec![InputDecl {
                name: "goal".into(),
                default: None,
            }],
            steps: vec![Step {
                name: Some("leaf".into()),
                guard: None,
                on_error: Default::default(),
                error_kinds: Vec::new(),
                build_slot: false,
                action: Action::DispatchJob {
                    assignee: "x".into(),
                    spec: toron_workflow::schema::DispatchSpec {
                        profile: Some("@coder".into()),
                        task: Some("{{inputs.goal}}".into()),
                        ..Default::default()
                    },
                    channel: None,
                },
            }],
        }
    }

    /// W6 §3.7 T3: keep-after-green writes the project catalog entry the
    /// resolver finds — load_layered_with sees it, shadowing user.
    #[test]
    fn w6_keep_after_green_resolves_in_project_tier() {
        let dir = config_root("w6keep");
        // User tier already has a same-name entry (older shape)…
        write(
            &dir.join("workflows/w6-kept.yaml"),
            "name: w6-kept\ntrigger:\n  type: webhook\nsteps:\n  - name: old\n    action: send_dm\n    to: pi\n    body: x\n",
        );
        let wf = w6_wf(
            "w6-kept",
            "composed run [origin: composed; from: scout:p1; first-run: t; author: tealdune]",
        );
        let proj = dir.join("proj");
        let path = write_workflow_yaml(&proj.join(".flywheel/workflows"), &wf).unwrap();
        assert!(path.is_file(), "kept file written: {}", path.display());
        let cat = WorkflowCatalog::load_layered_with(dir.clone(), Some(&proj)).unwrap();
        let entry = cat.workflows.get("w6-kept").expect("resolver finds kept");
        assert!(
            entry.steps[0].action.type_name() == "dispatch_job",
            "project tier shadows user"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// W6: provenance tokens survive the YAML round-trip and parse back.
    #[test]
    fn w6_provenance_fields_present_after_roundtrip() {
        let dir = config_root("w6prov");
        let desc = "composed run for obj [origin: composed; from: fresh-eyes-explore,systematic-execute; first-run: 2026-08-21T12:00:00Z; author: TealDune]";
        let proj = dir.join("proj");
        write_workflow_yaml(&proj.join(".flywheel/workflows"), &w6_wf("w6-prov", desc)).unwrap();
        let cat = WorkflowCatalog::load_layered_with(dir.clone(), Some(&proj)).unwrap();
        let got = parse_provenance(cat.workflows["w6-prov"].description.as_deref().unwrap());
        let (origin, from, first_run, author) = got.expect("provenance parses");
        assert_eq!(origin, "composed");
        assert_eq!(from, "fresh-eyes-explore,systematic-execute");
        assert_eq!(first_run, "2026-08-21T12:00:00Z");
        assert_eq!(author, "TealDune");
        let _ = fs::remove_dir_all(&dir);
    }
}
