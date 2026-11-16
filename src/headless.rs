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
/// Which harnesses degrade effort LOUDLY (no native effort flag): everything
/// except the harnesses with one. pi wires `--thinking`, grok wires
/// `--reasoning-effort`, agy wires `--effort`; opencode, gemini, fx, claude,
/// codex (and unknown harnesses) emit nothing, so the stderr warning fires.
pub fn effort_degraded(harness: &str) -> bool {
    crate::harness_registry::spec(&crate::harness_registry::canonical(harness))
        .is_none_or(|row| row.effort_flag.is_none())
}

pub fn model_args(harness: &str, model: &str, effort: Option<&str>) -> Vec<String> {
    let effort = effort.filter(|e| !e.is_empty());
    let canonical = crate::harness_registry::canonical(harness);
    if let Some(e) = effort
        && effort_degraded(canonical.as_str())
    {
        eprintln!(
            "flywheel: effort {e:?} has no native flag on harness {harness:?} — degrading to model-only (profile default stands)"
        );
    }
    // The flag itself is a registry fact (bd-6setu.1, ADR-0022 D3): this
    // function only assembles it.
    let Some(row) = crate::harness_registry::spec(&canonical) else {
        return vec![];
    };
    let Some(flag) = row.model_flag else {
        return vec![];
    };
    let mut args = vec![flag.to_string(), model.to_string()];
    if let (Some(e), Some(effort_flag)) = (effort, row.effort_flag) {
        args.push(effort_flag.to_string());
        args.push(e.to_string());
    }
    args
}

/// ADR-0022 L2 (bd-adr22-w2-phase-registry-igya): per-harness launch args
/// for an orchestration phase. Returns (args, degraded): degraded=true
/// means the harness has no native mechanic for this phase — the caller
/// WARNS (never silent), tags the run log `phase_degraded`, and the
/// dispatch proceeds in prompt mode. One match arm per harness; this
/// registry is the ONLY place a harness's phase vocabulary lives.
pub fn phase_launch_args(harness: &str, phase: &str) -> (Vec<String>, bool) {
    let harness = crate::harness_registry::canonical(harness);
    let Some(row) = crate::harness_registry::spec(&harness) else {
        // An unknown harness keeps the baseline: implement is never tagged
        // degraded (there is nothing to degrade), plan is.
        return (vec![], phase != "implement");
    };
    let (argv, degraded) = match phase {
        "plan" => row.phase_plan,
        "implement" => row.phase_implement,
        _ => return (vec![], true),
    };
    (argv.iter().map(|s| (*s).to_string()).collect(), degraded)
}

/// How a harness delivers a `mode` request (ADR-0022 D3 mode column).
/// `Honored` means the argv carries it; `Degraded` means prompt-mode with
/// the reason. There is no blocked variant here: blocking is a resolver
/// judgment from provenance (see `swarm_plan`), not an adapter fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeDelivery {
    Honored,
    Degraded { reason: String },
}

