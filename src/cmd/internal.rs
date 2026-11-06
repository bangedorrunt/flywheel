//! `flywheel internal …` leaves (split from `main.rs`, bd-wave5-main-eviction-d5hht).

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

use crate::config::Config;
use crate::hc::{
    AgentInfo, AgentStatus, Client, SendVerdict, Subscription, alt_screen_read_ladder,
};
use crate::output::print_json;
use crate::{
    agent_target, comms_send, durable, find_workspace, find_workspace_dyn, headless,
    load_inprogress_beads, load_ready_beads, profile, profile_launch_args, project_dir,
    project_dir_str, rebind_from_registry, resolve_agents_dyn, run_log, start_agent_with_retry_dyn,
    state, terminal_mail_name, utf8_path,
};
use crate::{backend, hc};
/// internal leaf: JSON batch of push events collected within `timeout_ms`.
///
/// governed-by: ADR-0001 (A2 event stream) + ADR-0002 (backend factory).
///
/// **Forward-only:** hc `events.subscribe` has no since/seq param (schema-
/// verified). This leaf opens a fresh subscription, blocks up to `timeout_ms`,
/// and returns whatever arrived. Crash recovery is NOT via event replay —
/// the loop's `__poll` snapshot is the correctness floor. `after_seq` is
/// best-effort in-run dedup only (skips events whose local seq is <= it).
///
/// Wire shape:
/// ```json
/// { "events": [{ "event_type", "pane_id", "agent", "status", "seq" }, ...],
///   "max_seq": 0, "stream_error": null }
/// ```
pub(crate) fn cmd_internal_poll_events(
    cfg: &Config,
    project: &str,
    timeout_ms: u64,
    after_seq: Option<u64>,
) -> Result<()> {
    let batch = collect_poll_events(cfg, project, timeout_ms, after_seq)?;
    print_json(&batch);
    Ok(())
}

/// Collect status-change events for `project` until `timeout_ms` elapses.
/// Extracted for unit-testability of the empty/error paths via the JSON shape.
fn collect_poll_events(
    cfg: &Config,
    project: &str,
    timeout_ms: u64,
    after_seq: Option<u64>,
) -> Result<serde_json::Value> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = match find_workspace_dyn(&*backend, project) {
        Ok(id) => id,
        Err(e) => {
            return Ok(json!({
                "events": [],
                "max_seq": after_seq.unwrap_or(0),
                "stream_error": format!("no workspace: {e}"),
            }));
        }
    };

    let panes = backend.pane_list(Some(&ws_id)).unwrap_or_default();
    if panes.is_empty() {
        return Ok(json!({
            "events": [],
            "max_seq": after_seq.unwrap_or(0),
            "stream_error": null,
        }));
    }

    let subs: Vec<Subscription> = panes
        .iter()
        .map(|p| Subscription::agent_status_changed(p.pane_id.clone()))
        .collect();

    let mut stream = match backend.subscribe(&subs) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "events": [],
                "max_seq": after_seq.unwrap_or(0),
                "stream_error": format!("subscribe failed: {e}"),
            }));
        }
    };

    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut seq = after_seq.unwrap_or(0);
    let mut stream_error: Option<String> = None;

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let read_to = remaining.max(Duration::from_millis(1));
        if let Err(e) = stream.set_read_timeout(Some(read_to)) {
            stream_error = Some(format!("set_read_timeout: {e}"));
            break;
        }
        match stream.read_event() {
            Ok(Some(ev)) => {
                if let Some(item) = summarize_status_event(&ev, &mut seq) {
                    if after_seq.is_some_and(|a| item.seq <= a) {
                        continue;
                    }
                    events.push(json!({
                        "event_type": item.event_type,
                        "pane_id": item.pane_id,
                        "agent": item.agent,
                        "status": item.status,
                        "seq": item.seq,
                    }));
                }
            }
            Ok(None) => {
                stream_error = Some("stream closed".into());
                break;
            }
            Err(_) => {
                break;
            }
        }
    }

    Ok(json!({
        "events": events,
        "max_seq": seq,
        "stream_error": stream_error,
    }))
}

/// Compact form of a status-change push for the `__poll_events` JSON batch.
struct StatusEventItem {
    event_type: &'static str,
    pane_id: String,
    agent: Option<String>,
    status: String,
    seq: u64,
}

/// Map a typed backend push event to a status item, advancing `seq`. Returns
/// `None` for non-status events (defensive — the subscription is already
/// narrowed to agent-status pushes). Consumes the UNIFIED toron-core
/// `BackendEvent` (ADR-0005 P1); the wire event_type strings are preserved so
/// downstream consumers see the same names as before the trait unification.
fn summarize_status_event(
    ev: &toron_core::backend::BackendEvent,
    seq: &mut u64,
) -> Option<StatusEventItem> {
    use toron_core::backend::BackendEvent::{
        AgentDetected, AgentStatusChanged, EventsLost, PaneExited, Raw,
    };
    let (event_type, pane_id, agent, status) = match ev {
        AgentStatusChanged {
            pane,
            agent,
            status,
        } => (
            "pane.agent_status_changed",
            pane.as_str().to_string(),
            agent.clone(),
            status.as_str().to_string(),
        ),
        AgentDetected {
            pane,
            agent,
            status,
        } => (
            "pane.agent_detected",
            pane.as_str().to_string(),
            agent.clone(),
            status.as_str().to_string(),
        ),
        PaneExited { pane } => (
            "pane.exited",
            pane.as_str().to_string(),
            None,
            "exited".to_string(),
        ),
        EventsLost => return None,
        Raw(_) => return None,
    };
    *seq = seq.saturating_add(1);
    Some(StatusEventItem {
        event_type,
        pane_id,
        agent,
        status,
        seq: *seq,
    })
}

/// internal leaf: JSON [[name, status], ...] for a project's managed agents.
///
/// governed-by: ADR-0002 — constructs the multiplexer via the factory so the
/// durable loop is backend-agnostic (hc today; superlogical/toron later).
pub(crate) fn cmd_internal_poll(cfg: &Config, project: &str, to: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agents = backend
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    let registry = state::load_spawn_registry(project);
    let pin_for = |name: &str| -> Option<&str> {
        registry
            .iter()
            .find(|e| e.name == name)
            .and_then(|e| e.mail_identity.as_deref())
    };
    let managed: Vec<Value> = if to == "all" {
        agents
            .iter()
            .map(|a| {
                let name = agent_target(a);
                json!([
                    name,
                    a.agent_status.as_str(),
                    a.agent.clone().unwrap_or_default(),
                    a.pane_id,
                    pin_for(name),
                ])
            })
            .collect()
    } else {
        let resolved = resolve_agents_dyn(&*backend, &ws_id, project, to)?;
        resolved
            .iter()
            .map(|a| {
                let name = agent_target(a);
                json!([
                    name,
                    a.agent_status.as_str(),
                    a.agent.clone().unwrap_or_default(),
                    a.pane_id,
                    pin_for(name),
                ])
            })
            .collect()
    };
    print_json(&json!({ "agents": managed }));
    Ok(())
}

