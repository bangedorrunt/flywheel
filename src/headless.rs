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
//! - `opencode`: `opencode run --format json [--model X] PROMPT` → NDJSON stream
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
///
/// governed-by: bd-ulzi — pi wires effort natively (`--thinking=<level>`,
/// verified `pi --help`: `--thinking <level>` + `--model provider/id`
/// shorthand). opencode v2 has no effort flag (`--model provider/model#variant`
/// only, `--thinking` is a display toggle) — effort there degrades LOUDLY
/// via [`effort_degraded`] (warn at the shared site, never silent).
pub fn effort_degraded(harness: &str) -> bool {
    matches!(harness, "opencode")
}

pub fn model_args(harness: &str, model: &str, effort: Option<&str>) -> Vec<String> {
    let effort = effort.filter(|e| !e.is_empty());
    if let Some(e) = effort
        && effort_degraded(harness)
    {
        eprintln!(
            "flywheel: effort {e:?} has no native flag on harness {harness:?} — degrading to model-only (profile default stands)"
        );
    }
    match harness {
        "opencode" => vec!["--model".to_string(), model.to_string()],
        "grok" => vec!["-m".to_string(), model.to_string()],
        "pi" => {
            let mut args = vec!["--model".to_string(), model.to_string()];
            if let Some(e) = effort {
                // bd-pi-thinking-equals-form: TWO argv entries, not
                // `--thinking=<e>`. pi 0.87.1's parser accepts the space form and
                // rejects the equals form with `Unknown option: --thinking`,
                // even though its own --help advertises the flag. Covered by
                // pi_emitted_effort_arg_form_is_accepted_by_the_real_binary.
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
        ("opencode", "plan") => (vec!["--agent".to_string(), "plan".to_string()], false),
        _ => (vec![], true),
    }
}

/// The omp role-model flags (`--smol=<id>` / `--slow=<id>` / `--plan=<id>`,
/// bd-gsvn.2). omp-only — other harnesses use `model` for every role and get
/// no role flags (per-harness gate: absent role ids emit nothing). Each role
/// id is emitted verbatim; registry resolution happens before this (the
/// `build_argv` + the omp role models (`--smol`/`--slow`/`--plan`,
/// bd-gsvn.2). The spawn + headless paths pass the profile's role fields;
/// other harnesses ignore them (per-harness gate inside `role_model_args`).
pub fn build_argv_roles(
    harness: &str,
    model: Option<&str>,
    // bd-47fu: ACCEPTED AND IGNORED, deliberately. `effort` was only ever consumed by
    // the `omp` arm and the role ids only by `role_model_args` (also omp-only); both
    // went with the harness. Underscore-prefixed rather than deleted because 12 call
    // sites would need re-arguing, and a reader who finds `_smol` LEARNS the field is
    // inert, where a deleted parameter is only rediscovered by a stale caller.
    // Signature + `Profile::smol/slow/plan` cleanup is bd-dvq7.
    _effort: Option<&str>,
    _smol: &str,
    _slow: &str,
    _plan: &str,
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
        "opencode" => {
            argv.extend(["opencode", "run", "--format", "json"].map(String::from));
            if let Some(m) = model {
                argv.extend(model_args("opencode", m, None));
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

impl HarnessOutcome {
    /// bd-7wah: pull a human-usable reason out of an error frame.
    ///
    /// opencode nests the provider's own message under `error.message` and puts the
    /// provider's own kind under `error.type`, with an HTTP `status` beside it. All
    /// three matter: "400" alone does not say whose limit was hit, and
    /// "provider.invalid-request" alone does not say what to do about it.
    fn describe_error_frame(v: &Value) -> String {
        let err = v.get("error");
        let message = err
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .or_else(|| err.and_then(Value::as_str))
            .or_else(|| v.get("message").and_then(Value::as_str))
            .unwrap_or("harness reported an error with no message")
            .to_string();
        let kind = err
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("error");
        let status = err
            .and_then(|e| e.get("status"))
            .map(|s| format!(" (HTTP {s})"))
            .unwrap_or_default();
        format!("{kind}{status}: {message}")
    }
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

        // bd-7wah: a TOP-LEVEL `type: "error"` is how opencode reports a rejected
        // request, and the old parser never looked at it — the error was only ever
        // read from INSIDE a `message_end`, so a provider 400 became an empty
        // success. Measured on this host: every opencode headless dispatch was
        // failing with `maximum context length is 36864 tokens ... you requested
        // about 37930`, and 383 of 442 run records recorded it as a success.
        if v.get("type").and_then(Value::as_str) == Some("error") {
            if error.is_none() {
                error = Some(HarnessOutcome::describe_error_frame(&v));
            }
            continue;
        }
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
    } else if response.is_empty() && error.is_none() {
        // bd-7wah: we were told this is a JSON stream and it contained no assistant
        // message. That is a PROTOCOL DISAGREEMENT — the harness and this build do
        // not agree on the shape — and it used to be reported as `success` with an
        // empty response, which is how a broken dispatch looked like a working one.
        // Distinct from "the agent finished and said nothing", which is a different
        // bug with a different fix, and both are now distinguishable from success.
        error = Some(format!(
            "unrecognised stream shape: {} JSON line(s), none carrying an assistant \
             message. The harness and this build disagree on the NDJSON shape.",
            stdout.lines().filter(|l| !l.trim().is_empty()).count()
        ));
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

/// The harness this step actually runs on. A non-empty `spec.harness` wins.
/// Otherwise the catalog profile's harness, which is where `--role-harness`
/// lands. The headless subprocess reloads profiles from disk, so the parent
/// must pass this value as `--harness` or the overlay is dropped.
pub fn effective_step_harness<'a>(
    spec_harness: Option<&'a str>,
    profile_harness: &'a str,
) -> &'a str {
    match spec_harness.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => h,
        None => profile_harness,
    }
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

    /// One row per harness argv shape: pi/omp/opencode/grok differ only in
    /// their flags (pi `--mode json`, omp the `=` form, opencode `run
    /// --format json`, grok `--single` + streaming-messages-json), and a
    /// `None` model drops the model flag entirely. Same argv as the five
    /// tests this replaces.
    #[test]
    fn build_argv_roles_rows() {
        let rows = [
            (
                "pi-with-model",
                build_argv_roles(
                    "pi",
                    Some("google/gemini-2.5-flash"),
                    None,
                    "",
                    "",
                    "",
                    "hello",
                )
                .unwrap(),
                [
                    "pi",
                    "-p",
                    "--mode",
                    "json",
                    "--model",
                    "google/gemini-2.5-flash",
                    "hello",
                ]
                .map(String::from)
                .to_vec(),
            ),
            (
                "pi-without-model",
                build_argv_roles("pi", None, None, "", "", "", "hello").unwrap(),
                ["pi", "-p", "--mode", "json", "hello"]
                    .map(String::from)
                    .to_vec(),
            ),
            (
                // bd-47fu: the `omp-equals-form` row is gone with omp. No
                // live harness emits `--model=`, and pi 0.87.1 REJECTS the
                // equals form, so a row asserting it would pin a bug.
                "pi-space-form",
                build_argv_roles("pi", Some("zai/glm-5.3"), None, "", "", "", "hi").unwrap(),
                ["pi", "-p", "--mode", "json", "--model", "zai/glm-5.3", "hi"]
                    .map(String::from)
                    .to_vec(),
            ),
            (
                "opencode-run-json",
                build_argv_roles(
                    "opencode",
                    Some("anthropic/claude-sonnet-4"),
                    None,
                    "",
                    "",
                    "",
                    "hi",
                )
                .unwrap(),
                [
                    "opencode",
                    "run",
                    "--format",
                    "json",
                    "--model",
                    "anthropic/claude-sonnet-4",
                    "hi",
                ]
                .map(String::from)
                .to_vec(),
            ),
            (
                "grok-streaming-messages-json",
                build_argv_roles("grok", Some("grok-4"), None, "", "", "", "hi").unwrap(),
                [
                    "grok",
                    "--single=hi",
                    "--output-format",
                    "streaming-messages-json",
                    "-m",
                    "grok-4",
                ]
                .map(String::from)
                .to_vec(),
            ),
        ];
        for (name, got, want) in rows {
            assert_eq!(got, want, "row {name}");
        }
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
            vec!["--model", "claude-sonnet-4"]
        );
        assert_eq!(model_args("grok", "grok-4", None), vec!["-m", "grok-4"]);
        // bd-47fu: the omp `=`-form, jcode's -p/-m split, and pa's
        // `--thinking` are all gone with their harnesses. The live forms
        // are opencode `--model X`, grok `-m X`, pi `--model X` + a
        // SEPARATE `--thinking <e>` (the equals form pi rejects).
        assert_eq!(
            model_args("pi", "zai/glm-5.3", Some("max")),
            vec!["--model", "zai/glm-5.3", "--thinking", "max"]
        );
        assert_eq!(
            model_args("pi", "opencode-go/deepseek-v4.1-flash", None),
            vec!["--model", "opencode-go/deepseek-v4.1-flash"]
        );
        assert_eq!(
            model_args("pi", "opencode-go/deepseek-v4.1-flash", Some("medium")),
            vec![
                "--model",
                "opencode-go/deepseek-v4.1-flash",
                "--thinking",
                "medium"
            ]
        );
        // bd-47fu: jcode's -p/-m split and omp's `=`-form are gone with
        // their harnesses; the rows above pin every live form instead.
        // A bare (provider-less) model id still emits only the model.
        assert_eq!(model_args("grok", "gpt-5.5", None), vec!["-m", "gpt-5.5"]);
        assert_eq!(
            model_args("pi", "glm-5.2", None),
            vec!["--model", "glm-5.2"]
        );
        // H1 (bd-harness-prime-agent-818f): prime-agent takes `--model` +
        // `--thinking` (omitted when no effort is set).
        assert_eq!(
            model_args("pi", "opencode-go/deepseek-v4.1-flash", None),
            vec!["--model", "opencode-go/deepseek-v4.1-flash"]
        );
        assert_eq!(
            model_args("pi", "opencode-go/deepseek-v4.1-flash", Some("medium")),
            vec![
                "--model",
                "opencode-go/deepseek-v4.1-flash",
                "--thinking",
                "medium"
            ]
        );
        assert!(
            model_args("hermes", "x", None).is_empty(),
            "unknown harness: no model args"
        );
    }

    #[test]
    fn opencode_v2_model_flag_is_long_form() {
        assert_eq!(
            model_args("opencode", "opencode/big-pickle", None),
            vec!["--model", "opencode/big-pickle"]
        );
    }

    /// governed-by: bd-ulzi — pi wires effort natively (`--thinking=`;
    /// provider/id passes through verbatim, the pi shorthand); opencode
    /// v2 keeps `--model provider/model#variant` verbatim and degrades LOUDLY
    /// on effort (no effort flag); empty effort emits no thinking flag.
    ///
    /// bd-pi-thinking-equals-form: pi's `--thinking` is TWO argv entries. This
    /// test previously asserted the equals form `--thinking=high`, which pi's
    /// parser rejects; the shape is now pinned by
    /// `pi_emitted_effort_arg_form_is_accepted_by_the_real_binary`, which asks
    /// the binary instead of trusting this one.
    #[test]
    fn jev_effort_pi_thinking_native_opencode_loud_degrade() {
        assert_eq!(
            model_args("pi", "glm-5.2", Some("high")),
            vec!["--model", "glm-5.2", "--thinking", "high"]
        );
        assert_eq!(
            model_args("pi", "opencode-go/deepseek-v4-flash", Some("low")),
            vec![
                "--model",
                "opencode-go/deepseek-v4-flash",
                "--thinking",
                "low"
            ]
        );
        assert_eq!(
            model_args("pi", "glm-5.2", None),
            vec!["--model", "glm-5.2"]
        );
        assert_eq!(
            model_args("opencode", "acme/sonnet#thinking", Some("max")),
            vec!["--model", "acme/sonnet#thinking"],
            "opencode v2 variant passes through; effort has no flag"
        );
        assert!(effort_degraded("opencode"), "opencode effort degrades loud");
        assert!(!effort_degraded("pi"), "pi effort is native");
        // bd-47fu: opencode is the loud-degradation harness; grok's effort
        // is not asserted here because the registry records its own tier.
        assert!(
            effort_degraded("opencode"),
            "opencode effort degrades loudly"
        );
    }

    /// bd-pi-thinking-equals-form: the argv we EMIT must be argv pi PARSES.
    ///
    /// The sibling test above can only prove we emit what we emit — it locked in
    /// `--thinking=high` because that is what the code did, and its doc comment
    /// claimed `pi --help` verified it. It did not. Against pi 0.87.1:
    ///
    ///   pi --thinking=high  -> Error: Unknown option: --thinking
    ///   pi --thinking high  -> args parse; fails later on the model
    ///
    /// So the equals form is rejected by the real parser while pi's own --help
    /// advertises the space form. This test closes that gap by asking the
    /// binary, with a deliberately bogus model so it fails at model resolution
    /// (no network, no real completion) and `Unknown option` is the only signal
    /// that means the ARG FORM was wrong.
    #[test]
    fn pi_emitted_effort_arg_form_is_accepted_by_the_real_binary() {
        let bogus_model = "bogus/model-flywheel-argcheck";
        let emitted = model_args("pi", bogus_model, Some("high"));

        let mut child = match std::process::Command::new("pi")
            .args(["--print"])
            .args(&emitted)
            .arg("x")
            // stdout goes to null, NOT a pipe: this test asserts only on stderr,
            // and an unread pipe fills its buffer and blocks the child, which
            // would surface as a spurious timeout rather than a real failure.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            // pi is not installed here; the pure argv assertions still cover the
            // shape. Skipping loudly beats a green test that proved nothing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("pi not on PATH — skipping the real-binary arg check");
                return;
            }
            Err(e) => panic!("spawn pi: {e}"),
        };

        // BOUNDED WAIT (effect-oriented principle 10). `output()` would block
        // forever if pi hung — e.g. waiting on a credential prompt the
        // non-interactive flag should prevent but does not guarantee. An
        // unbounded wait in a test is a CI hang, not a failure.
        const ARGS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        let deadline = std::time::Instant::now() + ARGS_TIMEOUT;
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("pi did not exit within {ARGS_TIMEOUT:?} — refusing to hang the suite");
                }
                None => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        };

        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read as _;
            let _ = pipe.read_to_string(&mut stderr);
        }
        assert!(
            status.success() || !stderr.is_empty(),
            "pi produced neither an exit nor a diagnostic: {stderr:?}"
        );
        assert!(
            !stderr.contains("Unknown option"),
            "pi rejected the argv flywheel emits: {emitted:?}\n{stderr}\n\
             pi takes `--thinking <level>` as two argv entries, not `--thinking=<level>`."
        );
    }

    /// governed-by: bd-ulzi — the dispatch-spec effort input rides the
    /// generic renderer (typed field + `{{inputs.x}}` refs, like plan):
    /// a Jev depth→effort value or author literal reaches the leaf, which
    /// lets it win over the profile default for that step only.
    #[test]
    fn jev_effort_dispatch_spec_effort_reaches_leaf() {
        let spec = DispatchSpec {
            effort: Some("high".into()),
            ..Default::default()
        };
        assert_eq!(
            spec_extra(&spec, "effort", &serde_json::json!({})).as_deref(),
            Some("high")
        );
        let templated = DispatchSpec {
            effort: Some("{{inputs.effort}}".into()),
            ..Default::default()
        };
        assert_eq!(
            spec_extra(&templated, "effort", &serde_json::json!({"effort": "max"})).as_deref(),
            Some("max")
        );
        assert_eq!(
            spec_extra(&DispatchSpec::default(), "effort", &serde_json::json!({})),
            None,
            "absent effort means the profile default stands"
        );
    }

    /// ADR-0022 W2 (bd-adr22-w2-phase-registry-igya): the phase registry —
    /// one match arm per harness, verified against the installed binaries
    /// 2026-08-21 (omp --plan-yolo --help; opencode agent list shows the
    /// builtin `plan` agent). The registry is the ONLY place a harness's
    /// phase vocabulary lives.
    #[test]
    fn phase_registry_table_per_harness() {
        // bd-47fu: omp carried `--plan-yolo` and is dropped with its arm, so
        // a harness with no phase mechanic must DEGRADE loudly rather than
        // pretend it planned.
        let (args, degraded) = phase_launch_args("pi", "plan");
        assert_eq!(args, Vec::<String>::new());
        assert!(degraded, "no phase mechanic is a loud degradation");
        // opencode: plan -> the builtin plan-mode agent.
        let (args, degraded) = phase_launch_args("opencode", "plan");
        assert_eq!(args, vec!["--agent", "plan"]);
        assert!(!degraded);
    }

    /// ADR-0022 W2: the argv wrapper inserts registry args BEFORE the
    /// trailing prompt positional and reports degradation to the caller.
    #[test]
    fn build_argv_roles_phase_inserts_before_prompt() {
        // opencode + plan: `--agent plan` lands before the prompt, not after.
        let (argv, degraded) = build_argv_roles_phase(
            "opencode",
            Some("anthropic/claude-sonnet-4"),
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
            .position(|a| a == "--agent")
            .expect("flag present");
        assert!(pos_flag < pos_prompt, "flag must precede prompt: {argv:?}");
        assert_eq!(pos_prompt, argv.len() - 1, "prompt stays last");

        // No phase -> byte-identical to build_argv_roles, never degraded.
        let (plain, degraded) = build_argv_roles_phase(
            "opencode",
            Some("anthropic/claude-sonnet-4"),
            None,
            "",
            "",
            "",
            "hi",
            None,
        )
        .expect("argv");
        assert!(!degraded);
        assert_eq!(
            plain,
            build_argv_roles(
                "opencode",
                Some("anthropic/claude-sonnet-4"),
                None,
                "",
                "",
                "",
                "hi"
            )
            .unwrap()
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
                "opencode" => Some("build (primary)\nplan (primary)\nexplore (subagent)".into()),
                _ => None,
            }
        };
        assert_eq!(phase_registry_probe("opencode", &good), None);
        assert_eq!(
            phase_registry_probe("pi", &good),
            None,
            "no mechanic, no probe"
        );

        let drifted = |bin: &str, _args: &[&str]| -> Option<String> {
            match bin {
                "opencode" => Some("explore (subagent)".into()),
                _ => None,
            }
        };
        // bd-47fu: omp's probe arm is gone with the harness, so the drift
        // contract is proven on opencode alone.
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
        assert!(phase_registry_probe("opencode", &dead).is_some());
    }

    /// ADR-0022 §3.2 R2: loud degradation. A (harness, phase) pair with no
    /// native mechanic returns empty args + degraded=true — the caller
    /// warns, tags the run log `phase_degraded`, and the dispatch proceeds
    /// in prompt mode. Never a silent no-op, never a panic.
    #[test]
    fn phase_registry_degrades_loudly_without_mechanic() {
        for harness in ["pi", "grok", "fx", "hermes-unknown"] {
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
        assert_eq!(args, [] as [String; 0]);
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
        for harness in ["pi", "grok", "fx", "hermes-unknown"] {
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

    #[test]
    fn effective_step_harness_prefers_spec_then_profile() {
        assert_eq!(effective_step_harness(Some("opencode"), "pi"), "opencode");
        assert_eq!(effective_step_harness(Some("  "), "pi"), "pi");
        assert_eq!(effective_step_harness(None, "grok"), "grok");
        assert_eq!(effective_step_harness(Some(""), "fx"), "fx");
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

/// How a pinned model reaches a harness's INTERACTIVE entrypoint — the one
/// `herdr agent start --kind <harness>` launches, as opposed to the headless
/// one-shot `build_argv_roles` uses.
///
/// bd-crlx: this distinction is load-bearing, not cosmetic. `opencode` v2 has
/// two different entrypoints with DIFFERENT flags:
///
/// - `opencode run --model provider/model` — the headless one-shot. `--model`
///   is a real flag (`opencode run --help`: `--model, -m string`).
/// - `opencode` (the root/TUI command herdr actually launches) — flags are
///   `--standalone --server --auto --continue/-c --session/-s --prompt`.
///   There is **no `--model` and no `-m`**, and v2.0.16 rejects it outright:
///   `Unrecognized flag: --model in command opencode`.
///
/// So reusing [`model_args`] on the pane path handed opencode a flag its own
/// binary rejects. The process died on spawn, herdr's readiness wait reported
/// `agent_not_running`, and the dispatch degraded into the permanent
/// `agent <name> not found in toron` — every opencode pane seat, always.
///
/// The TUI's model is config, not argv: `opencode.json`'s `model` key, or
/// in-session `/models` (https://opencode.ai/v2/docs/models/). opencode
/// merges config sources rather than replacing them, so the pin travels as a
/// per-pane `OPENCODE_CONFIG_CONTENT` document — see
/// [`crate::herdr_shim::apply_opencode_model_pin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveModelPlan {
    /// The harness takes the model as argv. `args` are appended to the launch.
    Argv(Vec<String>),
    /// The harness takes no model flag; the pin must travel by pane env.
    PinnedByEnv { model: String },
}

impl InteractiveModelPlan {
    /// The argv half, for callers that only splice flags.
    pub fn args(&self) -> &[String] {
        match self {
            Self::Argv(a) => a,
            Self::PinnedByEnv { .. } => &[],
        }
    }

    /// The model to pin via pane env, when the harness needs one.
    pub fn env_pin(&self) -> Option<&str> {
        match self {
            Self::Argv(_) => None,
            Self::PinnedByEnv { model } => Some(model),
        }
    }
}

/// Resolve the interactive launch args for a pinned model.
///
/// Every harness except opencode takes `--model` (or its own spelling) on its
/// interactive entrypoint, so they route to [`model_args`] unchanged. opencode
/// takes no model flag at all, so it emits NO argv and the pin moves to env.
pub fn interactive_model_plan(
    harness: &str,
    model: &str,
    effort: Option<&str>,
) -> InteractiveModelPlan {
    if harness == "opencode" {
        // Still warn about effort here, exactly as model_args does, so a
        // profile asking for `effort = max` on opencode is told the pin is
        // model-only rather than silently losing the request.
        let effort = effort.filter(|e| !e.is_empty());
        if let Some(e) = effort {
            eprintln!(
                "flywheel: effort {e:?} has no native flag on harness {harness:?} — degrading to model-only (profile default stands)"
            );
        }
        return InteractiveModelPlan::PinnedByEnv {
            model: model.to_string(),
        };
    }
    InteractiveModelPlan::Argv(model_args(harness, model, effort))
}

#[cfg(test)]
mod bd_crlx_interactive_model_tests {
    use super::{InteractiveModelPlan, interactive_model_plan};

    /// bd-crlx: the regression, pinned. `--model` on the opencode ROOT command
    /// is rejected by opencode v2.0.16, so the interactive path must emit no
    /// model argv for it and must carry the pin by env instead. If this ever
    /// goes back to argv, every opencode pane seat dies on spawn again.
    #[test]
    fn opencode_interactive_emits_no_model_flag_and_pins_by_env() {
        let plan =
            interactive_model_plan("opencode", "opencode-go/muse-spark-1.3-contributor", None);
        assert_eq!(
            plan,
            InteractiveModelPlan::PinnedByEnv {
                model: "opencode-go/muse-spark-1.3-contributor".into()
            },
            "opencode TUI has no --model; the pin must travel by pane env"
        );
        assert!(
            plan.args().is_empty(),
            "no argv may reach the opencode root command: {:?}",
            plan.args()
        );
        assert!(!plan.args().iter().any(|a| a == "--model" || a == "-m"));
    }

    /// Every other harness must route to the EXISTING [`model_args`],
    /// unchanged. That is the real invariant: the opencode carve-out must not
    /// become a second arg builder that can drift from the headless one.
    ///
    /// `claude`/`codex` fall through `model_args`' `_ => vec![]` arm today, so
    /// they pin nothing — pre-existing, recorded on bd-crlx rather than
    /// silently changed here.
    #[test]
    fn other_harnesses_keep_argv_model_pins() {
        for (harness, model) in [
            ("pi", "opencode-go/muse-spark-1.3-contributor"),
            ("grok", "grok-4"),
            // opencode and fx deliver by PANE ENV, not argv (bd-crlx), so
            // they belong to the env-pin test, not this one.
            ("claude", "anthropic/opus-5"),
            ("codex", "openai/gpt-5.2"),
        ] {
            let plan = interactive_model_plan(harness, model, None);
            assert_eq!(
                plan,
                InteractiveModelPlan::Argv(super::model_args(harness, model, None)),
                "row {harness} must be exactly model_args, not a second builder"
            );
            assert!(
                plan.env_pin().is_none(),
                "row {harness} must not need an env pin"
            );
        }
    }

    /// A harness that supports a model pin must still EMIT one. Equality with
    /// `model_args` is proven above; this guards the other direction — that a
    /// future edit cannot quietly empty the argv and lose the pin silently.
    #[test]
    fn argv_pinned_harnesses_still_emit_a_pin() {
        for (harness, model) in [
            ("pi", "opencode-go/muse-spark-1.3-contributor"),
            ("grok", "grok-4"),
        ] {
            let plan = interactive_model_plan(harness, model, None);
            assert_eq!(
                plan.args(),
                &super::model_args(harness, model, None)[..],
                "row {harness}"
            );
            assert!(
                !plan.args().is_empty(),
                "row {harness} lost its model pin entirely"
            );
        }
    }

    /// The pi effort spelling must survive the interactive path — the pi
    /// `--thinking <level>` fix (bd-dcnx) is the other regression that lived
    /// in this exact function, so pin it on the interactive route too.
    #[test]
    fn pi_interactive_keeps_the_space_form_effort_flag() {
        let plan =
            interactive_model_plan("pi", "opencode-go/muse-spark-1.3-contributor", Some("high"));
        let args = plan.args();
        let i = args
            .iter()
            .position(|a| a == "--thinking")
            .expect("--thinking present");
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some("high"),
            "effort must be its own argv entry, not --thinking=high: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--thinking=")),
            "equals form is rejected by pi 0.87.1: {args:?}"
        );
    }

    /// An opencode profile asking for effort still gets told, on the
    /// interactive route. Silence here would be a silent downgrade.
    #[test]
    fn opencode_interactive_still_reports_effort_degradation() {
        let plan = interactive_model_plan(
            "opencode",
            "opencode-go/muse-spark-1.3-contributor",
            Some("max"),
        );
        assert_eq!(
            plan.env_pin(),
            Some("opencode-go/muse-spark-1.3-contributor")
        );
    }
}

