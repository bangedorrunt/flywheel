// governed-by: ADR-0025
//! Headless harness adapter (ADR-0010 Spec A/E, P10.1): runs
//! `harness -p --mode=json PROMPT` per harness, captures the JSON stream,
//! and returns response + token usage.
//!
//! Functional core / imperative shell: argv construction and stream parsing
//! are pure (exhaustively unit-tested); the subprocess spawn is the thin IO
//! shell verified end-to-end by the P10.1 gate.
//!
//! Harness shapes (verified against each binary's --help, 2026-08-09):
//! - `pi`:      `pi -p --mode json [--model X] PROMPT` → NDJSON stream
//! - `omp`:     `omp -p --mode=json [--model=X] PROMPT` → NDJSON stream
//! - `opencode`: `opencode run --format json [-m X] PROMPT` → NDJSON stream
//! - `grok`:    `grok -p --output-format streaming-messages-json [-m X] PROMPT`
//!
//! governed-by: ADR-0010 (Spec A, Spec E)

use anyhow::{Result, bail};
use serde_json::Value;
use toron_workflow::schema::DispatchSpec;

/// The outcome of one headless harness invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessOutcome {
    pub response: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub status: String,
    pub model: String,
    pub duration_ms: u64,
    pub exit_code: i32,
    pub error: Option<String>,
}

/// The model flag args for one harness (`-m ID` / `--model ID` / `--model=ID`),
/// matching each binary's `--help` (verified 2026-08-09). Shared by the
/// headless argv builder and the pane-runner spawn path (P10.3, bd-ghcj.1).
/// omp also carries the thinking/effort flag (`--thinking=<level>`,
/// bd-42fs) when an effort is given.
pub fn model_args(harness: &str, model: &str, effort: Option<&str>) -> Vec<String> {
    match harness {
        "opencode" | "grok" => vec!["-m".to_string(), model.to_string()],
        "omp" => {
            let mut args = vec![format!("--model={model}")];
            if let Some(e) = effort.filter(|e| !e.is_empty()) {
                args.push(format!("--thinking={e}"));
            }
            args
        }
        "pi" => vec!["--model".to_string(), model.to_string()],
        // H2 (bd-harness-jcode): jcode takes provider and model as SEPARATE
        // flags (`-p opencode-go -m muse-spark-1.2-contributor`); the
        // composite registry key is split so `-m` carries the bare id.
        "jcode" => {
            let (provider, bare) = crate::models::split_key(model);
            let mut args = Vec::new();
            if !provider.is_empty() {
                args.push("-p".to_string());
                args.push(provider.to_string());
            }
            if !bare.is_empty() {
                args.push("-m".to_string());
                args.push(bare.to_string());
            }
            args
        }
        // H1 (bd-harness-prime-agent-818f): prime-agent takes `--model` +
        // `--thinking` (levels: off/minimal/low/medium/high/xhigh/max).
        // `--offline` lives in the spawn recipe, not here — launch args may
        // target either the recipe path (herdr-unknown kind) or a future
        // `agent.start --kind pa`.
        "pa" => {
            let mut args = vec!["--model".to_string(), model.to_string()];
            if let Some(e) = effort.filter(|e| !e.is_empty()) {
                args.push("--thinking".to_string());
                args.push(e.to_string());
            }
            args
        }
        _ => vec![],
    }
}

/// ADR-0022 L2 (bd-adr22-w2-phase-registry-igya): per-harness launch args
/// for an orchestration phase. Returns (args, degraded): degraded=true
/// means the harness has no native mechanic for this phase — the caller
/// WARNS (never silent), tags the run log `phase_degraded`, and the
/// dispatch proceeds in prompt mode. One match arm per harness; this
/// registry is the ONLY place a harness's phase vocabulary lives.
pub fn phase_launch_args(harness: &str, phase: &str) -> (Vec<String>, bool) {
    // Mechanics verified against the installed binaries 2026-08-21:
    // - omp `--plan-yolo`: "Force read-only plan mode at start" (omp --help).
    // - opencode `--agent plan`: the builtin `plan` (primary) agent
    //   (`opencode agent list`). opencode reads `--agent <name>`.
    // - opencode `--agent build` (bd-adr22-implement-not-degraded-puhs):
    //   the builtin `build` (primary) implement agent, same list.
    // pi/grok expose no phase mechanic — documented loud degradation.
    match (harness, phase) {
        // bd-adr22-implement-not-degraded-puhs: implement IS the no-flag
        // baseline on every harness — baseline mode is not a degrade. The
        // single exception: opencode's real `build` agent is mechanized.
        ("opencode", "implement") => (vec!["--agent".to_string(), "build".to_string()], false),
        (_, "implement") => (vec![], false),
        ("omp", "plan") => (vec!["--plan-yolo".to_string()], false),
        ("opencode", "plan") => (vec!["--agent".to_string(), "plan".to_string()], false),
        _ => (vec![], true),
    }
}