/// Dispatch options for the `__send` internal leaf (`cmd_internal_send`):
/// confirm-wait, resume-dedup invocation/iteration, and profile conditioning.
/// Bundled to keep the leaf's signature under clippy's argument cap; the
/// core `cfg`/`project`/`name`/`text` params stay positional.
pub(crate) struct SendOptions<'a> {
    pub(crate) confirm_ms: Option<u64>,
    pub(crate) invocation: Option<&'a str>,
    pub(crate) iteration: Option<u64>,
    pub(crate) profile: Option<&'a str>,
}

/// bd-eign.10: build the skill-load prefix for a profiled agent's FIRST
/// dispatch — a single line listing the profile's skills via the skill://
/// scheme (the harness resolves them). `None` when the agent already got it,
/// has no profile, or the profile lists no skills.
pub(crate) fn skill_injection_prefix(
    entry: &state::SpawnEntry,
    skills: &[String],
) -> Option<String> {
    if entry.profile.is_none() || entry.skills_injected || skills.is_empty() {
        return None;
    }
    let paths = skills
        .iter()
        .map(|s| format!("skill://{s}"))
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("Read {paths} before starting.\n\n"))
}

/// bd-83as/bd-mcax: first-dispatch predicate for the readiness gate —
/// true while the spawn registry shows skills not yet injected, i.e. this
/// pane booted fresh under the current spawn. Landed history is NOT
/// consulted: terminal names are stable across respawns, so a landed record
/// would silence the gate forever after the first send. Later dispatches
/// (injected lane) skip the gate entirely.
fn is_first_dispatch(skills_injected: bool) -> bool {
    !skills_injected
}

/// bd-mcax: gate decision — a first dispatch gates when the agent row is
/// missing (fresh pane still binding) or still booting. Anything else
/// proceeds straight to the normal send path.
fn should_gate_first_dispatch(first: bool, agent: Option<&AgentInfo>) -> bool {
    first && agent.is_none_or(crate::hc::agent_still_booting)
}

/// bd-83as: have this agent's skills been injected yet (spawn registry)?
/// Absent registry or entry reads as not-injected (fail toward gating —
/// the gate is a no-op wait on already-booted panes).
pub(crate) fn skills_injected_for(project: &str, name: &str) -> bool {
    state::load_spawn_registry(project)
        .iter()
        .filter(|e| e.name == name)
        .max_by_key(|e| e.incarnation)
        .is_some_and(|e| e.skills_injected)
}