/// bd-7wah: a REAL captured line, not a synthetic one.
///
/// Recorded 2026-09-29 from `opencode run --format json "reply with exactly: PONG"` on this
/// host, reproduced three times in a row. It is here because every opencode headless
/// dispatch was returning an empty response with `status: "success"`, and no test
/// covered the shape — 383 of 442 run records, 100% of the opencode ones.
///
/// The line is a top-level `type: "error"`. The old parser set `saw_json` on it, which
/// SUPPRESSED the raw-stdout fallback, then looked only for `type == "message_end"`,
/// found none, and returned an empty response with no error — because the error was
/// only ever read from INSIDE a `message_end`. A hardcoded `exit_code: 0` then recorded
/// the failure as a success. So a real 400 became indistinguishable from working.
#[cfg(test)]
const OPENCODE_ERROR_LINE: &str = r#"{"type":"error","sessionID":"ses_f16f636bdffeu7VnCKCO","timestamp":"2026-09-29T00:00:00Z","error":{"type":"provider.invalid-request","message":"This endpoint's maximum context length is 36864 tokens. However, you requested about 37930 tokens (34707 of text input, 3223 of tool input).","status":400}}"#;

#[cfg(test)]
mod bd_7wah_tests {
    use super::{HarnessOutcome, OPENCODE_ERROR_LINE, parse_stream};

