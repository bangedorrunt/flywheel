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
        _ => vec![],
    }
}

/// Build the argv for one harness (pure — the P10.1 unit surface).
pub fn build_argv(
    harness: &str,
    model: Option<&str>,
    effort: Option<&str>,
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
                ["grok", &single, "--output-format", "streaming-messages-json"]
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
pub fn dispatch_spec_profile(spec: &Value) -> Option<&str> {
    spec.get("profile").and_then(Value::as_str)
}

/// The stage name in a dispatch spec (`spec.stage`, e.g. `systematic-execute`).
pub fn dispatch_spec_stage(spec: &Value) -> Option<&str> {
    spec.get("stage").and_then(Value::as_str)
}

/// The per-step harness override in a dispatch spec (`spec.harness`, e.g.
/// `opencode` — bd-42fs step 5). When present, the runner uses this harness
/// INSTEAD of the profile's default (review/debate workflows pin the
/// reviewer to opencode).
pub fn dispatch_spec_harness(spec: &Value) -> Option<&str> {
    spec.get("harness").and_then(Value::as_str)
}

/// The task text in a dispatch spec (`spec.task`): an inline instruction that
/// replaces the workflow-default base prompt (bd-m7ac weekly-review — scouts
/// and the oracle carry their mission inline). `None` when absent (the
/// stage/base-prompt fallback applies).
pub fn dispatch_spec_task(spec: &Value) -> Option<&str> {
    spec.get("task").and_then(Value::as_str)
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
pub fn spec_extra(spec: &Value, key: &str, inputs: &Value) -> Option<String> {
    let raw = spec.get(key).and_then(Value::as_str)?.trim();
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
            build_argv("pi", Some("google/gemini-2.5-flash"), None, "hello").unwrap(),
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
            build_argv("pi", None, None, "hello").unwrap(),
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
            build_argv("omp", Some("gpt-5.2"), None, "hi").unwrap(),
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
            build_argv("opencode", Some("anthropic/claude-sonnet-4"), None, "hi").unwrap(),
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
            build_argv("grok", Some("grok-4"), None, "hi").unwrap(),
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
        let err = build_argv("hermes", None, None, "hi").unwrap_err();
        assert!(err.to_string().contains("hermes"), "{err}");
    }

    /// P10.3 (bd-ghcj.1): the model-flag args are per-harness, shared with
    /// the pane-runner spawn path (opencode/grok `-m`, omp `--model=`, pi
    /// `--model`). bd-42fs: omp carries `--thinking=<effort>` when given.
    #[test]
    fn model_args_follow_harness_forms() {
        assert_eq!(model_args("opencode", "claude-sonnet-4", None), vec!["-m", "claude-sonnet-4"]);
        assert_eq!(model_args("grok", "grok-4", None), vec!["-m", "grok-4"]);
        assert_eq!(model_args("omp", "gpt-5.2", None), vec!["--model=gpt-5.2"]);
        assert_eq!(
            model_args("omp", "glm-5.2", Some("max")),
            vec!["--model=glm-5.2", "--thinking=max"]
        );
        assert_eq!(
            model_args("omp", "deepseek-v4-flash", Some("high")),
            vec!["--model=deepseek-v4-flash", "--thinking=high"]
        );
        assert_eq!(model_args("pi", "glm-5.2", None), vec!["--model", "glm-5.2"]);
        assert!(model_args("hermes", "x", None).is_empty(), "unknown harness: no model args");
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
        let with_task = json!({ "profile": "@scout", "task": "Map toron gaps." });
        assert_eq!(dispatch_spec_task(&with_task), Some("Map toron gaps."));
        assert_eq!(dispatch_spec_profile(&with_task), Some("@scout"));
        let no_task = json!({ "profile": "@planner" });
        assert_eq!(dispatch_spec_task(&no_task), None);
        assert_eq!(dispatch_spec_task(&json!({})), None);
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
        let spec = json!({ "profile": "@planner", "stage": "systematic-execute" });
        assert_eq!(dispatch_spec_profile(&spec), Some("@planner"));
        assert_eq!(dispatch_spec_stage(&spec), Some("systematic-execute"));
        let bare = json!({ "profile": "@coder" });
        assert_eq!(dispatch_spec_profile(&bare), Some("@coder"));
        assert_eq!(dispatch_spec_stage(&bare), None);
    }

    /// bd-42fs step 5: the per-step harness override rides in the spec.
    #[test]
    fn dispatch_spec_extracts_harness_override() {
        let spec = json!({ "profile": "@reviewer", "harness": "opencode" });
        assert_eq!(dispatch_spec_harness(&spec), Some("opencode"));
        let bare = json!({ "profile": "@reviewer" });
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
        let spec = json!({ "profile": "@planner", "plan": "{{inputs.plan}}" });
        let inputs = json!({ "plan": "1. fix auth 2. ship" });
        assert_eq!(
            spec_extra(&spec, "plan", &inputs).as_deref(),
            Some("1. fix auth 2. ship")
        );
    }

    /// An unresolvable ref stays literal — the text is never dropped.
    #[test]
    fn spec_extra_keeps_unresolved_ref_literal() {
        let spec = json!({ "plan": "Work from {{inputs.missing}}" });
        assert_eq!(
            spec_extra(&spec, "plan", &json!({})).as_deref(),
            Some("Work from {{inputs.missing}}")
        );
    }

    /// Missing or empty extra → None (nothing appended to the prompt).
    #[test]
    fn spec_extra_none_when_absent_or_empty() {
        assert_eq!(spec_extra(&json!({}), "plan", &json!({})), None);
        assert_eq!(spec_extra(&json!({ "plan": "  " }), "plan", &json!({})), None);
    }
}