/// The omp role-model flags (`--smol=<id>` / `--slow=<id>` / `--plan=<id>`,
/// bd-gsvn.2). omp-only — other harnesses use `model` for every role and get
/// no role flags (per-harness gate: absent role ids emit nothing). Each role
/// id is emitted verbatim; registry resolution happens before this (the
/// caller passes the resolved provider/id form like `model_args`).
pub fn role_model_args(harness: &str, smol: &str, slow: &str, plan: &str) -> Vec<String> {
    if harness != "omp" {
        return vec![];
    }
    let mut args = Vec::new();
    for (flag, id) in [("--smol", smol), ("--slow", slow), ("--plan", plan)] {
        if !id.is_empty() {
            args.push(format!("{flag}={id}"));
        }
    }
    args
}

/// `build_argv` + the omp role models (`--smol`/`--slow`/`--plan`,
/// bd-gsvn.2). The spawn + headless paths pass the profile's role fields;
/// other harnesses ignore them (per-harness gate inside `role_model_args`).
pub fn build_argv_roles(
    harness: &str,
    model: Option<&str>,
    effort: Option<&str>,
    smol: &str,
    slow: &str,
    plan: &str,
    prompt: &str,
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();
    match harness {
        "pi" => {
            argv.extend(["pi", "-p", "--mode", "json"].map(String::from));
            if let Some(m) = model {
                argv.extend(model_args("pi", m, None));
            }
        }
        "omp" => {
            argv.extend(["omp", "-p", "--mode=json"].map(String::from));
            if let Some(m) = model {
                argv.extend(model_args("omp", m, effort));
            }
            argv.extend(role_model_args("omp", smol, slow, plan));
        }
        "opencode" => {
            argv.extend(["opencode", "run", "--format", "json"].map(String::from));
            if let Some(m) = model {
                argv.extend(model_args("opencode", m, None));
            }
        }
        // H2 (bd-harness-jcode): headless one-shot via `jcode run`. Plain
        // output (parse_stream falls back to raw stdout as the response when
        // no NDJSON stream is present), so no `--json` flag here.
        "jcode" => {
            argv.extend(["jcode", "run"].map(String::from));
            if let Some(m) = model {
                argv.extend(model_args("jcode", m, None));
            }
        }
        "grok" => {
            // `-p` alone fails this grok build ("a value is required for
            // '--single <PROMPT>'"); the prompt must ride the `--single`
            // value. Build it here (the prompt is appended at the end for
            // the other harnesses).
            let single = format!("--single={prompt}");
            argv.extend(
                [
                    "grok",
                    &single,
                    "--output-format",
                    "streaming-messages-json",
                ]
                .map(String::from),
            );
            if let Some(m) = model {
                argv.extend(model_args("grok", m, None));
            }
            return Ok(argv);
        }
        other => {
            bail!("unsupported harness for headless run: {other} (pi | omp | opencode | grok)")
        }
    }
    argv.push(prompt.to_string());
    Ok(argv)
}

/// Parse a harness's stdout into an outcome (pure). Harnesses that emit a
/// JSON stream (NDJSON message events) yield response + usage; anything else
/// falls back to raw stdout as the response.
///
/// Stream contract (verified against pi/omp/opencode/grok 2026-08-09): one
/// JSON object per line; the LAST assistant `message_end` carries
/// `message.content[].text` + `message.usage.{input,output}` +
/// `message.model`. Non-JSON lines (startup banners) are skipped; a stream
/// with no JSON at all is treated as plain text.
pub fn parse_stream(stdout: &str, harness: &str) -> HarnessOutcome {
    let _ = harness; // parser hint; the NDJSON shape is harness-agnostic today
    let mut response = String::new();
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut model = String::new();
    let mut error: Option<String> = None;
    let mut saw_json = false;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue; // banner noise, not stream corruption
        };
        saw_json = true;
        if v.get("type").and_then(Value::as_str) != Some("message_end") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        if let Some(e) = msg.get("errorMessage").and_then(Value::as_str)
            && error.is_none()
        {
            error = Some(e.to_string());
        }
        if msg.get("stopReason").and_then(Value::as_str) == Some("error") && error.is_none() {
            error = Some("harness stream ended with stopReason=error".into());
        }
        let is_assistant = msg.get("role").and_then(Value::as_str) == Some("assistant");
        if let Some(content) = msg.get("content").and_then(Value::as_array) {
            let text: String = content
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if is_assistant && !text.is_empty() {
                response = text;
            }
        }
        if is_assistant {
            if let Some(usage) = msg.get("usage") {
                if let Some(n) = usage.get("input").and_then(Value::as_u64) {
                    tokens_in = n;
                }
                if let Some(n) = usage.get("output").and_then(Value::as_u64) {
                    tokens_out = n;
                }
            }
            if let Some(m) = msg.get("model").and_then(Value::as_str) {
                model = m.to_string();
            }
        }
    }
    if !saw_json {
        // plain-text harness output (no JSON stream): raw stdout IS the
        // response, no usage accounting available.
        response = stdout.to_string();
    }
    HarnessOutcome {
        response: response.trim().to_string(),
        tokens_in,
        tokens_out,
        status: if error.is_some() {
            "fail".into()
        } else {
            "success".into()
        },
        model,
        duration_ms: 0,
        exit_code: 0,
        error,
    }
}