    /// THE RED TEST. The captured opencode failure must be a FAILURE that names the
    /// provider's reason. Before the fix this returned success with an empty response,
    /// which is how 87% of every dispatch on this host came back empty and green.
    #[test]
    fn a_real_opencode_error_line_is_a_failure_that_names_the_reason() {
        let out: HarnessOutcome = parse_stream(OPENCODE_ERROR_LINE, "opencode");
        assert_eq!(
            out.status, "fail",
            "a top-level `type: error` must not be reported as success"
        );
        assert!(
            out.error.as_deref().unwrap_or_default().contains("36864"),
            "the provider's own reason must survive: {:?}",
            out.error
        );
        assert_eq!(
            out.response, "",
            "and the response stays empty rather than being invented"
        );
    }

    /// The companion: a stream that DOES carry an assistant message still parses, so
    /// the fix does not break the 13% that worked.
    #[test]
    fn a_well_formed_assistant_message_still_parses() {
        let line = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"PONG"}],"usage":{"input":11,"output":3},"model":"m"}}"#;
        let out = parse_stream(line, "opencode");
        assert_eq!(out.status, "success");
        assert_eq!(out.response, "PONG");
        assert_eq!(out.tokens_in, 11);
        assert_eq!(out.tokens_out, 3);
    }

    /// `saw_json` with nothing usable in it is UNRECOGNISED, not an empty success. This
    /// is the third reachable-but-silent state, and it is the one that made the 87%
    /// invisible rather than merely frequent.
    #[test]
    fn json_but_no_assistant_message_and_no_error_is_a_failure_not_an_empty_success() {
        let out = parse_stream(r#"{"type":"session_start","sessionID":"s"}"#, "opencode");
        assert_eq!(
            out.status, "fail",
            "a stream shape we do not recognise is a protocol disagreement, not a success"
        );
        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("unrecognised"),
            "and it says so rather than reporting nothing: {:?}",
            out.error
        );
    }

    /// The genuine fallback still works: a harness that answers in plain text with no
    /// JSON at all. This is the `pi`/`omp` path that reached 0% empty, and it must not
    /// regress.
    #[test]
    fn plain_text_output_is_still_the_response() {
        let out = parse_stream(
            "hello from a plain-text harness
",
            "pi",
        );
        assert_eq!(out.status, "success");
        assert_eq!(out.response, "hello from a plain-text harness");
    }
}
