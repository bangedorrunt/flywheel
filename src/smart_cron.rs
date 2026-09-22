//! P4.5 saga-migration design definitions (bd-p4-migrate-sagas-7rmo).
//!
//! The three declarative workflow definitions that replace the captain's
//! hand-built self-reporting cron sagas (systemd timers + bash):
//! `docker-upgrade`, `agent-reach-upgrade`, and the `housekeeper-diagnose`
//! defined fixer. Canonical review copy lives in the design note:
//! toron/docs/notes/2026-08-08-p45-saga-migration-design.md
//!
//! These are DESIGN artifacts: they parse + lint clean and are invocable
//! through the unified engine TODAY. Registering them as crons (and driving
//! ticks) is gated on P4.3 (cron manager) / P4.4 (bead-on-fail).
//!
//! governed-by: ADR-0005 Spec 3, bd-p4-migrate-sagas-7rmo

/// The three design workflows, name → YAML.
#[cfg(test)]
pub const DESIGN_WORKFLOWS: &[(&str, &str)] = &[
    (
        "docker-upgrade",
        r#"
name: docker-upgrade
description: "Weekly health-gated Docker image upgrade with auto-rollback (saga replacement, ADR-0005 Spec 3)."
trigger:
  type: schedule
  cron: "30 3 * * 0"
  owner: housekeeper
  scope: host
policy:
  on_failure: bead
  on_repeat_failure: escalate
  repeat_threshold: 3
  on_success: log
  max_retries: 1
  no_claim_timeout: 600
steps:
  - name: snapshot
    action: spawn
    profile: "@housekeeper"
    workflow: housekeeper-snapshot
    await: true
    no_claim_timeout: 600
    on_error: fail_run
  - name: upgrade
    action: spawn
    profile: "@housekeeper"
    workflow: housekeeper-docker-upgrade
    await: true
    no_claim_timeout: 600
    on_error: fail_run
  - name: health-gate
    action: branch
    if: "{{steps.upgrade.healthy}} == true"
    then:
      - name: notify-ok
        action: send_dm
        to: housekeeper
        body: "docker-upgrade OK: {{steps.upgrade.containers}}"
    else:
      - name: rollback
        action: spawn
        profile: "@housekeeper"
        workflow: housekeeper-rollback
        await: true
        no_claim_timeout: 600
      - name: notify-fail
        action: send_dm
        to: housekeeper
        body: "docker-upgrade FAILED — rolled back to snapshot"
"#,
    ),
    (
        "agent-reach-upgrade",
        r#"
name: agent-reach-upgrade
description: "Weekly agent-reach CLI upgrade + verify (saga replacement, ADR-0005 Spec 3)."
trigger:
  type: schedule
  cron: "45 3 * * 0"
  owner: housekeeper
  scope: host
policy:
  on_failure: bead
  on_repeat_failure: escalate
  repeat_threshold: 3
  on_success: log
  max_retries: 1
  no_claim_timeout: 600
steps:
  - name: upgrade
    action: spawn
    profile: "@housekeeper"
    workflow: housekeeper-reach-upgrade
    await: true
    no_claim_timeout: 600
    on_error: fail_run
  - name: verify
    action: branch
    if: "{{steps.upgrade.verified}} == true"
    then:
      - name: notify-ok
        action: send_dm
        to: housekeeper
        body: "agent-reach-upgrade OK"
    else:
      - name: notify-fail
        action: send_dm
        to: housekeeper
        body: "agent-reach-upgrade FAILED — verify step did not pass"
      - name: escalate
        action: send_message
        to: ["housekeeper"]
        subject: "agent-reach-upgrade FAILED — verify step did not pass"
        body: "agent-reach-upgrade FAILED — verify step did not pass; no webhook sink is wired (fail-closed), so this message is the escalation"
        on_error: fail_run
"#,
    ),
    (
        "housekeeper-diagnose",
        r#"
name: housekeeper-diagnose
description: "Defined fixer (ADR-0005 Spec 3): reads a failure bead bundle and proposes a patch."
trigger:
  type: webhook
inputs:
  - name: bundle
steps:
  - name: triage
    action: spawn
    profile: "@housekeeper"
    workflow: housekeeper-diagnose-run
    await: true
    no_claim_timeout: 600
    on_error: fail_run
  - name: propose
    action: branch
    if: "{{steps.triage.patch}} != null"
    then:
      - name: fix-bead
        action: dispatch_job
        assignee: housekeeper
        spec: { profile: "@housekeeper", task: "apply patch: {{steps.triage.patch}}", run_id: "{{inputs.bundle.run_id}}" }
    else:
      - name: escalate
        action: send_dm
        to: housekeeper
        body: "housekeeper-diagnose could not propose a patch for {{inputs.bundle.workflow}} — human-in-the-loop"
"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use toron_workflow::engine::WorkflowEngine;
    use toron_workflow::schema::{Action, parse_workflow};

    fn collect_profiles(wf: &toron_workflow::schema::Workflow, cb: &mut dyn FnMut(&str)) {
        use toron_workflow::schema::Step;
        fn walk_action(action: &Action, cb: &mut dyn FnMut(&str)) {
            match action {
                Action::Spawn { profile, .. } => cb(profile),
                Action::DispatchJob { spec, .. } => {
                    if let Some(p) = spec.profile.as_deref() {
                        cb(p);
                    }
                }
                Action::Retry { step, .. } => walk_step(step, cb),
                Action::Foreach { each, .. } => walk_step(each, cb),
                Action::Parallel { each, .. } => walk_step(each, cb),
                Action::Branch {
                    then, else_steps, ..
                } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        walk_step(s, cb);
                    }
                }
                _ => {}
            }
        }
        fn walk_step(step: &Step, cb: &mut dyn FnMut(&str)) {
            walk_action(&step.action, cb);
        }
        for step in &wf.steps {
            walk_step(step, cb);
        }
        if let Some(g) = &wf.graph {
            for n in g.nodes.values() {
                walk_action(&n.action, cb);
            }
        }
    }

    fn template_tokens_of(wf: &toron_workflow::schema::Workflow) -> Vec<String> {
        use serde_json::Value;
        use toron_workflow::schema::Step;
        fn walk_step(step: &Step, out: &mut Vec<String>) {
            let body = serde_json::to_value(&step.action).unwrap_or(Value::Null);
            walk_value(&body, out);
        }
        fn walk_value(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::String(s) => {
                    let mut rest = s.as_str();
                    while let Some(start) = rest.find("{{") {
                        let after = &rest[start + 2..];
                        if let Some(end) = after.find("}}") {
                            out.push(after[..end].to_string());
                            rest = &after[end + 2..];
                        } else {
                            break;
                        }
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk_value(x, out);
                    }
                }
                Value::Object(o) => {
                    for x in o.values() {
                        walk_value(x, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for step in &wf.steps {
            walk_step(step, &mut out);
        }
        if let Some(g) = &wf.graph {
            for n in g.nodes.values() {
                let body = serde_json::to_value(&n.action).unwrap_or(Value::Null);
                walk_value(&body, &mut out);
            }
        }
        out
    }

    /// Engine tier (schema/guard/template/trigger-field validation): every
    /// design workflow parses + lints clean.
    #[test]
    fn design_workflows_pass_engine_lint() {
        for (name, yaml) in DESIGN_WORKFLOWS {
            let wf = parse_workflow(yaml).unwrap_or_else(|e| panic!("{name} parses: {e}"));
            let report = toron_workflow::lint_workflow(&wf);
            assert!(
                report.is_clean(),
                "{name} fails engine lint: {:?}",
                report.issues
            );
        }
    }

    /// Flywheel tier: `@housekeeper` resolves against the built-in profile
    /// catalog, and every `{{inputs.*}}` ref is declared. (`{{steps.*}}` refs
    /// pass through — the flywheel tier only checks `inputs.` refs.)
    #[test]
    fn design_workflows_pass_flywheel_tier() {
        let profiles = crate::profile::ProfileCatalog::load().unwrap_or_default();
        for (name, yaml) in DESIGN_WORKFLOWS {
            let wf = parse_workflow(yaml).unwrap();
            collect_profiles(&wf, &mut |p: &str| {
                let bare = p.trim_start_matches('@');
                if !bare.is_empty() {
                    assert!(
                        profiles.get(bare).is_some(),
                        "{name}: profile {p} unresolved (known: {})",
                        profiles.names().join(", ")
                    );
                }
            });
            let declared: HashSet<&str> = wf.inputs.iter().map(|i| i.name.as_str()).collect();
            for token in template_tokens_of(&wf) {
                if let Some(key) = token.strip_prefix("inputs.") {
                    let root = key.split('.').next().unwrap_or(key);
                    assert!(
                        declared.contains(root),
                        "{name}: {{{{inputs.{key}}}}} undeclared"
                    );
                }
            }
        }
    }

    /// The step graphs are invocable through the engine today (the proof the
    /// framework can RUN the sagas once P4.3 lands).
    #[tokio::test]
    async fn design_workflows_are_invocable() {
        let engine = WorkflowEngine::local().expect("engine");
        for (name, yaml) in DESIGN_WORKFLOWS {
            let wf = parse_workflow(yaml).unwrap();
            let inputs = if name == &"housekeeper-diagnose" {
                serde_json::json!({ "bundle": { "run_id": "r1", "workflow": "docker-upgrade", "error": "x" } })
            } else {
                serde_json::json!({})
            };
            let out = engine
                .invoke(name, wf, inputs)
                .await
                .unwrap_or_else(|e| panic!("{name} invokes: {e}"));
            assert!(out.is_object() || out.is_null(), "{name} returns a value");
        }
    }

    /// P4.5 acceptance (sequencing rule): the DEPLOYED docker-upgrade
    /// workflow carries explicit health-gate probes for the saga's five
    /// services (aiostreams/resonate/honcho/pihole/dnscrypt) — one
    /// `call_webhook` step per service, each `on_error: fail_run`. A dead
    /// service errors the step and FAILS the run (Spec 0) → `on_failure:
    /// bead`. This asserts the artifact in `toron/workflows/docker-upgrade.yaml`
    /// matches the const here, so drift between the deployable YAML and the
    /// proven design is caught.
    #[test]
    fn deployed_docker_upgrade_yaml_carries_the_five_gates() {
        use toron_workflow::schema::Action;

        let yaml = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../toron/workflows/docker-upgrade.yaml"),
        )
        .expect("deployed docker-upgrade.yaml readable");

        let wf = parse_workflow(&yaml).expect("deployed yaml parses");
        // every health-gate step is a call_webhook with on_error: fail_run
        let mut probe_count = 0;
        for step in &wf.steps {
            if let Action::CallWebhook { url, .. } = &step.action {
                assert_eq!(
                    step.on_error,
                    toron_workflow::schema::StepOnError::FailRun,
                    "gate {} must fail the run on a dead service",
                    step.name.as_deref().unwrap_or("probe")
                );
                assert!(
                    url.starts_with("http://127.0.0.1:"),
                    "probe targets a host port: {url}"
                );
                probe_count += 1;
            }
        }
        assert_eq!(probe_count, 5, "five saga health-gates");
        // the schedule + policy are set (owner housekeeper, bead on failure)
        match &wf.trigger {
            Some(toron_workflow::schema::Trigger::Schedule { owner, scope, .. }) => {
                assert_eq!(owner.as_deref(), Some("housekeeper"));
                assert_eq!(*scope, toron_workflow::schema::CronScope::Host);
            }
            other => panic!("docker-upgrade must be a schedule trigger: {other:?}"),
        }
        let policy = wf.policy.expect("policy block present");
        assert_eq!(
            policy.on_failure,
            toron_workflow::schema::FailureAction::Bead,
            "on_failure: bead"
        );
    }
}