/// bd-83as: bounded readiness wait over an injected lister (scriptable —
/// unit tests feed scripted boot sequences, production passes the live
/// backend list). Returns the fresh row once the pane stops booting; past
/// the deadline it fails typed, never hangs, never a bare ok:false downstream.
fn await_agent_ready<F>(
    mut list_agents: F,
    name: &str,
    ws_id: &str,
    deadline: Duration,
) -> anyhow::Result<AgentInfo>
where
    F: FnMut() -> anyhow::Result<Vec<AgentInfo>>,
{
    const GATE_BOUND: Duration = Duration::from_secs(10);
    let bound = deadline.min(GATE_BOUND).max(Duration::from_millis(1));
    let start = std::time::Instant::now();
    loop {
        let agents = list_agents()?;
        if let Some(a) = agents
            .iter()
            .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
            && !crate::hc::agent_still_booting(a)
        {
            return Ok(a.clone());
        }
        if start.elapsed() >= bound {
            anyhow::bail!(
                "agent {name} still booting or missing after {}s — first dispatch held at the readiness gate (bd-83as/bd-mcax)",
                bound.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// internal leaf: send one prompt to a named agent.
///
/// **ADR-0002 note:** this leaf is NOT yet migrated to the trait factory. The
/// confirm-path uses `Client::agent_prompt_coded` (typed `PlaneError` code →
/// `classify_send_result` landing semantics), which the trait surface doesn't
/// expose (the trait's `agent_prompt` returns a minimal JSON, losing the coded
/// error). Migrating requires either a trait-level coded-prompt method or a
/// different landing-detection mechanism — a design question, deferred to a
/// follow-up bead. The `__poll` and `__mail-blocked` leaves ARE migrated
/// (they map cleanly onto trait methods).
///
/// Without `--confirm`: fire-and-forget (`wait: None`) — byte-identical to
/// the legacy path. With `--confirm [TIMEOUT_MS]`: wait for the prompt to
/// LAND (agent reaches working|blocked), classify the hc error code, and
/// emit `{"ok": true, "name", "landed", "status", "reason"}`. Exit 1 only
/// for hard errors (agent not found); landing failures exit 0 with flags.
pub(crate) fn cmd_internal_send(
    cfg: &Config,
    project: &str,
    name: &str,
    text: &str,
    opts: SendOptions<'_>,
) -> Result<()> {
    let SendOptions {
        confirm_ms,
        invocation,
        iteration,
        profile,
    } = opts;
    if comms_send::enabled(cfg) {
        let task_id = match invocation.filter(|i| !i.is_empty()) {
            Some(inv) => comms_send::loop_task_id(inv, iteration.unwrap_or(0), name, text),
            None => mint_task_id(),
        };
        let out = comms_send::loop_send(cfg, project, name, text, &task_id)?;
        println!("{out}");
        return Ok(());
    }
    let profile_name = profile.filter(|p| !p.is_empty());
    let profile_obj = match profile_name {
        Some(p) => {
            let catalog = profile::ProfileCatalog::load()?;
            if let Some(pr) = catalog.get(p) {
                Some((
                    p.to_string(),
                    pr.harness.clone(),
                    pr.model.clone(),
                    pr.behavior.clone(),
                    pr.skills.clone(),
                ))
            } else {
                eprintln!(
                    "flywheel: __send --profile {p} unknown (flywheel profile list) — sending raw"
                );
                None
            }
        }
        None => None,
    };
    let started = std::time::Instant::now();

    if let Some(inv) = invocation.filter(|i| !i.is_empty()) {
        let hash = state::fnv1a64(text);
        let records = state::load_last_sent(project);
        let record = state::find_last_sent(&records, name, hash);
        if !durable::should_resend(record, inv) {
            print_json(&json!({
                "ok": true, "name": name, "landed": true,
                "status": serde_json::Value::Null, "reason": "landed", "deduped": true,
            }));
            return Ok(());
        }
    }

    let key = project_dir_str(cfg, project)?;
    let mut send_base = text.to_string();
    if let Some((_, _, _, _, skills)) = &profile_obj
        && !skills.is_empty()
    {
        let mut entries = state::load_spawn_registry(&key);
        let injected = entries
            .iter_mut()
            .find(|e| e.name == name)
            .map(|entry| {
                if let Some(prefix) = skill_injection_prefix(entry, skills) {
                    send_base = format!("{prefix}{send_base}");
                    entry.skills_injected = true;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if injected && let Err(e) = state::save_spawn_registry(project, &entries) {
            eprintln!("flywheel: cannot persist skills_injected for {name}: {e:#}");
        }
    }
    let mut send_text = match &profile_obj {
        Some((_, _, _, behavior, _)) => headless::conditioned_prompt(behavior, &send_base),
        None => send_base.clone(),
    };

    if text.starts_with("GOAL ") && send_text.len() > durable::DISPATCH_PROMPT_CAP {
        let bead = durable::bead_id_from_prompt(text).map(str::to_string);
        let rebuilt = bead.as_deref().map(|id| {
            let prefix = send_base.strip_suffix(text).unwrap_or_default();
            let pointer = durable::pointer_brief(id, project, text.starts_with("GOAL Resume "));
            let base = format!("{prefix}{pointer}");
            match &profile_obj {
                Some((_, _, _, behavior, _)) => headless::conditioned_prompt(behavior, &base),
                None => base,
            }
        });
        match rebuilt {
            Some(degraded) if degraded.len() <= durable::DISPATCH_PROMPT_CAP => {
                eprintln!(
                    "flywheel: WARN dispatched prompt for bead {} is {} bytes — over the {} byte cap; degraded to the pointer brief ({} bytes)",
                    bead.as_deref().unwrap_or("<unknown>"),
                    send_text.len(),
                    durable::DISPATCH_PROMPT_CAP,
                    degraded.len(),
                );
                send_text = degraded;
            }
            _ => bail!(
                "flywheel: dispatched prompt for bead {} is {} bytes — over the {} byte cap even as the pointer brief (loop bug; NOT sent, never truncated)",
                bead.as_deref().unwrap_or("<unknown>"),
                send_text.len(),
                durable::DISPATCH_PROMPT_CAP,
            ),
        }
    }

    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let mut agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    if agent.is_none() && rebind_from_registry(&*backend, project, name, &ws_id)? {
        agent = backend
            .agent_list()?
            .into_iter()
            .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    }
    let first = is_first_dispatch(skills_injected_for(project, name));
    let agent = if should_gate_first_dispatch(first, agent.as_ref()) {
        await_agent_ready(
            || backend.agent_list().map_err(anyhow::Error::from),
            name,
            &ws_id,
            Duration::from_secs(10),
        )?
    } else {
        agent.ok_or_else(|| anyhow!("agent {name} not found in {project}"))?
    };
    let target = agent_target(&agent).to_string();

    let record = |landed: bool, ambiguous: bool| {
        let rec = state::SentRecord {
            name: name.to_string(),
            text_hash: state::fnv1a64(text),
            invocation: invocation.unwrap_or_default().to_string(),
            iteration: iteration.unwrap_or(0),
            landed,
            ambiguous,
        };
        if let Err(e) = state::record_sent(project, rec) {
            eprintln!("flywheel: cannot record sent prompt: {e:#}");
        }
    };

    let Some(timeout_ms) = confirm_ms else {
        match backend.prompt(&target, &send_text, None) {
            Ok(_) => {}
            Err(e) if is_agent_not_ready(&e.code) => {
                if let Some(pane) = not_ready_retry_target(&target, &agent, false) {
                    eprintln!("flywheel: {name} agent_not_ready — retrying via pane id {pane}");
                    backend.prompt(&pane, &send_text, None)?;
                } else {
                    return Err(e.into());
                }
            }
            Err(e) => return Err(e.into()),
        }
        record(true, false);
        if let Some((pname, _, _, _, _)) = &profile_obj {
            log_engine_dispatch(project, pname, text, &send_text, started, true);
        }
        print_json(&json!({ "ok": true, "name": name }));
        return Ok(());
    };

    let wait = WaitOptions {
        timeout_ms: Some(timeout_ms),
        until: vec![AgentStatus::Working, AgentStatus::Blocked],
    };
    let landing = backend.prompt_coded(&target, &send_text, Some(&wait))?;
    let landing = if landing.code.as_deref() == Some("agent_not_ready")
        && let Some(pane) = not_ready_retry_target(&target, &agent, false)
    {
        eprintln!("flywheel: {name} agent_not_ready — retrying via pane id {pane}");
        backend.prompt_coded(&pane, &send_text, Some(&wait))?
    } else {
        landing
    };
    let verdict = classify_send_result(landing.code.as_deref(), landing.status);
    let body = confirm_send_json(name, verdict.clone());
    let landed = matches!(verdict, SendVerdict::Landed(_));
    record(landed, matches!(verdict, SendVerdict::TimedOut));
    if let Some((pname, _, _, _, _)) = &profile_obj {
        log_engine_dispatch(project, pname, text, &send_text, started, landed);
    }
    print_json(&body);
    Ok(())
}

/// bd-n9hy: one-line failure record for a __send subprocess dispatch — keeps
/// the raw stdout+stderr bytes plus exit status so a bare `{ok:false}` on a
/// fresh pane stays diagnosable. Log-only shape, never parsed.
pub(crate) fn format_send_failure(
    name: &str,
    stdout: &str,
    stderr: &str,
    status_ok: bool,
    code: i32,
) -> String {
    let out = if stdout.is_empty() { "<empty>" } else { stdout };
    let err = if stderr.is_empty() { "<empty>" } else { stderr };
    format!(
        "flywheel: send to {name} failed: status={code} ok={status_ok} stdout_len={} stderr_len={} stdout={out:?} stderr={err:?}",
        stdout.len(),
        stderr.len(),
    )
}

/// The confirmed-send leaf JSON for ONE classified landing. Honesty contract
/// (bd-adr0006-cosmetic-sweep-q0g0): a timeout is AMBIGUOUS — the prompt may
/// have started — so it emits `landed: null` + `ambiguous: true`, never a
/// definitive false that green completions would carry as a miss. Stalled and
/// coded errors ARE definitive non-landings and keep `landed: false`.
fn confirm_send_json(name: &str, verdict: SendVerdict) -> Value {
    match verdict {
        SendVerdict::Landed(st) => json!({
            "ok": true, "name": name, "landed": true,
            "status": st.map(|s| s.as_str().to_string()),
            "reason": "landed",
        }),
        SendVerdict::Stalled => json!({
            "ok": true, "name": name, "landed": false,
            "status": Value::Null, "reason": "stalled",
        }),
        SendVerdict::TimedOut => json!({
            "ok": true, "name": name, "landed": Value::Null,
            "ambiguous": true, "status": Value::Null, "reason": "timeout",
        }),
        SendVerdict::Coded(code) => json!({
            "ok": true, "name": name, "landed": false,
            "status": Value::Null, "reason": format!("error:{code}"),
        }),
    }
}

/// bd-gsvn.1: write the runs/ entry for a profile-conditioned loop/pane
/// dispatch (mode: pane — the engine's pane-runner logging gain). The
/// entry's `prompt_hash` covers the CONDITIONED text (what the agent
/// actually received); `model`/`harness` come from the profile.
fn log_engine_dispatch(
    project: &str,
    profile_name: &str,
    raw_text: &str,
    sent_text: &str,
    started: std::time::Instant,
    landed: bool,
) {
    use sha1::Digest;
    let _ = project;
    let mut hasher = sha1::Sha1::new();
    sha1::Digest::update(&mut hasher, sent_text.as_bytes());
    let prompt_hash = hex::encode(sha1::Digest::finalize(hasher));
    let (harness, model) = profile::ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.get(profile_name).cloned())
        .map(|p| (p.harness, p.model))
        .unwrap_or_default();
    let entry = run_log::RunLogEntry {
        run_id: run_log::new_run_id(),
        ts: state::now_iso(),
        workflow: String::new(),
        cron: None,
        profile: profile_name.to_string(),
        harness,
        model,
        mode: "pane".into(),
        prompt_hash,
        response: String::new(),
        status: if landed {
            "success".into()
        } else {
            "fail".into()
        },
        tokens_in: 0,
        tokens_out: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        exit_code: i32::from(!landed),
        error: None,
        phase: None,
        phase_degraded: false,
    };
    if let Err(e) = run_log::write_run(&entry) {
        eprintln!("flywheel: cannot write run log for @{profile_name}: {e:#}");
    }
    let _ = raw_text;
}

/// internal leaf: ready beads as [{id, title}] via `br ready --json`.
/// Resolve a `__wait-output` target to a pane id: an agent NAME (matched in
/// the caller-provided, workspace-filtered agent list) maps to its pane; a
/// target containing `:` is treated as a raw pane id; anything else is
/// unknown. Pure — no lookup logic duplicated from `__send`.
pub(crate) fn resolve_pane_id(agents: &[hc::AgentInfo], target: &str) -> Option<String> {
    if let Some(a) = agents.iter().find(|a| a.name.as_deref() == Some(target)) {
        return Some(a.pane_id.clone());
    }
    if target.contains(':') && !target.is_empty() {
        return Some(target.to_string());
    }
    None
}

/// The wait-output mode for the `--regex` flag: hc's literal-substring
/// mode is spelled `match` in the leaf (mapped to `substring` in the client).
pub(crate) fn wait_output_mode(regex: bool) -> &'static str {
    if regex { "regex" } else { "match" }
}

/// The leaf's machine-readable output shape.
pub(crate) fn wait_output_json(matched: bool, matched_line: Option<&str>, pane_id: &str) -> Value {
    json!({
        "matched": matched,
        "matched_line": matched_line,
        "pane_id": pane_id,
    })
}

/// internal leaf: block until a pane's output matches a pattern.
///
/// Target is an agent name (resolved via the project's agent list) or a raw
/// pane id. Exit 0 on match, 1 on timeout / unknown target — stdout carries
/// `{"matched", "matched_line", "pane_id"}` either way (stderr for messages).
/// For automation/scripts; the durable workflow does not use this leaf (no
/// workflow-step type yet — future option, see cmd_workflow).
pub(crate) fn cmd_internal_wait_output(
    cfg: &Config,
    project: &str,
    target: &str,
    pattern: &str,
    regex: bool,
    timeout_ms: Option<u64>,
) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = find_workspace(&mut c, project)?;
    let agents = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    let Some(pane_id) = resolve_pane_id(&agents, target) else {
        let out = wait_output_json(false, None, "");
        print_json(&out);
        eprintln!("flywheel: target {target} not found in {project}");
        std::process::exit(1);
    };
    match hc::pane_wait_output(&c, &pane_id, wait_output_mode(regex), pattern, timeout_ms) {
        Ok(r) => {
            print_json(&wait_output_json(
                true,
                r.matched_line.as_deref(),
                &r.pane_id,
            ));
            Ok(())
        }
        Err(e) if e.code == "timeout" => {
            let out = wait_output_json(false, None, &pane_id);
            print_json(&out);
            eprintln!("flywheel: no match for {pattern:?} in {pane_id} within timeout");
            std::process::exit(1);
        }
        Err(e) => Err(anyhow::anyhow!("pane.wait_for_output failed: {e:#}")),
    }
}

pub(crate) fn cmd_internal_ready(cfg: &Config, project: &str) -> Result<()> {
    let beads = load_ready_beads(cfg, project)?;
    print_json(&durable::ready_leaf_envelope(&beads));
    Ok(())
}

/// internal leaf: in-progress beads as [{id, title, assignee}] via
/// `br list --status in_progress --json` (bd-627z.1: assignee drives the
/// stranded-reclaim + scoped-completion decision).
pub(crate) fn cmd_internal_inprogress(cfg: &Config, project: &str) -> Result<()> {
    let beads = load_inprogress_beads(cfg, project)?;
    print_json(&durable::inprogress_leaf_envelope(&beads));
    Ok(())
}

/// internal leaf (bd-627z.1, bd-qk9q, bd-reclaim-liveness-not-spelling-hn8b):
/// re-open stranded in_progress beads to ready. The decision resolves the
/// assignee through the spawn registry to its pane and asks hc whether
/// that pane is live (`durable::reclaim_decision`) — no spelling list. Kept
/// under BOTH registry names of a live pane (terminal name and mail pin), and a
/// live reservation for the bead itself (reason = bead id) is a lease no
/// reclaim may break. A spelling the registry cannot resolve and pane
/// evidence cannot refute is KEPT — the old name comparison reclaimed live
/// workers whose claim used a different spelling for the same agent, which
/// is the destructive race this fixes. An assignee-less bead with no lease is
/// still reclaimed (bd-qk9q). Reclaim = `br update <id> --status open
/// --assignee ""` so the bead returns to the ready pool and the loop
/// re-dispatches it.
///
/// Emits `{"reclaimed": [ids], "failed": [{"id", "error"}]}` — the durable
/// loop re-dispatches reclaimed ids this poll.
pub(crate) fn cmd_internal_reclaim_stranded(cfg: &Config, project: &str, live: &str) -> Result<()> {
    let live_names: Vec<String> = live
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let dir = project_dir(cfg, project);
    let registry = state::load_spawn_registry(dir.to_str().unwrap_or(project));
    let live_panes: std::collections::HashSet<String> = {
        let mut panes = std::collections::HashSet::new();
        if let Ok(b) = backend::multiplexer_backend(cfg)
            && let Ok(ws) = find_workspace_dyn(&*b, project)
            && let Ok(agents) = b.agent_list()
        {
            for a in agents {
                if a.workspace_id == ws {
                    panes.insert(a.pane_id.clone());
                }
            }
        }
        panes
    };
    let mut live_identities: std::collections::HashSet<String> = live_names.into_iter().collect();
    for e in &registry {
        if live_panes.contains(&e.pane_id) {
            live_identities.insert(e.name.clone());
            if let Some(pin) = &e.mail_identity {
                live_identities.insert(pin.clone());
            }
        }
    }
    let leased_beads: std::collections::HashSet<String> = {
        let key = project_dir_str(cfg, project)?;
        let rows = match mail_or_die(cfg) {
            Ok(mail) => mail.list_active(&key, None, None).unwrap_or_else(|e| {
                eprintln!(
                    "flywheel: reclaim lease read failed (continuing without lease evidence): {e:#}"
                );
                json!({ "reservations": [] })
            }),
            Err(e) => {
                eprintln!(
                    "flywheel: reclaim lease read mail unavailable (continuing without lease evidence): {e:#}"
                );
                json!({ "reservations": [] })
            }
        };
        rows.get("reservations")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("reason").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let world = durable::ReclaimWorld {
        live_identities: &live_identities,
        registry: &registry,
        live_panes: &live_panes,
        leased_beads: &leased_beads,
    };
    let beads = load_inprogress_beads(cfg, project)?;
    let mut reclaimed: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for b in &beads {
        match durable::reclaim_decision(&b.id, b.assignee.as_deref(), &world) {
            durable::ReclaimDecision::Keep { reason } => {
                if matches!(
                    reason,
                    durable::ReclaimHold::LiveLease | durable::ReclaimHold::UnknownIdentity
                ) {
                    eprintln!(
                        "flywheel: reclaim held for {} (assignee {:?}, {})",
                        b.id,
                        b.assignee,
                        reason.as_str()
                    );
                }
            }
            durable::ReclaimDecision::Reclaim { reason } => {
                match crate::br::run_br(
                    Some(&dir),
                    &["update", &b.id, "--status", "open", "--assignee", ""],
                ) {
                    Ok(_) => {
                        eprintln!(
                            "flywheel: reclaimed stranded bead {} (assignee {:?}, {})",
                            b.id,
                            b.assignee,
                            reason.as_str()
                        );
                        reclaimed.push(b.id.clone());
                    }
                    Err(e) => failed.push(json!({ "id": b.id, "error": format!("{e:#}") })),
                }
            }
        }
    }
    print_json(&json!({ "reclaimed": reclaimed, "failed": failed }));
    Ok(())
}

/// internal leaf: print the hc agent kind of a named agent.
pub(crate) fn cmd_internal_kind(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    println!("{}", agent.agent.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// internal leaf: the profile name an agent serves, from the spawn registry
/// (bd-gsvn.1). Empty line when the agent has no profile (plain swarm
/// spawns — byte-identical pre-migration behavior). Registry read only —
/// never consults hc.
pub(crate) fn cmd_internal_profile(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let dir = project_dir(cfg, project);
    let key = match dir.to_str() {
        Some(k) => k.to_string(),
        None => return Ok(()),
    };
    let registry = state::load_spawn_registry(&key);
    let profile = registry
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.profile.clone())
        .unwrap_or_default();
    println!("{profile}");
    Ok(())
}

/// internal leaf: print recent output of a named agent (context-marker check).
/// review-fix F6: routes through the backend factory (ADR-0002).
pub(crate) fn cmd_internal_read(cfg: &Config, project: &str, name: &str) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_id = find_workspace_dyn(&*backend, project)?;
    let agent = backend
        .agent_list()?
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("agent {name} not found in {project}"))?;
    let target = agent_target(&agent).to_string();
    let mut ops = hc::BackendAltScreenRead::new(&*backend, &target, 60);
    let text = match alt_screen_read_ladder(&mut ops, hc::RECENT_UNWRAPPED_SOURCE) {
        Ok(t) => t,
        Err(e) => return Err(anyhow!("{target}: alt-screen read failed: {e:#}")),
    };
    let dir = project_dir(cfg, project);
    let pin_identity = {
        let reg = state::load_spawn_registry(project);
        let names = state::load_agent_names(&dir);
        wake_identity_pin(&reg, &names, name)
    };
    let state: Option<agentstate::AgentStateFile> =
        agentstate::read_state_for_agent(&dir, name, Some(&pin_identity));
    print_json(&json!({ "terminal": text, "state": state }));
    Ok(())
}

/// internal leaf: close the agent's pane, split a fresh pane, restart the agent
/// under the same name (fresh session context). bd-ude8: omp-in-hc
/// EXITS on PTY stdin EOF, and `pane.close` on its pane delivers exactly
/// that — so omp restarts IN PLACE (same pane, no split, no close); other
/// kinds keep the split+close cycle.
pub(crate) fn cmd_internal_respawn(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    force: bool,
) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    respawn_with_backend(&*backend, cfg, project, name, kind, force)
}

/// Testable core of `cmd_internal_respawn` (bd-ude8): the mux backend is
/// injected so the omp same-pane path and the argv re-injection are unit-
/// observable; the factory stays at the wrapper.
pub(crate) fn respawn_with_backend(
    backend: &dyn backend::MultiplexerBackend,
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
    force: bool,
) -> Result<()> {
    let ws_id = find_workspace_dyn(backend, project)?;
    let dir = project_dir(cfg, project);
    let agents = backend.agent_list()?;
    if !force
        && let Some(a) = agents
            .iter()
            .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name))
    {
        let healthy = matches!(a.agent_status.as_str(), "idle" | "working" | "blocked");
        if healthy {
            print_json(&json!({
                "already_exists": true,
                "name": name,
                "pane_id": a.pane_id,
            }));
            return Ok(());
        }
    }
    let agent = agents
        .into_iter()
        .find(|a| a.workspace_id == ws_id && a.name.as_deref() == Some(name));
    let old_pane = agent.as_ref().map(|a| a.pane_id.clone());
    let same_pane = kind == "omp";
    let new_pane = if same_pane {
        old_pane.clone().ok_or_else(|| {
            anyhow!("omp respawn for {name} in {project}: no live pane to restart in")
        })?
    } else {
        let panes = backend.pane_list(Some(&ws_id))?;
        let anchor = old_pane
            .as_ref()
            .and_then(|old| {
                panes
                    .iter()
                    .find(|p| &p.pane_id != old)
                    .map(|p| p.pane_id.clone())
            })
            .or_else(|| panes.first().map(|p| p.pane_id.clone()))
            .ok_or_else(|| anyhow!("no pane in {project} to split for respawn"))?;
        let pane = backend.pane_split(&anchor, "down", Some(&utf8_path(&dir, "workspace dir")?))?;
        if let Some(old) = &old_pane {
            let _ = backend.pane_close(old);
        }
        pane.pane_id
    };
    let args = respawn_launch_args(cfg, project, name, kind);
    let fresh = start_agent_with_retry_dyn(backend, name, kind, &new_pane, &args)?;
    if !same_pane
        && let Ok(mail) = backend::mail_backend(cfg)
        && let Some(key) = dir.to_str()
    {
        if let Some(old) = &old_pane
            && let Err(e) = backend::run_mail(mail.clear_pane_identity(key, old))
        {
            eprintln!("flywheel: mail clear_pane_identity failed for {old}: {e}");
        }
        if let Err(e) = backend::run_mail(mail.write_pane_identity(
            key,
            &fresh.pane_id,
            &terminal_mail_name(name),
        )) {
            eprintln!(
                "flywheel: mail write_pane_identity failed for {name} on {}: {e}",
                fresh.pane_id
            );
        }
    }
    println!(
        "respawned {} {} -> {}",
        kind_paint(kind),
        old_pane.as_deref().unwrap_or("<gone>"),
        fresh.pane_id
    );
    Ok(())
}

/// bd-ude8: respawn argv = the agent's registry-pinned profile launch args
/// (bd-eign.9 / bd-l2ky parity — the SAME args as the original spawn, never
/// an empty list for a profiled agent). No registry entry, or an entry
/// without a profile (legacy) → empty argv (plain swarm agents keep harness
/// defaults — unchanged). Never fails the respawn on catalog trouble.
pub(crate) fn respawn_launch_args(
    cfg: &Config,
    project: &str,
    name: &str,
    kind: &str,
) -> Vec<String> {
    let Ok(key) = project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let profile_name = match state::load_spawn_registry(&key)
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.profile)
    {
        Some(p) => p,
        None => return Vec::new(),
    };
    let Ok(catalog) = profile::ProfileCatalog::load() else {
        return Vec::new();
    };
    match catalog
        .get(&profile_name)
        .map(|p| profile_launch_args(kind, p, None))
    {
        Some(Ok(args)) => args,
        Some(Err(e)) => {
            eprintln!(
                "flywheel: respawn of '{name}' profile '{profile_name}' failed model routing \
                 (bare launch, harness default model): {e}"
            );
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// internal leaf (ADR-0010 Spec E, P10.1): run one prompt headlessly through
/// a harness and log it. The leaf stays DUMB — the runner preprends the role
/// preamble; here: resolve profile → harness, run `harness -p --mode=json`,
/// capture the JSON stream, write `runs/<run_id>.jsonl`, print the outcome.
///
/// Output (json mode): `{ ok, run_id, response, status, tokens_in,
/// tokens_out, model, duration_ms, error? }` — the parent runner parses this
/// one line as the step value.
/// bd-42fs: is a headless outcome rate-limited (HTTP 429 in the harness
/// error stream or stderr)? Pure — the retry-on-429 trigger.
pub(crate) fn is_rate_limited(outcome: &headless::HarnessOutcome) -> bool {
    outcome
        .error
        .as_deref()
        .map(|e| e.contains("429") || e.to_lowercase().contains("rate limit"))
        .unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_internal_run_headless(
    profile_ref: &str,
    prompt: &str,
    model_override: Option<&str>,
    harness_override: Option<&str>,
    workflow: Option<&str>,
    cron: Option<&str>,
    phase: Option<&str>,
    effort_override: Option<&str>,
    json: bool,
) -> Result<()> {
    use sha1::Digest;

    /// One harness invocation attempt (bd-42fs retry-on-429 support).
    struct RunAttempt {
        model_id: String,
        model: String,
        outcome: headless::HarnessOutcome,
        rate_limited: bool,
        phase_degraded: bool,
    }

    let profile_name = headless::strip_profile_ref(profile_ref);
    let catalog = profile::ProfileCatalog::load()?;
    let profile = catalog
        .get(profile_name)
        .ok_or_else(|| anyhow!("unknown profile @{profile_name} (flywheel profile list)"))?;
    let registry = models::ModelRegistry::load_routing()?;
    let inherited = registry.resolve_profile_route(&profile.harness, &profile.model)?;
    if inherited.0.is_empty() && harness_override.is_none() {
        bail!("profile @{profile_name} has no harness — cannot run headless");
    }
    let harness = harness_override
        .filter(|h| !h.is_empty())
        .unwrap_or(inherited.0.as_str());
    if !matches!(harness, "pi" | "omp" | "opencode" | "grok") {
        bail!("unsupported harness for headless run: {harness} (pi | omp | opencode | grok)");
    }
    let model_id = match model_override {
        Some(over) => over.to_string(),
        None => inherited.1.clone(),
    };
    let model = if model_id.is_empty() {
        String::new()
    } else if model_override.is_some() {
        if registry.get(&model_id).is_none() {
            eprintln!(
                "flywheel: model {model_id:?} not in registry (flywheel model add) — passing through"
            );
        }
        registry.resolve(&model_id, harness)
    } else {
        registry.resolve_strict(&model_id, harness)?
    };

    let role_smol = resolve_role_flag(&registry, &profile.smol, harness)?;
    let role_slow = resolve_role_flag(&registry, &profile.slow, harness)?;
    let role_plan = resolve_role_flag(&registry, &profile.plan, harness)?;

    let run_once =
        |harness: &str, model_id: &str, model: &str, effort: &str| -> Result<RunAttempt> {
            let (argv, phase_degraded) = headless::build_argv_roles_phase(
                harness,
                (!model.is_empty()).then_some(model),
                (!effort.is_empty()).then_some(effort),
                &role_smol,
                &role_slow,
                &role_plan,
                prompt,
                phase,
            )
            .map_err(|e| anyhow!("headless run for @{profile_name}: {e}"))?;
            let started = std::time::Instant::now();
            let output = Command::new(&argv[0])
                .args(&argv[1..])
                .output()
                .with_context(|| format!("spawn {} for headless run", argv[0]))?;
            let duration_ms = started.elapsed().as_millis() as u64;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            let mut outcome = headless::parse_stream(&stdout, harness);
            outcome.exit_code = output.status.code().unwrap_or(-1);
            outcome.duration_ms = duration_ms;
            if outcome.response.is_empty() && outcome.error.is_none() && !stderr.trim().is_empty() {
                let trimmed = stderr.trim();
                if stderr_is_only_warnings(trimmed) {
                    outcome.error = Some(format!(
                        "harness produced no response and its stderr carried only warnings: \
                         {trimmed}"
                    ));
                } else {
                    outcome.error = Some(trimmed.to_string());
                }
            }
            if output.status.code() != Some(0) && outcome.error.is_none() {
                outcome.error = Some(format!("harness exited {}", output.status));
            }
            if outcome.error.is_some() {
                outcome.status = "fail".into();
            }
            let rate_limited = is_rate_limited(&outcome);
            Ok(RunAttempt {
                model_id: model_id.to_string(),
                model: model.to_string(),
                outcome,
                rate_limited,
                phase_degraded,
            })
        };

    let mut attempt = run_once(
        harness,
        &model_id,
        &model,
        // governed-by: bd-ulzi — effort stays profile-default with Jev
        effort_override
            .filter(|e| !e.trim().is_empty())
            .unwrap_or(profile.effort.as_str()),
    )?;
    let mut retried_fallback = false;
    {
        let err_str = attempt.outcome.error.as_deref().unwrap_or("");
        let cf = failure::classify(err_str, Some(attempt.outcome.exit_code), None);
        if cf.reroute_eligible {
            eprintln!(
                "flywheel: headless classified kind={} evidence={}",
                cf.kind, cf.evidence
            );
            if let Some(next_profile) =
                reroute_next_profile(profile_name, cf.kind, &cf.evidence, &[], 0)
                && let Some(fb_prof) = catalog.get(&next_profile)
            {
                let fb_harness = if fb_prof.harness.is_empty() {
                    harness
                } else {
                    fb_prof.harness.as_str()
                };
                let fb_model_id = if fb_prof.model.is_empty() {
                    model_id.clone()
                } else {
                    fb_prof.model.clone()
                };
                let fb_model = registry.resolve(&fb_model_id, fb_harness);
                eprintln!(
                    "flywheel: headless reroute {} -> {} (profile fallback)",
                    profile_name, next_profile
                );
                let agent = agent_ref::AgentRef::new("flywheel", profile_name);
                let intent = agent_ref::RebindIntent::prepare(
                    agent,
                    agent_ref::AgentBinding {
                        profile_name: profile_name.to_string(),
                        model: model_id.clone(),
                        harness: harness.to_string(),
                        location: agent_ref::BindingLocation::Pane,
                    },
                    agent_ref::AgentBinding {
                        profile_name: next_profile.clone(),
                        model: fb_model_id.clone(),
                        harness: fb_harness.to_string(),
                        location: agent_ref::BindingLocation::Pane,
                    },
                    agent_ref::RebindReason::ProfileFallback {
                        kind: cf.kind.to_string(),
                        evidence: cf.evidence.clone(),
                    },
                    0,
                );
                match agent_ref::commit_rebind("flywheel", intent, |_| {
                    run_once(fb_harness, &fb_model_id, &fb_model, &fb_prof.effort)
                }) {
                    Ok(fb) => {
                        attempt = fb;
                        retried_fallback = true;
                    }
                    Err(e) => {
                        eprintln!("flywheel: profile fallback run {next_profile} failed: {e:#}");
                    }
                }
            }
        }
    }
    if attempt.rate_limited && !model_id.is_empty() {
        let err_ev = attempt
            .outcome
            .error
            .clone()
            .unwrap_or_else(|| "429".into());
        let mut hopped = false;
        if agent_ref::RebindIntent::allowed_from_failure(failure::FailureKind::RateLimit, &err_ev) {
            for fb_id in &profile.model_fallback {
                if fb_id.is_empty() || fb_id == &model_id {
                    continue;
                }
                let rung = registry.route_for(fb_id, harness);
                let intent = pane::model_fallback_intent(
                    "flywheel",
                    profile_name,
                    profile_name,
                    &model_id,
                    harness,
                    &rung,
                    err_ev.clone(),
                );
                eprintln!(
                    "flywheel: @{profile_name} rate-limited on {model_id} (429) — model_fallback hop to {fb_id}"
                );
                match agent_ref::commit_rebind("flywheel", intent, |_| {
                    run_once(&rung.harness, &rung.model_id, &rung.model, &profile.effort)
                }) {
                    Ok(fb) => {
                        attempt = fb;
                        retried_fallback = true;
                        hopped = true;
                        break;
                    }
                    Err(e) => eprintln!("flywheel: model_fallback run {fb_id} failed: {e:#}"),
                }
            }
        }
        if !hopped && let Some(route) = registry.fallback_route(&model_id, harness) {
            let intent = pane::model_fallback_intent(
                "flywheel",
                profile_name,
                profile_name,
                &model_id,
                harness,
                &route,
                err_ev.clone(),
            );
            eprintln!(
                "flywheel: @{profile_name} rate-limited on {model_id} (429) — registry ModelEntry.fallback hop to {}",
                route.model_id
            );
            match agent_ref::commit_rebind("flywheel", intent, |_| {
                run_once(
                    &route.harness,
                    &route.model_id,
                    &route.model,
                    &profile.effort,
                )
            }) {
                Ok(fb) => {
                    attempt = fb;
                    retried_fallback = true;
                }
                Err(e) => eprintln!("flywheel: fallback run {} failed: {e:#}", route.model_id),
            }
        }
    }
    let RunAttempt {
        model_id: used_model_id,
        model: used_model,
        outcome,
        phase_degraded,
        ..
    } = attempt;

    let mut hasher = sha1::Sha1::new();
    sha1::Digest::update(&mut hasher, prompt.as_bytes());
    let prompt_hash = hex::encode(sha1::Digest::finalize(hasher));

    let entry = run_log::RunLogEntry {
        run_id: run_log::new_run_id(),
        ts: state::now_iso(),
        workflow: workflow.unwrap_or("").to_string(),
        cron: cron.map(str::to_string),
        profile: profile_name.to_string(),
        harness: harness.to_string(),
        model: used_model,
        mode: "headless".into(),
        prompt_hash,
        response: outcome.response.clone(),
        status: outcome.status.clone(),
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        duration_ms: outcome.duration_ms,
        exit_code: outcome.exit_code,
        error: outcome.error.clone(),
        phase: phase.filter(|p| !p.is_empty()).map(str::to_string),
        phase_degraded,
    };
    run_log::write_run(&entry)?;

    if json {
        let mut v = json!({
            "ok": outcome.error.is_none(),
            "run_id": entry.run_id,
            "response": entry.response,
            "status": entry.status,
            "tokens_in": entry.tokens_in,
            "tokens_out": entry.tokens_out,
            "model": entry.model,
            "model_id": used_model_id,
            "retried_fallback": retried_fallback,
            "duration_ms": entry.duration_ms,
        });
        if let Some(p) = &entry.phase {
            v["phase"] = json!(p);
            v["phase_degraded"] = json!(phase_degraded);
        }
        if let Some(e) = &entry.error {
            v["error"] = json!(e);
            let cf = failure::classify(e, Some(outcome.exit_code), None);
            if cf.kind == failure::FailureKind::RateLimit
                || cf.kind == failure::FailureKind::Timeout
                || cf.kind == failure::FailureKind::Billing
                || cf.kind == failure::FailureKind::Auth
            {
                v["kind"] = json!(cf.kind.to_string());
            }
        }
        print_json(&v);
    } else {
        let status = if outcome.error.is_some() {
            "FAIL"
        } else {
            "ok"
        };
        println!(
            "headless run {} [{status}] {} tok={}+{} {}ms profile={}{}",
            entry.run_id,
            entry.harness,
            entry.tokens_in,
            entry.tokens_out,
            entry.duration_ms,
            entry.profile,
            if retried_fallback {
                format!(" (fallback {used_model_id})")
            } else {
                String::new()
            },
        );
        if let Some(e) = &entry.error {
            eprintln!("headless run error: {e}");
        }
    }
    Ok(())
}

/// The headless ActionRunner dispatch (ADR-0010 Spec A headless row, P10.1):
/// `dispatch_job` resolves the step profile, prepends the behavior preamble
/// (Spec C), and shells out to the `__run-headless` leaf — subprocess-reuse,
/// never an in-process hc/toron client (Spec A). The leaf's JSON (one
/// line) flows into `{{steps.<name>}}`; its run log carries response + usage.
///
/// Non-dispatch actions stay recorded, never dispatched (P10.1 scope).
///
/// **spawn carve-out:** `Action::Spawn` never reaches this runner — the
/// engine handles it (`run_spawn`: durable promise + SpawnSink → kind-43001,
/// ADR-0005 Spec 4a), and headless MUST NOT use the promise↔job binding
/// (ADR-0010 Spec E). Headless spawn routing + the `await:false` detached
/// branch land with the pane runner (P10.3, bd-ghcj.1). P10.1 headless is
/// dispatch_job-driven — synchronous by engine design.
/// Parse one `__run-headless` leaf line. Valid JSON passes through;
/// anything else is a typed leaf failure carrying bounded evidence —
/// never `Ok(ok:false)` (ADR-0032 Spec 9 tail: a failed leaf fails the
/// parent run instead of masquerading as a value).
pub(crate) fn parse_leaf_output(trimmed: &str, raw: &str, err: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => Ok(v),
        Err(_) => {
            const LIMIT: usize = 2000;
            let cut = |s: &str| {
                if s.len() > LIMIT {
                    format!("{}…[truncated]", &s[..LIMIT])
                } else {
                    s.to_string()
                }
            };
            Err(anyhow!(
                "headless leaf returned non-JSON output (stdout: {}; stderr: {})",
                cut(raw),
                cut(err)
            ))
        }
    }
}

pub(crate) fn headless_dispatch(
    catalog: &profile::ProfileCatalog,
    wf_name: &str,
    inputs: &Value,
    cron: Option<&str>,
    action: &toron_workflow::schema::Action,
    project_dir: Option<&Path>,
) -> Result<Value> {
    let toron_workflow::schema::Action::DispatchJob { spec, .. } = action else {
        if matches!(action, toron_workflow::schema::Action::Spawn { .. }) {
            bail!(
                "headless runner: Action::Spawn must be handled by the engine (run_spawn) — routing bug"
            );
        }
        return Ok(json!({ "action": action.type_name() }));
    };
    let profile_ref = headless::dispatch_spec_profile(spec)
        .ok_or_else(|| anyhow!("headless dispatch: dispatch_job spec has no profile"))?;
    let profile_name = headless::strip_profile_ref(profile_ref);
    let profile = catalog.get(profile_name).ok_or_else(|| {
        anyhow!("headless dispatch: unknown profile @{profile_name} (flywheel profile list)")
    })?;
    let mut base = match headless::dispatch_spec_task(spec) {
        Some(task) if !task.trim().is_empty() => task.to_string(),
        _ => match headless::dispatch_spec_stage(spec) {
            Some(s) => match project_dir {
                Some(dir) => stage_prompt_for_project(s, dir)?,
                None => stage_prompt(s)?,
            },
            None => headless::base_prompt_for(wf_name, inputs),
        },
    };
    for key in ["plan", "project"] {
        if let Some(extra) = headless::spec_extra(spec, key, inputs) {
            base.push_str("\n\n");
            base.push_str(key);
            base.push_str(": ");
            base.push_str(&extra);
        }
    }
    let prompt = headless::conditioned_prompt(&profile.behavior, &base);

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__run-headless")
        .arg(profile_name)
        .arg(&prompt)
        .arg("--workflow")
        .arg(wf_name)
        .arg("--json");
    let harness =
        headless::effective_step_harness(headless::dispatch_spec_harness(spec), &profile.harness);
    if !harness.is_empty() {
        cmd.arg("--harness").arg(harness);
    }
    if let Some(p) = headless::dispatch_spec_phase(spec)
        && !p.trim().is_empty()
    {
        cmd.arg("--phase").arg(p);
    }
    // governed-by: bd-ulzi — the spec effort input rides to the leaf,
    if let Some(e) = headless::spec_extra(spec, "effort", inputs)
        && !e.trim().is_empty()
    {
        cmd.arg("--effort").arg(e);
    }
    if let Some(cron) = cron {
        cmd.arg("--cron").arg(cron);
    }
    if !profile.model.is_empty() && models::slot_alias(&profile.model).is_none() {
        cmd.arg("--model").arg(&profile.model);
    }
    let output = cmd
        .output()
        .with_context(|| format!("__run-headless for @{profile_name}"))?;
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let trimmed = raw.trim();
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    let v = parse_leaf_output(trimmed, &raw, &err)?;
    if phase_chain::plan_artifact_missing(spec, inputs) {
        eprintln!(
            "flywheel: phase implement dispatch has NO completed plan artifact — \
             warn-first gate (ADR-0022 §3.5); step output tagged plan_artifact_missing"
        );
        crate::state::log_event(
            "flywheel",
            crate::state::EventKind::Workflow,
            plan_warning_notice(wf_name, "implement"),
        )
        .ok();
    }
    Ok(phase_chain::annotate_dispatch_output(spec, inputs, v))
}

/// bd-omwg: headless fail-closed gate (ADR-0010 Spec E). A workflow carrying
/// a `spawn` step cannot run headless: there is no pane to launch and no
/// honest kind-43001/43004 — faking them is forbidden. Refuse BEFORE the
/// engine invoke (the engine's `run_spawn` would otherwise return
/// `ok:true` with no pane — the exact lie this bead kills). Walks nested
/// steps (retry/branch/foreach/parallel), because a nested spawn is the
/// same lie. Returns the offending step names.
fn workflow_spawn_steps(wf: &toron_workflow::Workflow) -> Vec<String> {
    fn walk(action: &toron_workflow::schema::Action, name: Option<&str>, out: &mut Vec<String>) {
        match action {
            toron_workflow::schema::Action::Spawn { .. } => {
                let n = name.unwrap_or("<unnamed>").to_string();
                if !out.contains(&n) {
                    out.push(n);
                }
            }
            toron_workflow::schema::Action::Retry { step, .. } => walk(&step.action, name, out),
            toron_workflow::schema::Action::Branch {
                then, else_steps, ..
            } => {
                for s in then.iter().chain(else_steps.iter()) {
                    walk(&s.action, s.name.as_deref(), out);
                }
            }
            toron_workflow::schema::Action::Foreach { each, .. } => walk(&each.action, name, out),
            toron_workflow::schema::Action::Parallel { each, .. } => walk(&each.action, name, out),
            _ => {}
        }
    }
    let mut out = vec![];
    for step in &wf.steps {
        walk(&step.action, step.name.as_deref(), &mut out);
    }
    out
}

/// bd-omwg: the gate itself — error names every spawn step and points at
/// pane mode.
pub(crate) fn headless_spawn_gate(wf: &toron_workflow::Workflow) -> Result<()> {
    let steps = workflow_spawn_steps(wf);
    if steps.is_empty() {
        return Ok(());
    }
    bail!(
        "headless cannot run spawn step(s) {:?}: a spawn launches a real pane — use --mode pane; faking kind-43001/43004 is forbidden (ADR-0010 Spec E)",
        steps
    );
}

#[cfg(test)]
mod spawn_drain_gate_tests {
    use super::*;

    fn parse(yaml: &str) -> toron_workflow::Workflow {
        toron_workflow::parse_workflow(yaml).expect("parse workflow")
    }

    /// bd-omwg: a flat spawn step is a headless violation — the gate must
    /// refuse it (ADR-0010 Spec E fail-closed, never fake kind-43001/43004).
    #[test]
    fn headless_spawn_gate_rejects_flat_spawn_step() {
        let wf = parse(
            r#"
name: gate-flat
trigger:
  type: webhook
steps:
  - name: job
    action: spawn
    profile: "@housekeeper"
    await: false
"#,
        );
        let err = headless_spawn_gate(&wf).unwrap_err();
        assert!(err.to_string().contains("job"), "{err}");
        assert!(
            err.to_string().contains("pane"),
            "points at pane mode: {err}"
        );
    }

    /// bd-omwg: a spawn nested inside retry/branch/foreach/parallel is the
    /// same lie — the gate walks nested steps, not just the top level.
    #[test]
    fn headless_spawn_gate_walks_nested_steps() {
        let wf = parse(
            r#"
name: gate-nested
trigger:
  type: webhook
steps:
  - name: outer
    action: branch
    if: "{{inputs.x}}"
    then:
      - name: inner-job
        action: spawn
        profile: "@coder"
        await: true
"#,
        );
        let err = headless_spawn_gate(&wf).unwrap_err();
        assert!(err.to_string().contains("inner-job"), "{err}");
    }

    /// bd-omwg: a spawn-free workflow passes the gate untouched.
    #[test]
    fn headless_spawn_gate_passes_spawn_free_workflow() {
        let wf = parse(
            r#"
name: gate-clean
trigger:
  type: webhook
steps:
  - name: hello
    action: send_dm
    to: operator
    body: "hi"
"#,
        );
        headless_spawn_gate(&wf).expect("no spawn -> pass");
    }
}