/// Strip the `@` from a profile reference (`@planner` → `planner`).
pub fn strip_profile_ref(name: &str) -> &str {
    name.strip_prefix('@').unwrap_or(name)
}

/// The dispatch prompt: the profile's behavior preamble, then the base
/// prompt. An empty behavior (no preamble authored) passes the base through
/// untouched.
pub fn conditioned_prompt(behavior: &str, base: &str) -> String {
    if behavior.is_empty() {
        base.to_string()
    } else {
        format!("{behavior}\n\n{base}")
    }
}

/// The profile reference in a dispatch spec (`spec.profile`, e.g. `@planner`).
pub fn dispatch_spec_profile(spec: &DispatchSpec) -> Option<&str> {
    spec.profile.as_deref()
}

/// The stage name in a dispatch spec (`spec.stage`, e.g. `systematic-execute`).
pub fn dispatch_spec_stage(spec: &DispatchSpec) -> Option<&str> {
    spec.stage.as_deref()
}

/// The per-step harness override in a dispatch spec (`spec.harness`, e.g.
/// `opencode` — bd-42fs step 5). When present, the runner uses this harness
/// INSTEAD of the profile's default (review/debate workflows pin the
/// reviewer to opencode).
pub fn dispatch_spec_harness(spec: &DispatchSpec) -> Option<&str> {
    spec.harness.as_deref()
}

/// The lifecycle phase in a dispatch spec (`spec.phase`, e.g. `plan` —
/// ADR-0022 W2). Drives the phase registry ([`phase_launch_args`]) on both
/// the headless and pane launch paths.
pub fn dispatch_spec_phase(spec: &DispatchSpec) -> Option<&str> {
    spec.phase.as_deref()
}

/// `build_argv_roles` + the ADR-0022 phase registry: when `phase` is set,
/// the registry args are INSERTED BEFORE the trailing prompt positional
/// (every harness arm ends its argv with the prompt; flags after it would
/// be swallowed as prompt text). Returns `(argv, phase_degraded)` —
/// degraded=true means the harness has no native mechanic for the phase
/// and the caller must warn + tag the run log (never silent).
#[allow(clippy::too_many_arguments)]
pub fn build_argv_roles_phase(
    harness: &str,
    model: Option<&str>,
    effort: Option<&str>,
    smol: &str,
    slow: &str,
    plan: &str,
    prompt: &str,
    phase: Option<&str>,
) -> Result<(Vec<String>, bool)> {
    let mut argv = build_argv_roles(harness, model, effort, smol, slow, plan, prompt)?;
    let Some(phase) = phase.filter(|p| !p.is_empty()) else {
        return Ok((argv, false));
    };
    let (mut extra, degraded) = phase_launch_args(harness, phase);
    if degraded {
        eprintln!(
            "flywheel: phase {phase:?} has no native mechanic on harness {harness:?} — degrading to prompt mode (run log: phase_degraded)"
        );
    }
    if !extra.is_empty() {
        let insert_at = argv.len().saturating_sub(1); // before the prompt positional
        argv.splice(insert_at..insert_at, extra.drain(..));
    }
    Ok((argv, degraded))
}

