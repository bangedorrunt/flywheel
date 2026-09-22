//! ADR-0022 §3.3/§3.5 chain mechanics (bd-cvv9): the gated `ship` chain
//! runner and the warn-first plan-artifact gate.
//!
//! The `ship` built-in is DATA (five `call_workflow` links, builtin_workflows).
//! `run_ship_chain` invokes each link through the SAME durable engine
//! (`engine.invoke` — one rail) and applies the §4.5 completion contract
//! BETWEEN links: warn-first by default (a failed contract logs a gate
//! event and the chain continues), blocking after the W5 flip
//! (`FLYWHEEL_SHIP_GATE_MODE=block`).
//!
//! governed-by: ADR-0022 §3.3, §3.4, §3.5
use anyhow::Result;
use serde_json::{Value, json};
use toron_workflow::WorkflowEngine;
use toron_workflow::schema::{Action, Step, Workflow};

use crate::phase_contracts::check_phase;

/// Gate disposition between ship links (§3.5): warn-first until the
/// enforcement flip; `block` refuses to continue past a failed contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Warn,
    Block,
}

/// `FLYWHEEL_SHIP_GATE_MODE=block` flips the chain gates to blocking (the
/// W5 flip is this default changing). Unknown values fall back to warn —
/// the safe side, never silently blocking.
pub fn gate_mode() -> GateMode {
    match std::env::var("FLYWHEEL_SHIP_GATE_MODE").as_deref() {
        Ok("block") => GateMode::Block,
        _ => GateMode::Warn,
    }
}