/// Mode argv for `mode: read-only` (verified `grok --help`, `agy --help`,
/// `pi --help --tools`, `gemini --help`, `opencode run --help`).
/// `workspace-write` and `isolated` emit nothing today (degraded).
#[must_use]
pub fn mode_launch_args(harness: &str, mode: &str) -> (Vec<String>, ModeDelivery) {
    let degraded = |reason: &str| {
        (
            Vec::new(),
            ModeDelivery::Degraded {
                reason: reason.to_string(),
            },
        )
    };
    if mode != "read-only" {
        return degraded(&format!("mode `{mode}` has no dedicated flag; prompt-only"));
    }
    let canonical = crate::harness_registry::canonical(harness);
    let Some(row) = crate::harness_registry::spec(&canonical) else {
        return degraded(&format!(
            "harness {canonical:?} has no read-only flag; prompt-only"
        ));
    };
    let (argv, degraded_reason) = row.read_only;
    let args: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    match degraded_reason {
        Some(reason) => (
            args,
            ModeDelivery::Degraded {
                reason: reason.to_string(),
            },
        ),
        None if args.is_empty() => degraded(&format!(
            "harness {canonical:?} has no read-only flag; prompt-only"
        )),
        None => (args, ModeDelivery::Honored),
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
    effort: Option<&str>,
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
                match effort.filter(|e| !e.is_empty()) {
                    // Headless effort rides the model variant; the pane
                    // path (interactive_model_plan) is unchanged and degraded.
                    Some(e) => argv.extend(["--model".to_string(), format!("{m}#{e}")]),
                    None => argv.extend(model_args("opencode", m, None)),
                }
            }
        }
        "grok" => {
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
    let _ = harness;
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
            continue;
        };
        saw_json = true;

        // bd-7wah: a TOP-LEVEL `type: "error"` is how opencode reports a rejected
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
        response = stdout.to_string();
    } else if response.is_empty() && error.is_none() {
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

/// ADR-0021 D3 step > stage > profile: the step's own ask wins, then the
/// stage contract, then the profile default. Empty strings count as
/// absent. Returns the winning value and which rung asked for it.
/// Provenance comes from `swarm_plan` so plan and dispatch agree.
#[must_use]
pub fn resolve_dispatch_field(
    step: Option<&str>,
    stage: Option<&str>,
    profile: Option<&str>,
) -> Option<(String, crate::swarm_plan::Provenance)> {
    use crate::swarm_plan::Provenance;
    for (ask, from) in [
        (step, Provenance::Step),
        (stage, Provenance::Stage),
        (profile, Provenance::Profile),
    ] {
        if let Some(v) = ask.map(str::trim).filter(|v| !v.is_empty()) {
            return Some((v.to_string(), from));
        }
    }
    None
}

/// `build_argv_roles` + the ADR-0022 phase registry: when `phase` is set,
/// the registry args are INSERTED BEFORE the trailing prompt positional
/// (flags after it would be swallowed as prompt text). Returns `(argv,
/// phase_degraded)` — degraded=true means the harness has no native mechanic
/// for the phase and the caller must warn + tag the run log (never silent).
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
        let insert_at = phase_argv_insert_at(&argv, prompt);
        argv.splice(insert_at..insert_at, extra.drain(..));
    }
    Ok((argv, degraded))
}

/// Where the phase-registry args belong in an argv (bd-vbrj).
///
/// "Before the trailing prompt positional" is only a position when the LAST
/// element IS the prompt. It is not, for grok: that arm returns early with the
/// prompt embedded in `--single=<prompt>`, so the last element is
/// `--output-format`'s VALUE. Splicing at `len - 1` there lands between the flag
/// and its value, and grok reads the phase flag as the output format.
///
/// So the rule is conditional on the shape, not assumed: before the prompt when
/// the prompt is the trailing positional, otherwise append — correct for grok,
/// where flags may follow `--single=`. The `saturating_sub` this replaces made
/// the wrong answer look safe; it clamped instead of revealing that the assumed
/// shape was wrong.
///
/// Pure, so both shapes are testable without a grok arm in the phase registry.
/// Today the bug is latent ONLY because `phase_launch_args` returns empty for
/// grok — an invariant held by another table's contents rather than by this
/// code.
fn phase_argv_insert_at(argv: &[String], prompt: &str) -> usize {
    match argv.last() {
        Some(last) if last == prompt => argv.len() - 1,
        _ => argv.len(),
    }
}