/// ADR-0022 §3.2 R3 (bd-adr22-w2-phase-registry-igya): the preflight
/// phase-registry probe — does the flag/mode this registry names still
/// exist in the INSTALLED harness? The anti-flag-drift defense: a renamed
/// flag must surface as a named warning with the remedy, never a silent
/// no-op at dispatch time. `probe` runs a harness self-description command
/// and returns its stdout (None = the harness could not be probed at all).
/// Returns `Some(problem)` naming the exact remedy, or `None` when the
/// registry's mechanic is present (or the harness has no mechanic to
/// probe). Pure via the `probe` closure — unit-tested with fakes.
pub fn phase_registry_probe(
    harness: &str,
    probe: &dyn Fn(&str, &[&str]) -> Option<String>,
) -> Option<String> {
    match harness {
        "omp" => {
            let Some(out) = probe(harness, &["--help"]) else {
                return Some(
                    "omp: cannot probe plan-phase flag (omp --help failed) — verify --plan-yolo manually or fix the omp install"
                        .to_string(),
                );
            };
            if out.contains("--plan-yolo") {
                None
            } else {
                Some(
                    "omp: plan-phase flag --plan-yolo not found in `omp --help` — update headless::phase_launch_args to the installed omp's plan mechanic"
                        .to_string(),
                )
            }
        }
        "opencode" => {
            let Some(out) = probe(harness, &["agent", "list"]) else {
                return Some(
                    "opencode: cannot probe wired agents (opencode agent list failed) — verify the `plan` and `build` agents manually or fix the opencode install"
                        .to_string(),
                );
            };
            // Wired builtin agents appear as "<name> (primary)" / "<name>":
            // match each bare name on an agent-list line.
            let missing: Vec<&str> = ["plan", "build"]
                .into_iter()
                .filter(|agent| {
                    !out.lines()
                        .any(|l| l.trim_start().starts_with(*agent) || l.trim() == *agent)
                })
                .collect();
            if missing.is_empty() {
                None
            } else {
                Some(format!(
                    "opencode: wired agents not found in `opencode agent list`: {} — update headless::phase_launch_args to the installed opencode's agents",
                    missing.join(", ")
                ))
            }
        }
        // No mechanic registered -> nothing to probe.
        _ => None,
    }
}

/// The task text in a dispatch spec (`spec.task`): an inline instruction that
/// replaces the workflow-default base prompt (bd-m7ac weekly-review — scouts
/// and the oracle carry their mission inline). `None` when absent (the
/// stage/base-prompt fallback applies).
pub fn dispatch_spec_task(spec: &DispatchSpec) -> Option<&str> {
    spec.task.as_deref()
}

/// The base prompt for a headless dispatch with no explicit stage: the
/// workflow's `goal` input when present, else a workflow-named default.
pub fn base_prompt_for(wf_name: &str, inputs: &Value) -> String {
    match inputs.get("goal").and_then(Value::as_str) {
        Some(goal) if !goal.trim().is_empty() => format!("Goal: {goal}"),
        _ => format!("Execute the next step of workflow {wf_name}"),
    }
}