/// Render the tiny template vocabulary the ship built-ins use:
/// `{{inputs.KEY}}` and `{{steps.NAME}}` (whole-value refs, embedded as
/// JSON). Anything else passes through literal — the engine's full renderer
/// owns arbitrary templates; the chain runner only needs link inputs.
pub fn render_link_inputs(
    template: &Value,
    inputs: &Value,
    steps: &serde_json::Map<String, Value>,
) -> Value {
    fn walk(v: &Value, inputs: &Value, steps: &serde_json::Map<String, Value>) -> Value {
        match v {
            Value::String(s) => {
                let t = s.trim();
                if let Some(key) = t
                    .strip_prefix("{{inputs.")
                    .and_then(|r| r.strip_suffix("}}"))
                {
                    return inputs.get(key).cloned().unwrap_or(Value::Null);
                }
                if let Some(name) = t
                    .strip_prefix("{{steps.")
                    .and_then(|r| r.strip_suffix("}}"))
                {
                    return steps.get(name).cloned().unwrap_or(Value::Null);
                }
                v.clone()
            }
            Value::Array(a) => Value::Array(a.iter().map(|i| walk(i, inputs, steps)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, val)| (k.clone(), walk(val, inputs, steps)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    walk(template, inputs, steps)
}

/// §3.5 warn-first plan-artifact gate, pure half: does an `implement`
/// dispatch carry (or sit downstream of) a completed plan artifact? A
/// `spec.plan` reference (the goal-direct pattern) or a plan/beads artifact
/// in the run inputs satisfies it; anything else is warn-worthy — never a
/// block, until the W5 flip.
pub fn plan_artifact_missing(spec: &toron_workflow::schema::DispatchSpec, inputs: &Value) -> bool {
    if spec.phase.as_deref() != Some("implement") {
        return false; // only implement is gated on a plan artifact
    }
    let spec_carries_plan = spec
        .plan
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && p.trim() != "{{steps.plan}}");
    let inputs_carry_plan = ["plan", "beads"]
        .iter()
        .any(|k| inputs.get(k).is_some_and(|v| !v.is_null()));
    let extra_carries_plan = spec.extra.get("plan").is_some_and(|v| !v.is_null());
    !(spec_carries_plan || inputs_carry_plan || extra_carries_plan)
}

/// Wire point for the warn gate: annotate a dispatch step output with
/// `plan_artifact_missing: true` (+ the stderr warning is the caller's).
/// Pure; unit-pinned. `phase_degraded` rides separately (W2 registry).
pub fn annotate_dispatch_output(
    spec: &toron_workflow::schema::DispatchSpec,
    inputs: &Value,
    out: Value,
) -> Value {
    if plan_artifact_missing(spec, inputs) {
        let mut map = out;
        if let Some(obj) = map.as_object_mut() {
            obj.insert("plan_artifact_missing".into(), json!(true));
        }
        map
    } else {
        out
    }
}

/// The ship link step: (name, workflow, rendered inputs) in chain order.
fn ship_links(ship: &Workflow) -> Vec<(String, String)> {
    ship.steps
        .iter()
        .filter_map(|s| {
            if let Action::CallWorkflow { workflow, .. } = &s.action {
                Some((
                    s.name.clone().unwrap_or_else(|| workflow.clone()),
                    workflow.clone(),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// The per-link dispatch step whose output the contract checks: the FIRST
/// dispatch_job step of the linked workflow (the phase built-ins are
/// single-dispatch chains; review's is nested inside its parallel fanout).
fn phase_dispatch_output(link_out: &Value) -> Value {
    // invoke output maps step names to outputs; the phase built-ins name
    // their dispatch step `phase-<p>` (namespace: the ADR-0005 `plan`
    // builtin predates and keeps its name). The review fanout collects a
    // list under its step key — take the last reviewer's report.
    for phase in ["resolve", "plan", "implement", "review", "verify"] {
        let key = format!("phase-{phase}");
        if let Some(v) = link_out.get(key.as_str()).or_else(|| link_out.get(phase)) {
            if let Some(list) = v.as_array()
                && let Some((last, _)) = list.split_last()
            {
                return last.clone();
            }
            return v.clone();
        }
    }
    Value::Null
}

/// Inject `dep_cycles` into the plan report by running `br dep cycles
/// --json` in the project dir (the runner verifies acyclicity; the agent
/// does not self-certify). br unavailable / no tracker → the field stays
/// absent and the contract fails loudly on "must inject".
fn inject_dep_cycles(report: &mut Value, project_dir: &std::path::Path) {
    let out = std::process::Command::new("br")
        .arg("dep")
        .arg("cycles")
        .arg("--json")
        .current_dir(project_dir)
        .output();
    if let Ok(o) = out
        && let Ok(v) = serde_json::from_slice::<Value>(&o.stdout)
    {
        let count = v.get("count").and_then(Value::as_u64).unwrap_or(u64::MAX);
        if let Some(obj) = report.as_object_mut() {
            obj.insert("dep_cycles".into(), json!(count));
        }
    }
}

/// Merge the runner-verified `dep_cycles` into the plan link's REPORT
/// (under `response` when the leaf wrapped it) so `check_phase("plan", ..)`
/// sees it. Best-effort shape handling; a miss fails the contract honestly.
fn merge_dep_cycles_into_step(step_out: &mut Value, project_dir: &std::path::Path) {
    // direct object report
    if step_out.get("dep_cycles").is_none() && step_out.get("beads").is_some() {
        inject_dep_cycles(step_out, project_dir);
        return;
    }
    // headless leaf shape: {"response": "<report json>"}
    if let Some(r) = step_out.get("response").and_then(Value::as_str)
        && let Ok(mut report) = serde_json::from_str::<Value>(r.trim())
    {
        inject_dep_cycles(&mut report, project_dir);
        if let Ok(text) = serde_json::to_string(&report)
            && let Some(obj) = step_out.as_object_mut()
        {
            obj.insert("response".into(), json!(text));
        }
    }
}

/// One evaluated link, for the chain output + gate events.
fn link_phase(name: &str) -> &'static str {
    match name {
        "resolve" => "resolve",
        "plan" => "plan",
        "implement" => "implement",
        "review" => "review",
        "verify" => "verify",
        _ => "phase",
    }
}

/// Run the ship chain: invoke each link via the durable engine, evaluate
/// the §4.5 contract between links, gate per `mode`. Every gate evaluation
/// is logged (`phase_gate` events, visible in `flywheel runs`); warn never
/// blocks, block bails with the named evidence.
#[allow(clippy::too_many_arguments)]
pub async fn run_ship_chain(
    engine: &WorkflowEngine,
    resolver: &toron_workflow::WorkflowResolver,
    project_dir: &std::path::Path,
    project: &str,
    run_id: &str,
    ship: &Workflow,
    inputs: Value,
    mode: GateMode,
) -> Result<Value> {
    let links = ship_links(ship);
    anyhow::ensure!(
        !links.is_empty(),
        "ship chain has no call_workflow links to run"
    );
    let mut steps_out: serde_json::Map<String, Value> = serde_json::Map::new();
    let mut gates: Vec<Value> = Vec::new();
    let mut failed_gates = 0usize;

    for (i, (name, workflow_name)) in links.iter().enumerate() {
        let sub = resolver(workflow_name).ok_or_else(|| {
            anyhow::anyhow!("ship link {name}: unknown workflow {workflow_name:?}")
        })?;
        let link_inputs = ship
            .steps
            .iter()
            .find_map(|s: &Step| match &s.action {
                Action::CallWorkflow {
                    workflow,
                    inputs: tpl,
                } if workflow == workflow_name => {
                    Some(render_link_inputs(tpl, &inputs, &steps_out))
                }
                _ => None,
            })
            .unwrap_or_else(|| json!({}));
        let sub_run_id = format!("{run_id}-link{i}-{name}");
        let link_out = engine.invoke(&sub_run_id, sub, link_inputs.clone()).await?;
        let mut step_out = phase_dispatch_output(&link_out);
        let phase = link_phase(name);
        if phase == "plan" {
            merge_dep_cycles_into_step(&mut step_out, project_dir);
        }
        let verdict = check_phase(phase, &step_out);
        // The child engine run is useful for execution tracing, but the
        // evaluation belongs to the parent ship run so `artifacts stitch
        // <run_id>` exposes the complete chain in one evidence trail.
        let evaluation = crate::artifact::EvaluationEvidence::from_phase_contract(
            run_id,
            "ship",
            name,
            &link_inputs,
            &step_out,
            &verdict,
        );
        let (evaluation_path, evaluation_error) =
            match crate::artifact::write_evaluation_evidence(project, &evaluation) {
                Ok(path) => (Some(path.display().to_string()), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
        let gate_ok = verdict.ok && evaluation_path.is_some();
        crate::state::log_event(
            project,
            crate::state::EventKind::Workflow,
            json!({
                "workflow": "ship",
                "link": name,
                "phase": phase,
                "phase_gate": true,
                "gate_ok": gate_ok,
                "contract_ok": verdict.ok,
                "gate_mode": format!("{mode:?}").to_lowercase(),
                "evidence": verdict.evidence,
                "evaluation_artifact": evaluation_path,
                "evaluation_error": evaluation_error,
                "run_id": sub_run_id,
            }),
        )
        .ok();
        if !gate_ok {
            failed_gates += 1;
            let failure = evaluation_error
                .as_deref()
                .map(|error| format!("evaluation evidence was not persisted: {error}"))
                .unwrap_or_else(|| format!("phase contract failed: {}", verdict.evidence));
            eprintln!(
                "flywheel: ship gate {name} FAILED ({failure}) — {}",
                match mode {
                    GateMode::Warn => "warn-first: chain continues with gate-warnings",
                    GateMode::Block => "BLOCKING: chain refuses to continue",
                }
            );
            if mode == GateMode::Block {
                anyhow::bail!("ship chain blocked at link {name}: {failure}");
            }
        }
        gates.push(json!({
            "link": name,
            "phase": phase,
            "ok": gate_ok,
            "contract_ok": verdict.ok,
            "evidence": verdict.evidence,
            "evaluation_artifact": evaluation_path,
            "evaluation_hash": evaluation.evidence_hash,
            "evaluation_error": evaluation_error,
        }));
        steps_out.insert(name.clone(), step_out);
    }

    let status = if failed_gates == 0 {
        "completed"
    } else {
        "gate-warnings"
    };
    Ok(json!({
        "links": steps_out,
        "gates": gates,
        "gate_mode": format!("{mode:?}").to_lowercase(),
        "failed_gates": failed_gates,
        "status": status,
    }))
}

/// The ship interception predicate: the Run arm routes this workflow
/// through `run_ship_chain` instead of a plain invoke, so the §4.5 gates
/// sit between the links (the plain engine path has no gate consumer —
/// guards cannot call contract functions).
pub fn is_ship_chain(workflow_name: &str, wf: &Workflow) -> bool {
    workflow_name == "ship" && ship_links(wf).len() >= 2
}

/// bd-cvv9 §3.3 routing (the ONE rail with gates): the Run arm invokes
/// through this instead of a bare `engine.invoke`, so the `ship` chain
/// gets its §4.5 gates between links while every other workflow keeps the
/// plain durable invoke byte-for-byte.
#[allow(clippy::too_many_arguments)]
pub async fn invoke_or_ship_chain(
    engine: &toron_workflow::WorkflowEngine,
    resolver: &toron_workflow::WorkflowResolver,
    project_dir: &std::path::Path,
    project: &str,
    run_id: &str,
    wf_name: &str,
    wf: &Workflow,
    inputs: Value,
) -> Result<Value> {
    if is_ship_chain(wf_name, wf) {
        run_ship_chain(
            engine,
            resolver,
            project_dir,
            project,
            run_id,
            wf,
            inputs,
            gate_mode(),
        )
        .await
    } else {
        engine.invoke(run_id, wf.clone(), inputs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ---- render_link_inputs -------------------------------------------

    #[test]
    fn render_link_inputs_resolves_inputs_and_steps_refs() {
        let template = json!({ "goal": "{{inputs.objective}}", "plan": "{{steps.plan}}" });
        let out = render_link_inputs(
            &template,
            &json!({ "objective": "fix the flake" }),
            &serde_json::Map::from_iter([(
                "plan".into(),
                json!({ "beads": [{ "id": "bd-x1", "acceptance": "y" }] }),
            )]),
        );
        assert_eq!(out["goal"], "fix the flake");
        assert_eq!(out["plan"]["beads"][0]["id"], "bd-x1");
    }

    #[test]
    fn render_link_inputs_leaves_literals_alone() {
        let out = render_link_inputs(&json!("cargo test"), &json!({}), &Default::default());
        assert_eq!(out, json!("cargo test"));
    }

    // ---- plan-artifact warn gate --------------------------------------

    fn implement_spec(plan: Option<&str>) -> toron_workflow::schema::DispatchSpec {
        serde_json::from_value(json!({ "phase": "implement", "profile": "@coder",
            "plan": plan, "acceptance": "cargo test" }))
        .expect("spec parses")
    }

    #[test]
    fn implement_without_plan_artifact_is_warnworthy() {
        let spec = implement_spec(None);
        assert!(plan_artifact_missing(&spec, &json!({ "objective": "x" })));
    }

    #[test]
    fn implement_with_plan_artifact_is_clean() {
        // spec.plan carrying the prior link's artifact (rendered, not the template)
        let spec = implement_spec(Some("{\"beads\":[...]}"));
        assert!(!plan_artifact_missing(&spec, &json!({})));
        // unrendered template ref with NO other artifact stays warnworthy
        let spec = implement_spec(Some("{{steps.plan}}"));
        assert!(plan_artifact_missing(&spec, &json!({})));
        // run inputs carrying the plan
        let spec = implement_spec(None);
        assert!(!plan_artifact_missing(
            &spec,
            &json!({ "plan": { "beads": [] } })
        ));
    }

    #[test]
    fn non_implement_phases_are_never_warnworthy() {
        let spec: toron_workflow::schema::DispatchSpec =
            serde_json::from_value(json!({ "phase": "review", "n": 2 })).unwrap();
        assert!(!plan_artifact_missing(&spec, &json!({})));
    }

    #[test]
    fn annotate_adds_the_tag_only_when_missing() {
        let spec = implement_spec(None);
        let out = annotate_dispatch_output(&spec, &json!({}), json!({ "ok": true }));
        assert_eq!(out["plan_artifact_missing"], json!(true));
        let clean = annotate_dispatch_output(
            &implement_spec(Some("rendered-plan")),
            &json!({}),
            json!({ "ok": true }),
        );
        assert!(clean.get("plan_artifact_missing").is_none());
    }

    // ---- ship shape ----------------------------------------------------

    #[test]
    fn is_ship_chain_requires_the_name_and_links() {
        let not_ship = crate::builtin_workflows::builtin_workflows()
            .get("goal-direct")
            .expect("goal-direct")
            .clone();
        assert!(!is_ship_chain("goal-direct", &not_ship));
    }

    // ---- bd-cvv9 wiring: invoke_or_ship_chain routing -------------------

    /// Contract-passing reports per phase — the fake leaf every linked
    /// built-in dispatch lands on. The plan report PRE-injects dep_cycles=0
    /// so the test does not depend on a live `br` tracker.
    fn report_runner() -> toron_workflow::engine::EffectRunner {
        Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
            let phase = match &inv.action {
                Action::DispatchJob { spec, .. } => spec.phase.clone().unwrap_or_default(),
                _ => String::new(),
            };
            Ok(match phase.as_str() {
                "resolve" => json!({
                    "target": "t", "non_goals": ["n"],
                    "blast_radius": "b", "open_questions": []
                }),
                "plan" => json!({
                    "beads": [{ "id": "bd-test1", "acceptance": "true" }],
                    "dep_cycles": 0
                }),
                "implement" => json!({
                    "commit_sha": "abc1234",
                    "commit_message": "fix(chain): witness (bd-test1)",
                    "gate": { "command": "true", "exit": 0, "output": "ok" }
                }),
                "review" => json!({ "findings": ["minor nit"], "verdict": "AGREE" }),
                "verify" => json!({
                    "gate": { "command": "true", "exit": 0, "output": "ok" }
                }),
                _ => json!({ "ok": true }),
            })
        })
    }

    #[test]
    fn ship_routes_through_the_gated_chain_goal_direct_stays_plain() {
        let wfs = crate::builtin_workflows::builtin_workflows();
        let ship = wfs.get("ship").expect("ship builtin").clone();
        let goal_direct = wfs.get("goal-direct").expect("goal-direct").clone();
        let resolver: toron_workflow::WorkflowResolver = Arc::new(|n| {
            crate::builtin_workflows::builtin_workflows()
                .get(n)
                .cloned()
        });
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let _enter = rt.enter();
        let engine = toron_workflow::WorkflowEngine::local_with_resolvers_and_effect(
            crate::profile::ProfileCatalog::catalog_profile_resolver(),
            resolver.clone(),
            Some(report_runner()),
        )
        .expect("local engine");
        let tmp = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();

        // ship → gated chain output (gates between ALL five links)
        let out = rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                invoke_or_ship_chain(
                    &engine,
                    &resolver,
                    &tmp,
                    "bd-cvv9-test",
                    &format!("bd-cvv9-ship-{nanos}"),
                    "ship",
                    &ship,
                    json!({ "objective": "o", "data_shape": "d", "gate": "true" }),
                ),
            )
            .await
            .expect("chain must not park")
            .expect("gated chain completes")
        });
        assert_eq!(out["status"], "completed", "{out}");
        let gates = out["gates"].as_array().expect("gates array");
        assert_eq!(gates.len(), 5, "one gate per link: {gates:?}");
        assert!(
            gates.iter().all(|g| g["ok"] == json!(true)),
            "every §4.5 contract passed: {gates:?}"
        );
        let links: Vec<&str> = gates.iter().filter_map(|g| g["link"].as_str()).collect();
        assert_eq!(
            links,
            vec!["resolve", "plan", "implement", "review", "verify"]
        );

        // goal-direct → plain invoke (no gates key on the output)
        let out2 = rt.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                invoke_or_ship_chain(
                    &engine,
                    &resolver,
                    &tmp,
                    "bd-cvv9-test",
                    &format!("bd-cvv9-goal-{nanos}"),
                    "goal-direct",
                    &goal_direct,
                    json!({ "objective": "o" }),
                ),
            )
            .await
            .expect("plain run must not park")
            .expect("plain invoke completes")
        });
        assert!(out2.get("gates").is_none(), "no chain gates on plain runs");
    }
}