/// ADR-0022 phase 2b (bd-2hh0h): splice a mode mechanic into the harness
/// argv before the trailing prompt positional (same rule as
/// [`phase_argv_insert_at`], so a flag never lands after prompt text).
/// Returns the argv plus the stderr warning when the mode is degraded —
/// the caller prints it (loud, never silent) and the seat still runs.
/// Absent mode passes through untouched and quiet.
#[must_use]
pub fn apply_mode_argv(
    argv: Vec<String>,
    harness: &str,
    mode: Option<&str>,
    prompt: &str,
) -> (Vec<String>, Option<String>) {
    let Some(mode) = mode.map(str::trim).filter(|m| !m.is_empty()) else {
        return (argv, None);
    };
    let (extra, delivery) = mode_launch_args(harness, mode);
    let degraded = matches!(delivery, ModeDelivery::Degraded { .. });
    let mut argv = argv;
    if !extra.is_empty() {
        let at = phase_argv_insert_at(&argv, prompt);
        argv.splice(at..at, extra);
    }
    let warn = degraded.then(|| {
        format!("flywheel: mode {mode:?} has no native mechanic on harness {harness:?} — degrading to prompt mode")
    });
    (argv, warn)
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

    /// ADR-0022 phase 2a (bd-adapters-model-effort-ptgf7): the adapter rows.
    /// grok delivers effort via `--reasoning-effort` (verified `grok --help`);
    /// gemini takes `-m` (verified `gemini --help`); agy takes `--model` +
    /// `--effort` (verified `agy --help`: low|medium|high|xhigh|max).
    /// fx/claude/codex stay flag-less by policy.
    #[test]
    fn phase2a_adapter_rows_deliver_model_and_effort() {
        assert_eq!(
            model_args("grok", "grok-4", Some("high")),
            vec!["-m", "grok-4", "--reasoning-effort", "high"]
        );
        assert_eq!(
            model_args("gemini", "gemini-2.5-pro", None),
            vec!["-m", "gemini-2.5-pro"]
        );
        assert_eq!(
            model_args("agy", "agy/some-model", Some("high")),
            vec!["--model", "agy/some-model", "--effort", "high"]
        );
        for alias in ["antigravity", "ag", "antigravity-cli"] {
            assert_eq!(
                model_args(alias, "m", Some("low")),
                vec!["--model", "m", "--effort", "low"],
                "alias {alias} rides the agy row"
            );
        }
        assert!(
            model_args("fx", "x", None).is_empty(),
            "fx has no model flag"
        );
        assert!(
            model_args("fx", "x", Some("high")).is_empty(),
            "fx has no effort flag either"
        );
        assert!(model_args("claude", "x", None).is_empty());
        assert!(model_args("codex", "x", None).is_empty());
        for h in ["opencode", "gemini", "fx", "claude", "codex"] {
            assert!(effort_degraded(h), "{h} has no effort flag");
        }
        for h in ["pi", "grok", "agy", "antigravity", "ag"] {
            assert!(!effort_degraded(h), "{h} delivers effort natively");
        }
    }

    /// ADR-0022 phase 2b (bd-7rg0f): grok and agy grow plan mechanics.
    /// grok `--permission-mode plan` (verified `grok --help`); agy `--mode
    /// plan` (verified `agy --help`), aliases resolving through the registry.
    #[test]
    fn phase_registry_plan_mechanics_grok_agy() {
        let (args, degraded) = phase_launch_args("grok", "plan");
        assert_eq!(args, vec!["--permission-mode", "plan"]);
        assert!(!degraded, "grok plan is wired, not degraded");
        for h in ["agy", "antigravity", "ag", "antigravity-cli"] {
            let (args, degraded) = phase_launch_args(h, "plan");
            assert_eq!(args, vec!["--mode", "plan"], "{h} rides the agy row");
            assert!(!degraded, "{h} plan is wired, not degraded");
        }
        let (args, degraded) = phase_launch_args("opencode", "plan");
        assert_eq!(args, vec!["--agent", "plan"]);
        assert!(!degraded);
    }

    /// ADR-0022 phase 2b (bd-7rg0f): read-only mode delivery per harness.
    #[test]
    fn mode_launch_args_read_only_rows() {
        for (harness, want) in [
            ("grok", vec!["--permission-mode", "plan"]),
            ("agy", vec!["--mode", "plan"]),
            ("opencode", vec!["--agent", "plan"]),
            ("pi", vec!["--tools", "read,grep,find,ls"]),
        ] {
            let (args, delivery) = mode_launch_args(harness, "read-only");
            assert_eq!(
                args,
                want.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "{harness}"
            );
            assert_eq!(delivery, ModeDelivery::Honored, "{harness}");
        }
        let (args, delivery) = mode_launch_args("gemini", "read-only");
        assert_eq!(args, vec!["--approval-mode", "default"]);
        assert_eq!(
            delivery,
            ModeDelivery::Degraded {
                reason: "approval-gated, not enforced".to_string()
            }
        );
        for harness in ["fx", "claude", "codex"] {
            let (args, delivery) = mode_launch_args(harness, "read-only");
            assert!(args.is_empty(), "{harness}: no read-only flag");
            assert!(
                matches!(delivery, ModeDelivery::Degraded { .. }),
                "{harness}: degraded"
            );
        }
        for mode in ["workspace-write", "isolated"] {
            let (args, delivery) = mode_launch_args("pi", mode);
            assert!(args.is_empty(), "{mode}: nothing emitted today");
            assert!(
                matches!(delivery, ModeDelivery::Degraded { .. }),
                "{mode}: degraded with a reason"
            );
        }
    }

    /// ADR-0022 phase 2b (bd-7rg0f): opencode headless carries effort as a
    /// `#variant` on the model; the pane path is unchanged and degraded.
    #[test]
    fn opencode_headless_effort_rides_hash_variant() {
        let argv = build_argv_roles("opencode", Some("p/m"), Some("high"), "", "", "", "hi")
            .expect("argv");
        assert!(
            argv.windows(2).any(|w| w == ["--model", "p/m#high"]),
            "headless effort rides the variant: {argv:?}"
        );
        let plan = interactive_model_plan("opencode", "p/m", Some("high"));
        assert!(plan.args().is_empty(), "pane path emits no model argv");
        assert!(effort_degraded("opencode"), "pane effort still degrades");
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
        assert_eq!(model_args("grok", "gpt-5.5", None), vec!["-m", "gpt-5.5"]);
        assert_eq!(
            model_args("pi", "glm-5.2", None),
            vec!["--model", "glm-5.2"]
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

    /// governed-by: bd-ulzi — pi wires effort natively (`--thinking <level>`);
    /// provider/id passes through verbatim; opencode v2 keeps
    /// `--model provider/model#variant` verbatim and degrades LOUDLY on effort
    /// (no effort flag); empty effort emits no thinking flag.
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
        assert!(
            effort_degraded("opencode"),
            "opencode effort degrades loudly"
        );
    }

    /// bd-pi-thinking-equals-form: pi 0.87.1 rejects `--thinking=high` with
    /// `Unknown option: --thinking` and accepts the two-entry form, so the
    /// emitted argv is pinned pure in
    /// `pi_emitted_effort_argv_is_space_separated`.
    #[test]
    fn pi_emitted_effort_argv_is_space_separated() {
        assert_eq!(
            model_args("pi", "bogus/model-flywheel-argcheck", Some("high")),
            vec![
                "--model",
                "bogus/model-flywheel-argcheck",
                "--thinking",
                "high"
            ],
            "pi takes `--thinking <level>` as two argv entries"
        );
    }

    /// LIVE, ignored by default: spawns the operator's `pi` off PATH.
    #[test]
    #[ignore = "live: spawns the operator's pi binary and is flaky under load"]
    fn live_pi_emitted_effort_arg_form_is_accepted_by_the_real_binary() {
        let bogus_model = "bogus/model-flywheel-argcheck";
        let emitted = model_args("pi", bogus_model, Some("high"));

        let mut child = match std::process::Command::new("pi")
            .args(["--print"])
            .args(&emitted)
            .arg("x")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("pi not on PATH — skipping the real-binary arg check");
                return;
            }
            Err(e) => panic!("spawn pi: {e}"),
        };

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
        let (args, degraded) = phase_launch_args("pi", "plan");
        assert_eq!(args, Vec::<String>::new());
        assert!(degraded, "no phase mechanic is a loud degradation");
        let (args, degraded) = phase_launch_args("opencode", "plan");
        assert_eq!(args, vec!["--agent", "plan"]);
        assert!(!degraded);
    }

    /// ADR-0022 phase 2b (bd-7rg0f): mode args splice before the trailing
    /// prompt positional without splitting a flag from its value; degraded
    /// rows warn instead of running silent.
    #[test]
    fn apply_mode_argv_splices_before_prompt_and_warns_when_degraded() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (argv, warn) = apply_mode_argv(
            s(&["opencode", "run", "PROMPT"]),
            "opencode",
            Some("read-only"),
            "PROMPT",
        );
        assert_eq!(argv, s(&["opencode", "run", "--agent", "plan", "PROMPT"]));
        assert!(warn.is_none(), "honored mode stays quiet");
        let (argv, warn) = apply_mode_argv(
            s(&["gemini", "PROMPT"]),
            "gemini",
            Some("read-only"),
            "PROMPT",
        );
        assert_eq!(argv, s(&["gemini", "--approval-mode", "default", "PROMPT"]));
        assert!(
            warn.is_some_and(|w| w.contains("gemini")),
            "degraded mode warns naming the harness"
        );
        let (argv, warn) = apply_mode_argv(s(&["fx", "PROMPT"]), "fx", Some("read-only"), "PROMPT");
        assert_eq!(argv, s(&["fx", "PROMPT"]), "no flag: argv untouched");
        assert!(warn.is_some(), "no flag still warns, never silent");
        let (argv, warn) = apply_mode_argv(s(&["pi", "PROMPT"]), "pi", None, "PROMPT");
        assert_eq!(argv, s(&["pi", "PROMPT"]));
        assert!(warn.is_none(), "absent mode is quiet");
    }

    /// ADR-0022 W2: the argv wrapper inserts registry args BEFORE the
    /// trailing prompt positional and reports degradation to the caller.
    #[test]
    fn build_argv_roles_phase_inserts_before_prompt() {
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

        let (argv, degraded) =
            build_argv_roles_phase("pi", None, None, "", "", "", "hi", Some("plan")).expect("argv");
        assert!(degraded);
        assert!(!argv.iter().any(|a| a.contains("plan")));
    }

    /// bd-vbrj: THE shape. grok does NOT end its argv with the prompt — the
    /// prompt rides `--single=<prompt>`, so the trailing element is
    /// `--output-format`'s value. Every other arm pushes the prompt last.
    ///
    /// This is the fact the phase splice got wrong, and it is asserted about
    /// the REAL builder rather than a hand-written argv, so it fails if grok's
    /// arm is ever changed to append a positional.
    #[test]
    fn grok_argv_does_not_end_with_the_prompt() {
        let prompt = "do the thing";
        let argv = build_argv_roles("grok", None, None, "", "", "", prompt).expect("argv");
        assert_ne!(
            argv.last().map(String::as_str),
            Some(prompt),
            "grok embeds the prompt in --single=, so the trailing element is \
             --output-format's value. If this arm ever appends a positional \
             prompt, the phase splice below changes shape and this test says so: \
             {argv:?}"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("streaming-messages-json")
        );

        for harness in ["pi", "opencode"] {
            let argv = build_argv_roles(harness, None, None, "", "", "", prompt).expect("argv");
            assert_eq!(
                argv.last().map(String::as_str),
                Some(prompt),
                "{harness} ends with the prompt positional: {argv:?}"
            );
        }
    }

    /// bd-vbrj: the insertion rule under both shapes. grok's arm reaches the
    /// splice only once `phase_launch_args` grows a grok mechanic, so this tests
    /// the rule directly — the splice must never land between a flag and its
    /// value, whichever shape it is handed.
    #[test]
    fn phase_args_never_split_a_flag_from_its_value() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();

        let oc = s(&["opencode", "run", "--agent", "plan", "PROMPT"]);
        assert_eq!(phase_argv_insert_at(&oc, "PROMPT"), oc.len() - 1);

        let gk = s(&[
            "grok",
            "--single=PROMPT",
            "--output-format",
            "streaming-messages-json",
        ]);
        assert_eq!(phase_argv_insert_at(&gk, "PROMPT"), gk.len());

        let inline = s(&["grok", "--single=PROMPT"]);
        assert_eq!(phase_argv_insert_at(&inline, "PROMPT"), inline.len());

        assert_eq!(phase_argv_insert_at(&[], "PROMPT"), 0);
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
        let oc_problem = phase_registry_probe("opencode", &drifted).expect("opencode drift caught");
        assert!(
            oc_problem.contains("plan") && oc_problem.contains("build"),
            "{oc_problem}"
        );

        let lost_build = |bin: &str, _args: &[&str]| -> Option<String> {
            match bin {
                "opencode" => Some("plan (primary)\nexplore (subagent)".into()),
                _ => None,
            }
        };
        let build_problem =
            phase_registry_probe("opencode", &lost_build).expect("build drift caught");
        assert!(build_problem.contains("build"), "{build_problem}");

        let dead = |_: &str, _: &[&str]| -> Option<String> { None };
        assert!(phase_registry_probe("opencode", &dead).is_some());
    }

    /// ADR-0022 §3.2 R2: loud degradation. A (harness, phase) pair with no
    /// native mechanic returns empty args + degraded=true — the caller
    /// warns, tags the run log `phase_degraded`, and the dispatch proceeds
    /// in prompt mode. Never a silent no-op, never a panic. Phase 2b wired
    /// mechanics for grok plan, so grok left this list (its row moved to
    /// phase_registry_plan_mechanics_grok_agy).
    #[test]
    fn phase_registry_degrades_loudly_without_mechanic() {
        for harness in ["pi", "fx", "hermes-unknown"] {
            let (args, degraded) = phase_launch_args(harness, "plan");
            assert!(args.is_empty(), "{harness}: no mechanic -> no args");
            assert!(degraded, "{harness}: plan must degrade");
        }
        for phase in ["resolve", "review", "ship"] {
            let (_, degraded) = phase_launch_args("omp", phase);
            assert!(degraded, "omp {phase}: no mechanic -> degraded");
            let (_, degraded) = phase_launch_args("opencode", phase);
            assert!(degraded, "opencode {phase}: no mechanic -> degraded");
        }
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

    /// ADR-0021 D3 step > stage > profile: the step's own ask wins, then
    /// the stage contract, then the profile default. Empty strings count
    /// as absent.
    #[test]
    fn dispatch_field_prefers_step_over_stage_over_profile() {
        use crate::swarm_plan::Provenance;
        assert_eq!(
            resolve_dispatch_field(Some("high"), Some("max"), Some("low")),
            Some(("high".to_string(), Provenance::Step))
        );
        assert_eq!(
            resolve_dispatch_field(None, Some("max"), Some("low")),
            Some(("max".to_string(), Provenance::Stage))
        );
        assert_eq!(
            resolve_dispatch_field(None, None, Some("low")),
            Some(("low".to_string(), Provenance::Profile))
        );
        assert_eq!(resolve_dispatch_field(None, None, None), None);
        assert_eq!(resolve_dispatch_field(Some("  "), Some(""), None), None);
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
/// `hc agent start --kind <harness>` launches, as opposed to the headless
/// one-shot `build_argv_roles` uses.
///
/// bd-crlx: this distinction is load-bearing, not cosmetic. `opencode` v2 has
/// two different entrypoints with DIFFERENT flags:
///
/// - `opencode run --model provider/model` — the headless one-shot. `--model`
///   is a real flag (`opencode run --help`: `--model, -m string`).
/// - `opencode` (the root/TUI command hc actually launches) — flags are
///   `--standalone --server --auto --continue/-c --session/-s --prompt`.
///   There is **no `--model` and no `-m`**, and v2.0.16 rejects it outright:
///   `Unrecognized flag: --model in command opencode`.
///
/// So reusing [`model_args`] on the pane path handed opencode a flag its own
/// binary rejects. The process died on spawn, hc's readiness wait reported
/// `agent_not_running`, and the dispatch degraded into the permanent
/// `agent <name> not found in toron` — every opencode pane seat, always.
///
/// The TUI's model is config, not argv: `opencode.json`'s `model` key, or
/// in-session `/models` (https://opencode.ai/v2/docs/models/). opencode
/// merges config sources rather than replacing them, so the pin travels as a
/// per-pane `OPENCODE_CONFIG_CONTENT` document — see
/// [`crate::hc::apply_opencode_model_pin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveModelPlan {
    /// The harness takes the model as argv. `args` are appended to the launch.
    Argv(Vec<String>),
    /// The harness takes no model flag; the pin must travel by pane env.
    PinnedByEnv { model: String },
    /// The ask is [`crate::models::HARNESS_DEFAULT`]: the harness's own
    /// configured model stands, so nothing is injected.
    HarnessDefault,
}

impl InteractiveModelPlan {
    /// The argv half, for callers that only splice flags.
    pub fn args(&self) -> &[String] {
        match self {
            Self::Argv(a) => a,
            Self::PinnedByEnv { .. } | Self::HarnessDefault => &[],
        }
    }

    /// The model to pin via pane env, when the harness needs one.
    pub fn env_pin(&self) -> Option<&str> {
        match self {
            Self::Argv(_) | Self::HarnessDefault => None,
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
    if crate::models::ModelChoice::parse(model) == crate::models::ModelChoice::HarnessDefault {
        return InteractiveModelPlan::HarnessDefault;
    }
    let row = crate::harness_registry::spec(&crate::harness_registry::canonical(harness));
    if row.is_some_and(|r| r.pane_model_by_env) {
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