/// Render a dispatch-spec extra (`spec.plan`, `spec.prompt`, `spec.task`)
/// against the run inputs: `{{inputs.X}}` refs resolve to the input value;
/// an unresolvable ref stays literal (never drops the text). Empty result →
/// `None` (no extra to append).
pub fn spec_extra(spec: &DispatchSpec, key: &str, inputs: &Value) -> Option<String> {
    let raw = match key {
        "profile" => spec.profile.as_deref(),
        "task" => spec.task.as_deref(),
        "stage" => spec.stage.as_deref(),
        "harness" => spec.harness.as_deref(),
        "model" => spec.model.as_deref(),
        "effort" => spec.effort.as_deref(),
        "phase" => spec.phase.as_deref(),
        "plan" => spec.plan.as_deref(),
        "project" => spec.project.as_deref(),
        other => spec.extra.get(other).and_then(Value::as_str),
    }?
    .trim();
    if raw.is_empty() {
        return None;
    }
    let mut out = raw.to_string();
    while let Some(start) = out.find("{{inputs.") {
        let rest = &out[start + "{{inputs.".len()..];
        let Some(end) = rest.find("}}") else {
            break;
        };
        let name = &rest[..end];
        match inputs.get(name).and_then(Value::as_str) {
            Some(v) => {
                let rendered = v.to_string();
                out.replace_range(start..start + "{{inputs.".len() + end + 2, &rendered);
                // The rendered value could itself contain a ref — restart the
                // scan; an unresolvable ref (replaced by itself) must NOT
                // loop forever.
                if out[start..].starts_with("{{inputs.") {
                    break;
                }
            }
            None => break,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pi_argv_with_model() {
        assert_eq!(
            build_argv_roles(
                "pi",
                Some("google/gemini-2.5-flash"),
                None,
                "",
                "",
                "",
                "hello"
            )
            .unwrap(),
            vec![
                "pi".to_string(),
                "-p".into(),
                "--mode".into(),
                "json".into(),
                "--model".into(),
                "google/gemini-2.5-flash".into(),
                "hello".into(),
            ]
        );
    }

    #[test]
    fn pi_argv_without_model() {
        assert_eq!(
            build_argv_roles("pi", None, None, "", "", "", "hello").unwrap(),
            vec![
                "pi".to_string(),
                "-p".into(),
                "--mode".into(),
                "json".into(),
                "hello".into(),
            ]
        );
    }

    #[test]
    fn omp_argv_uses_equals_form() {
        assert_eq!(
            build_argv_roles("omp", Some("gpt-5.2"), None, "", "", "", "hi").unwrap(),
            vec![
                "omp".to_string(),
                "-p".into(),
                "--mode=json".into(),
                "--model=gpt-5.2".into(),
                "hi".into(),
            ]
        );
    }

    #[test]
    fn opencode_argv_uses_run_with_json_format() {
        assert_eq!(
            build_argv_roles(
                "opencode",
                Some("anthropic/claude-sonnet-4"),
                None,
                "",
                "",
                "",
                "hi"
            )
            .unwrap(),
            vec![
                "opencode".to_string(),
                "run".into(),
                "--format".into(),
                "json".into(),
                "-m".into(),
                "anthropic/claude-sonnet-4".into(),
                "hi".into(),
            ]
        );
    }

    #[test]
    fn grok_argv_uses_streaming_messages_json() {
        assert_eq!(
            build_argv_roles("grok", Some("grok-4"), None, "", "", "", "hi").unwrap(),
            vec![
                "grok".to_string(),
                "--single=hi".into(),
                "--output-format".into(),
                "streaming-messages-json".into(),
                "-m".into(),
                "grok-4".into(),
            ]
        );
    }

    #[test]
    fn unknown_harness_is_an_error() {
        let err = build_argv_roles("hermes", None, None, "", "", "", "hi").unwrap_err();
        assert!(err.to_string().contains("hermes"), "{err}");
    }

    /// P10.3 (bd-ghcj.1): the model-flag args are per-harness, shared with
    /// the pane-runner spawn path (opencode/grok `-m`, omp `--model=`, pi
    /// `--model`). bd-42fs: omp carries `--thinking=<effort>` when given.
    #[test]
    fn model_args_follow_harness_forms() {
        assert_eq!(
            model_args("opencode", "claude-sonnet-4", None),
            vec!["-m", "claude-sonnet-4"]
        );
        assert_eq!(model_args("grok", "grok-4", None), vec!["-m", "grok-4"]);
        assert_eq!(model_args("omp", "gpt-5.2", None), vec!["--model=gpt-5.2"]);
        // H2 (bd-harness-jcode): jcode takes provider and model separately.
        assert_eq!(
            model_args("jcode", "opencode-go/muse-spark-1.2-contributor", None),
            vec!["-p", "opencode-go", "-m", "muse-spark-1.2-contributor"]
        );
        // A bare (provider-less) model id emits only -m.
        assert_eq!(model_args("jcode", "gpt-5.5", None), vec!["-m", "gpt-5.5"]);
        assert_eq!(
            model_args("omp", "glm-5.2", Some("max")),
            vec!["--model=glm-5.2", "--thinking=max"]
        );
        assert_eq!(
            model_args("omp", "deepseek-v4-flash", Some("high")),
            vec!["--model=deepseek-v4-flash", "--thinking=high"]
        );
        assert_eq!(
            model_args("pi", "glm-5.2", None),
            vec!["--model", "glm-5.2"]
        );
        // H1 (bd-harness-prime-agent-818f): prime-agent takes `--model` +
        // `--thinking` (omitted when no effort is set).
        assert_eq!(
            model_args("pa", "opencode-go/deepseek-v4-flash", None),
            vec!["--model", "opencode-go/deepseek-v4-flash"]
        );
        assert_eq!(
            model_args("pa", "opencode-go/deepseek-v4-flash", Some("medium")),
            vec![
                "--model",
                "opencode-go/deepseek-v4-flash",
                "--thinking",
                "medium"
            ]
        );
        assert!(
            model_args("hermes", "x", None).is_empty(),
            "unknown harness: no model args"
        );
    }

    /// ADR-0022 W2 (bd-adr22-w2-phase-registry-igya): the phase registry —
    /// one match arm per harness, verified against the installed binaries
    /// 2026-08-21 (omp --plan-yolo --help; opencode agent list shows the
    /// builtin `plan` agent). The registry is the ONLY place a harness's
    /// phase vocabulary lives.
    #[test]
    fn phase_registry_table_per_harness() {
        // omp: plan -> read-only plan mode at start (native mechanic).
        let (args, degraded) = phase_launch_args("omp", "plan");
        assert_eq!(args, vec!["--plan-yolo"]);
        assert!(!degraded);
        // opencode: plan -> the builtin plan-mode agent.
        let (args, degraded) = phase_launch_args("opencode", "plan");
        assert_eq!(args, vec!["--agent", "plan"]);
        assert!(!degraded);
    }

    /// ADR-0022 W2: the argv wrapper inserts registry args BEFORE the
    /// trailing prompt positional and reports degradation to the caller.
    #[test]
    fn build_argv_roles_phase_inserts_before_prompt() {
        // omp + plan: --plan-yolo lands before the prompt, not after.
        let (argv, degraded) = build_argv_roles_phase(
            "omp",
            Some("gpt-5.2"),
            None,
            "",
            "",
            "",
            "do the thing",
            Some("plan"),
        )
        .expect("argv");
        assert!(!degraded);
        let pos_prompt = argv
            .iter()
            .position(|a| a == "do the thing")
            .expect("prompt last");
        let pos_flag = argv
            .iter()
            .position(|a| a == "--plan-yolo")
            .expect("flag present");
        assert!(pos_flag < pos_prompt, "flag must precede prompt: {argv:?}");
        assert_eq!(pos_prompt, argv.len() - 1, "prompt stays last");

        // No phase -> byte-identical to build_argv_roles, never degraded.
        let (plain, degraded) =
            build_argv_roles_phase("omp", Some("gpt-5.2"), None, "", "", "", "hi", None)
                .expect("argv");
        assert!(!degraded);
        assert_eq!(
            plain,
            build_argv_roles("omp", Some("gpt-5.2"), None, "", "", "", "hi").unwrap()
        );

        // Degraded pair: args stay empty, degraded reported (pi has no
        // mechanic) — the caller warns + tags, argv unchanged shape.
        let (argv, degraded) =
            build_argv_roles_phase("pi", None, None, "", "", "", "hi", Some("plan")).expect("argv");
        assert!(degraded);
        assert!(!argv.iter().any(|a| a.contains("plan")));
    }

    /// ADR-0022 W2: spec.phase reaches the runner through the typed spec.
    #[test]
    fn dispatch_spec_phase_reads_typed_field() {
        let spec: DispatchSpec =
            serde_json::from_value(serde_json::json!({"profile": "@planner", "phase": "plan"}))
                .expect("spec");
        assert_eq!(dispatch_spec_phase(&spec), Some("plan"));
        assert_eq!(dispatch_spec_phase(&DispatchSpec::default()), None);
    }

    /// ADR-0022 §3.2 R3: the preflight probe catches flag drift against a
    /// fake harness self-description, names the remedy, and stays silent
    /// when the mechanic is present. Harnesses without a mechanic are not
    /// probed.
    #[test]
    fn phase_registry_probe_catches_drift() {
        let good = |bin: &str, _args: &[&str]| -> Option<String> {
            match bin {
                "omp" => Some("  --plan-yolo   Force read-only plan mode".into()),
                "opencode" => Some("build (primary)\nplan (primary)\nexplore (subagent)".into()),
                _ => None,
            }
        };
        assert_eq!(phase_registry_probe("omp", &good), None);
        assert_eq!(phase_registry_probe("opencode", &good), None);
        assert_eq!(
            phase_registry_probe("pi", &good),
            None,
            "no mechanic, no probe"
        );

        let drifted = |bin: &str, _args: &[&str]| -> Option<String> {
            match bin {
                "omp" => Some("  --model=<value>  (plan flag renamed away)".into()),
                "opencode" => Some("explore (subagent)".into()),
                _ => None,
            }
        };
        let omp_problem = phase_registry_probe("omp", &drifted).expect("omp drift caught");
        assert!(omp_problem.contains("--plan-yolo"), "{omp_problem}");
        let oc_problem = phase_registry_probe("opencode", &drifted).expect("opencode drift caught");
        assert!(
            oc_problem.contains("plan") && oc_problem.contains("build"),
            "{oc_problem}"
        );

        // Partial drift: build lost but plan intact still names the missing
        // wired agent (bd-adr22-implement-not-degraded-puhs).
        let lost_build = |bin: &str, _args: &[&str]| -> Option<String> {
            match bin {
                "opencode" => Some("plan (primary)\nexplore (subagent)".into()),
                _ => None,
            }
        };
        let build_problem =
            phase_registry_probe("opencode", &lost_build).expect("build drift caught");
        assert!(build_problem.contains("build"), "{build_problem}");

        // Unprobeable harness (binary gone) -> warn, not panic.
        let dead = |_: &str, _: &[&str]| -> Option<String> { None };
        assert!(phase_registry_probe("omp", &dead).is_some());
    }

    /// ADR-0022 §3.2 R2: loud degradation. A (harness, phase) pair with no
    /// native mechanic returns empty args + degraded=true — the caller
    /// warns, tags the run log `phase_degraded`, and the dispatch proceeds
    /// in prompt mode. Never a silent no-op, never a panic.
    #[test]
    fn phase_registry_degrades_loudly_without_mechanic() {
        for harness in ["pi", "grok", "pa", "hermes-unknown"] {
            let (args, degraded) = phase_launch_args(harness, "plan");
            assert!(args.is_empty(), "{harness}: no mechanic -> no args");
            assert!(degraded, "{harness}: plan must degrade");
        }
        // Known harnesses degrade on phases they have no mechanic for.
        // bd-adr22-implement-not-degraded-puhs: `implement` is NOT in this
        // list anymore — it is the no-flag baseline (see the baseline test).
        for phase in ["resolve", "review", "ship"] {
            let (_, degraded) = phase_launch_args("omp", phase);
            assert!(degraded, "omp {phase}: no mechanic -> degraded");
            let (_, degraded) = phase_launch_args("opencode", phase);
            assert!(degraded, "opencode {phase}: no mechanic -> degraded");
        }
        // Unknown phase strings on a known harness degrade too.
        let (args, degraded) = phase_launch_args("omp", "brainstorm");
        assert!(args.is_empty());
        assert!(degraded);
    }

    /// bd-adr22-implement-not-degraded-puhs: implement IS the no-flag
    /// baseline — every harness dispatches it with zero extra args and
    /// degraded=false. Tagging every implement step `phase_degraded` was
    /// wrong and noisy: baseline mode is not a degrade. The one exception
    /// is opencode, where a REAL mechanized implement agent exists
    /// (`build (primary)` in `opencode agent list`) and gets wired.
    #[test]
    fn implement_phase_is_baseline_not_degraded_on_every_harness() {
        // No-mechanic harnesses: plain baseline, NOT degraded.
        for harness in ["pi", "grok", "omp", "pa", "hermes-unknown"] {
            let (args, degraded) = phase_launch_args(harness, "implement");
            assert!(
                args.is_empty(),
                "{harness}: implement baseline carries no args, got {args:?}"
            );
            assert!(
                !degraded,
                "{harness}: implement must not be tagged phase_degraded"
            );
        }
        // opencode: the real builtin `build` agent is the implement
        // mechanic — mechanized AND not degraded.
        let (args, degraded) = phase_launch_args("opencode", "implement");
        assert_eq!(args, vec!["--agent", "build"]);
        assert!(!degraded, "opencode implement: wired agent is not degraded");
    }

    /// bd-gsvn.2: omp role models inject as `--smol/--slow/--plan`; every
    /// other harness gets NO role flags (it uses `model` for all roles).
    #[test]
    fn role_model_args_omp_only_and_per_id() {
        // omp with all three roles.
        assert_eq!(
            role_model_args("omp", "smol-1", "slow-1", "plan-1"),
            vec!["--smol=smol-1", "--slow=slow-1", "--plan=plan-1"]
        );
        // absent roles emit nothing.
        assert_eq!(role_model_args("omp", "", "", ""), Vec::<String>::new());
        assert_eq!(
            role_model_args("omp", "smol-1", "", "plan-1"),
            vec!["--smol=smol-1", "--plan=plan-1"]
        );
        // non-omp harnesses: never.
        for h in ["opencode", "grok", "pi", "hermes"] {
            assert_eq!(
                role_model_args(h, "s", "l", "p"),
                Vec::<String>::new(),
                "{h}"
            );
        }
    }

    /// bd-gsvn.2: the full omp argv carries model + effort + all three role
    /// models in flag order; a non-omp harness ignores the roles entirely.
    #[test]
    fn build_argv_roles_injects_omp_role_models() {
        assert_eq!(
            build_argv_roles(
                "omp",
                Some("glm-5.2"),
                Some("max"),
                "smol-1",
                "slow-1",
                "plan-1",
                "hi",
            )
            .unwrap(),
            vec![
                "omp".to_string(),
                "-p".into(),
                "--mode=json".into(),
                "--model=glm-5.2".into(),
                "--thinking=max".into(),
                "--smol=smol-1".into(),
                "--slow=slow-1".into(),
                "--plan=plan-1".into(),
                "hi".into(),
            ]
        );
        // opencode ignores roles (uses -m for everything).
        assert_eq!(
            build_argv_roles(
                "opencode",
                Some("claude-sonnet-4"),
                None,
                "s",
                "l",
                "p",
                "hi"
            )
            .unwrap(),
            vec![
                "opencode".to_string(),
                "run".into(),
                "--format".into(),
                "json".into(),
                "-m".into(),
                "claude-sonnet-4".into(),
                "hi".into(),
            ]
        );
    }

    /// The real pi JSON stream shape (verified against pi 2026-08-09):
    /// NDJSON; the last assistant `message_end` carries content + usage.
    #[test]
    fn parse_stream_extracts_response_and_usage() {
        let stream = r#"{"type":"session","version":3,"id":"s1"}
{"type":"agent_start"}
{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"message_start","message":{"role":"assistant","content":[],"api":"openai-completions","model":"glm-5.2","usage":{"input":0,"output":0}}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"OK"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"OK"}],"api":"openai-completions","provider":"zai","model":"glm-5.2","usage":{"input":13,"output":3,"cacheRead":51840}}}
{"type":"agent_settled"}"#;
        let out = parse_stream(stream, "pi");
        assert_eq!(out.response, "OK");
        assert_eq!(out.tokens_in, 13);
        assert_eq!(out.tokens_out, 3);
        assert_eq!(out.status, "success");
        assert_eq!(out.model, "glm-5.2");
        assert!(out.error.is_none());
    }

    /// An error message in the stream marks the run failed and captures the
    /// error text (model 404s surface in the run log, not as panics).
    #[test]
    fn parse_stream_captures_error_message() {
        let stream = r#"{"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"{\"error\":{\"code\":404}}"}}
{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"model not found"}}
{"type":"agent_settled"}"#;
        let out = parse_stream(stream, "pi");
        assert_eq!(out.status, "fail");
        assert!(out.error.is_some(), "error captured");
        assert!(out.error.as_deref().unwrap().contains("model not found"));
    }

    /// Non-JSON stdout (e.g. a harness in plain-text mode) falls back to the
    /// raw text as the response.
    #[test]
    fn parse_stream_falls_back_to_raw_text() {
        let out = parse_stream("plain response\nsecond line", "opencode");
        assert_eq!(out.response, "plain response\nsecond line");
        assert_eq!(out.status, "success");
        assert_eq!(out.tokens_in, 0, "no usage info in plain mode");
    }

    #[test]
    fn strip_profile_ref_removes_at() {
        assert_eq!(strip_profile_ref("@planner"), "planner");
        assert_eq!(strip_profile_ref("planner"), "planner");
    }

    /// bd-m7ac: `spec.task` (the inline mission) resolves from the dispatch
    /// spec; absent tasks fall back to the stage/base-prompt path.
    #[test]
    fn dispatch_spec_task_resolves_inline_mission() {
        let with_task = DispatchSpec {
            profile: Some("@scout".into()),
            task: Some("Map toron gaps.".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_task(&with_task), Some("Map toron gaps."));
        assert_eq!(dispatch_spec_profile(&with_task), Some("@scout"));
        let no_task = DispatchSpec {
            profile: Some("@planner".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_task(&no_task), None);
        assert_eq!(dispatch_spec_task(&DispatchSpec::default()), None);
    }

    #[test]
    fn conditioned_prompt_prepends_behavior() {
        let out = conditioned_prompt("You are a PLANNER.", "Goal: X");
        assert_eq!(out, "You are a PLANNER.\n\nGoal: X");
    }

    #[test]
    fn conditioned_prompt_passes_through_without_behavior() {
        assert_eq!(conditioned_prompt("", "Goal: X"), "Goal: X");
    }

    #[test]
    fn dispatch_spec_extracts_profile_and_stage() {
        let spec = DispatchSpec {
            profile: Some("@planner".into()),
            stage: Some("ce-execute".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_profile(&spec), Some("@planner"));
        assert_eq!(dispatch_spec_stage(&spec), Some("ce-execute"));
        let bare = DispatchSpec {
            profile: Some("@coder".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_profile(&bare), Some("@coder"));
        assert_eq!(dispatch_spec_stage(&bare), None);
    }

    /// bd-42fs step 5: the per-step harness override rides in the spec.
    #[test]
    fn dispatch_spec_extracts_harness_override() {
        let spec = DispatchSpec {
            profile: Some("@reviewer".into()),
            harness: Some("opencode".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_harness(&spec), Some("opencode"));
        let bare = DispatchSpec {
            profile: Some("@reviewer".into()),
            ..Default::default()
        };
        assert_eq!(dispatch_spec_harness(&bare), None);
    }

    #[test]
    fn base_prompt_uses_goal_input() {
        let inputs = json!({ "goal": "decompose X" });
        assert_eq!(base_prompt_for("plan", &inputs), "Goal: decompose X");
    }

    #[test]
    fn base_prompt_falls_back_to_workflow_name() {
        assert_eq!(
            base_prompt_for("beads", &json!({})),
            "Execute the next step of workflow beads"
        );
    }

    /// `spec.plan` resolves `{{inputs.plan}}` against the run inputs (the
    /// beads workflow's plan reaches the planner headlessly — bd-oj9o).
    #[test]
    fn spec_extra_renders_inputs_ref() {
        let spec = DispatchSpec {
            plan: Some("{{inputs.plan}}".into()),
            ..Default::default()
        };
        let inputs = json!({ "plan": "1. fix auth 2. ship" });
        assert_eq!(
            spec_extra(&spec, "plan", &inputs).as_deref(),
            Some("1. fix auth 2. ship")
        );
    }

    /// An unresolvable ref stays literal — the text is never dropped.
    #[test]
    fn spec_extra_keeps_unresolved_ref_literal() {
        let spec = DispatchSpec {
            plan: Some("Work from {{inputs.missing}}".into()),
            ..Default::default()
        };
        assert_eq!(
            spec_extra(&spec, "plan", &json!({})).as_deref(),
            Some("Work from {{inputs.missing}}")
        );
    }

    /// Missing or empty extra → None (nothing appended to the prompt).
    #[test]
    fn spec_extra_none_when_absent_or_empty() {
        assert_eq!(
            spec_extra(&DispatchSpec::default(), "plan", &json!({})),
            None
        );
        assert_eq!(
            spec_extra(
                &DispatchSpec {
                    plan: Some("  ".into()),
                    ..Default::default()
                },
                "plan",
                &json!({})
            ),
            None
        );
    }
}
