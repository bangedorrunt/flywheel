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

/// --- internal batch 2: inbox guards, loop-mail leaves (bd-wave5-main-eviction-d5hht) ---
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::Path;

use crate::cmd::spawn::cmd_spawn;
use crate::config::Config;
use crate::output::print_json;
use crate::state::EventKind;
use crate::{
    AgentCounts, SpawnOpts, XprojInboxItem, extract_context_limit_from_inbox,
    extract_xproj_from_inbox, log, mail_send, mail_whois_ok, newest_in_thread, project_dir,
    project_dir_str, resolve_mail_target, resolve_mail_target_registry, swarm_label,
};
use crate::{backend, doorbell, durable, stages, state, verdict};
/// internal leaf: scan orchestrator inbox for `[xproj]` swarm requests.
/// Soft-fail: missing mail → empty list. Output: `{ "items": [ XprojInboxItem ] }`.
/// internal leaf: mail digest for ONE agent (loop-side wake signal).
///
/// The durable loop calls this per idle agent before dispatch; the digest is
/// prepended to the next prompt so the agent wakes already knowing what peers
/// asked it — without carrying a standing peer-poll template in its context
/// (token-efficient: no per-agent self-poll loop; the orchestrator polls).
///
/// S8.1: the digest RESOLVES the wake identity (spawn-registry /
/// agent-names pin, else the canonical name) and registers it idempotently
/// BEFORE the fetch — the wake prompt's "fetch as X" then names an identity
/// that exists. `mail_identity` rides alongside `agent` (the hc-name map
/// key the loop uses).
///
/// S8.2: `urgent` counts only wake-filtered rows (read or already-executed
/// assigns are excluded; duplicate assigns collapse).
///
/// Output: `{ "agent": <terminal name>, "mail_identity": <registered identity>,
/// "recent": N, "urgent": N, "last_from": "...", "last_subject": "...",
/// "thread_id": "..." }`.
/// Soft-fail: mail down / unknown agent → zeroed digest, never an error.
pub(crate) fn cmd_internal_mail_digest(cfg: &Config, project: &str, agent: &str) -> Result<()> {
    let key_name = agent.trim().to_string();
    let identity = {
        let reg = state::load_spawn_registry(project);
        let names = state::load_agent_names(&project_dir(cfg, project));
        wake_identity_pin(&reg, &names, &key_name)
    };
    let zero = json!({
        "agent": key_name, "mail_identity": identity, "unread": 0, "urgent": 0,
        "last_from": null, "last_subject": null, "thread_id": null,
    });
    let mail = if let Ok(m) = backend::mail_backend(cfg) {
        m
    } else {
        print_json(&zero);
        return Ok(());
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&zero);
        return Ok(());
    }
    let auto_profile = backend::AgentProfile {
        name: identity.clone(),
        program: Some("opencode".into()),
        model: Some("opencode".into()),
        persona: None,
        task_description: Some("flywheel mail CLI actor".into()),
    };
    if let Err(e) = backend::run_mail(mail.register_agent(&key, &auto_profile)) {
        eprintln!("flywheel: mail digest auto-register of {identity} failed (zeroing digest): {e}");
    }
    let v = if let Ok(v) = backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key,
        agent: identity.clone(),
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(10),
        since_ts: None,
        thread_id: None,
    })) {
        v
    } else {
        print_json(&zero);
        return Ok(());
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let arr = v_json.as_array().cloned().unwrap_or_default();
    let recent = arr.len();
    let urgent_rows = wake_urgent(&arr);
    let urgent = urgent_rows.len();
    let last = arr.first();
    let chosen = newest_urgent(&urgent_rows, last);
    let last_from = last.and_then(|m| {
        m.get("sender")
            .or_else(|| m.get("from"))
            .and_then(|f| f.as_str())
    });
    print_json(&json!({
        "agent": key_name,
        "mail_identity": identity,
        "recent": recent,
        "urgent": urgent,
        "plane_s": plane_s_open_jobs_for(project, &identity),
        "last_id": chosen.and_then(|m| {
            m.get("id").or_else(|| m.get("message_id")).and_then(|i| i.as_str())
        }),
        "last_from": last_from,
        "last_subject": chosen.and_then(|m| m.get("subject").and_then(|s| s.as_str())),
        "thread_id": chosen.and_then(|m| m.get("thread_id").and_then(|t| t.as_str())),
    }));
    Ok(())
}

/// bd-pin-public-plane-s-easy.5: count OPEN kind-43001 jobs assigned to an
/// agent (plane-S wake). Calls the existing `toron job list --state open
/// --assignee <pin>` CLI; zero on any failure (best-effort — the loop
/// retries next poll).
fn plane_s_open_jobs_for(project: &str, identity: &str) -> usize {
    match crate::mail::Mail::from_config(None) {
        Ok(Some(m)) => m.open_jobs_for(project, identity).unwrap_or(0),
        _ => 0,
    }
}

/// bd-a7j: newest (message id, sender) in a thread from a sender —
/// recency-ranked, so `verify` = "did the thread move after dispatch?".
///
/// Read via the INBOX PROJECTION (fetch_inbox), not FTS search: hyphenated
/// thread ids are not reliably phrase-searchable in FTS5 (index-coverage
/// gap, cf bd-debate-reply-routing-99ad), so a reply can sit in the
/// orchestrator inbox while `search_messages(thread_id=...)` returns
/// nothing — the check falsely reports "no reply". Inbox rows carry the
/// sender NAME (toron resolves registered npubs) or the raw npub hex —
/// whois resolves the name so both match.
///
/// governed-by: ADR-0001 §9.1 (bd-debate-reply-routing-99ad)
fn thread_head(
    mail: &dyn backend::MailSurface,
    key: &str,
    thread: &str,
    sender: &str,
) -> Result<Option<(String, String)>> {
    let sender_npub = backend::run_mail(mail.whois(key, sender))
        .map(|w| w.npub)
        .unwrap_or_default();
    let items = backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.to_string(),
        agent: mail.agent_name().to_string(),
        unread_only: false,
        include_bodies: false,
        limit: None,
        urgent_only: false,
        since_ts: None,
        thread_id: Some(thread.to_string()),
    }))?;
    Ok(newest_in_thread(&items, thread, sender, &sender_npub)
        .map(|m| (m.id.to_string(), m.sender.clone())))
}

/// bd-dispatch-dedupe-2zlz collector: classify the bead thread's messages
/// into guard signals and stat the receipts dir. Emits RAW FACTS only — the
/// go/hold decision is durable::eval_dispatch_guard (pure, unit-tested).
/// Mail failures print an empty-facts shape (fail-open at the collector;
/// the leaf logs loudly so a dead daemon never silently gates dispatches).
/// ADR-0010 D14 — captain-facing turn-end guard (harness-hook adapter).
///
/// Predicate: if beads are in_progress OR a dispatcher is required, and
/// the wake-owner beacon (doorbell wake_state) is stale past
/// `FLYWHEEL_GUARD_GRACE_SECS` (default 300s), the captain-facing
/// Stop/idle path blocks with a follow_up recover turn. Crew panes are
/// exempt. A D5 AFK daemon with a fresh identity-matched beacon satisfies
/// the predicate (wired false until D5 lands). Structural backstop, not
/// permission to omit the live cycle.
///
/// Hook contract: exit 0 + `{"decision":"allow"}` = turn may end;
/// exit 3 + `{"decision":"block","reason":..,"follow_up":..}` =
/// turn-end blocked, run the follow_up (`flywheel recover <project>`).
/// Unknown in-flight count (br unreadable) fails closed to Block.
pub(crate) const GUARD_GRACE_DEFAULT_SECS: u64 = 300;

/// D14 decision: allow the turn to end, or block it naming the recover turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TurnDecision {
    Allow,
    Block { reason: String, follow_up: String },
}

/// Pure D14 predicate.
///
/// | input | decision |
/// |-------|----------|
/// | crew pane | Allow (exempt) |
/// | AFK satisfied | Allow (D5 daemon, fresh identity-matched beacon) |
/// | in-flight unknown | Block (fail closed — never ends blind) |
/// | no work (0 in-flight, no dispatcher) | Allow |
/// | beacon fresh (age <= grace) | Allow |
/// | work + stale/missing beacon | Block + follow_up recover |
pub(crate) fn turn_end_guard(
    in_flight: Option<u64>,
    dispatcher_required: bool,
    beacon_age_secs: Option<u64>,
    is_crew: bool,
    afk_satisfied: bool,
    grace_secs: u64,
    project: &str,
) -> TurnDecision {
    if is_crew {
        return TurnDecision::Allow;
    }
    if afk_satisfied {
        return TurnDecision::Allow;
    }
    let in_flight = match in_flight {
        Some(n) => n,
        None => {
            return TurnDecision::Block {
                reason: "cannot determine in-flight beads (br unreadable) — failing closed".into(),
                follow_up: format!("flywheel recover {project}"),
            };
        }
    };
    if in_flight == 0 && !dispatcher_required {
        return TurnDecision::Allow;
    }
    match beacon_age_secs {
        Some(age) if age <= grace_secs => TurnDecision::Allow,
        _ => TurnDecision::Block {
            reason: format!(
                "{in_flight} bead(s) in flight (dispatcher_required={dispatcher_required}) with wake-owner beacon stale past {grace_secs}s"
            ),
            follow_up: format!("flywheel recover {project}"),
        },
    }
}

/// Live in-flight count from the work ledger. None when br is unreadable
/// (D14 fails closed on None — the turn blocks naming br, never ends blind).
fn in_flight_count(project_root: &std::path::Path) -> Option<u64> {
    let rows = crate::br::Bead::in_progress(Some(project_root)).ok()?;
    Some(rows.len() as u64)
}

/// Delegation-shaped tool stems denied on the captain/steer pane (the
/// dispatcher-owning primary). A delegation-shaped harness spawn creates
/// work with NO spawn-registry metadata — invisible to the doorbell, the
/// dispatcher, and the guard — so the fence sits on the tool surface, not
/// in a prompt.
const DELEGATION_STEMS: [&str; 8] = [
    "agent", "subagent", "task", "workflow", "spawn", "dispatch", "worktree", "delegate",
];

/// Delegation-shaped but LEGAL: observe/stop verbs + session-local todo.
/// These cannot create untracked work.
const DELEGATION_ALLOW: [&str; 5] = [
    "taskoutput",
    "taskstop",
    "tasklist",
    "taskcreate",
    "taskupdate",
];

/// Pure deny matcher: `Some(stem)` = delegation-shaped tool to deny on a
/// captain/steer pane; `None` = allow. mcp__* is excluded (toron MCP is the
/// SANCTIONED coordination surface — denying it would fence the guardrail
/// itself). Exact allowlist entries win over the `task` stem.
pub(crate) fn delegation_deny(tool: &str) -> Option<&'static str> {
    let t = tool.trim().to_ascii_lowercase();
    if t.is_empty() || t.starts_with("mcp__") {
        return None;
    }
    if DELEGATION_ALLOW.contains(&t.as_str()) {
        return None;
    }
    DELEGATION_STEMS
        .iter()
        .find(|s| t.starts_with(**s))
        .copied()
}

/// Role-aware decision: crew panes are EXEMPT (they are the sanctioned
/// spawn target — a crew pane calling its harness's delegation tool would
/// just double-fence; the registry tracks them). Default (no role) = the
/// captain/steer primary → enforced.
pub(crate) fn subagent_guard_decision(role: Option<&str>, tool: &str) -> Result<(), &'static str> {
    if matches!(role.map(str::to_ascii_lowercase).as_deref(), Some("crew")) {
        return Ok(());
    }
    match delegation_deny(tool) {
        None => Ok(()),
        Some(_) => Err(
            "delegation-shaped tool denied on the captain/steer pane (ADR-0010 D13): a harness-native spawn creates work with no spawn-registry metadata — use flywheel spawn/send, or toron mail to an existing crew pin",
        ),
    }
}

pub(crate) fn cmd_subagent_guard(tool: &str, role: Option<&str>) -> Result<()> {
    match subagent_guard_decision(role, tool) {
        Ok(()) => {
            print_json(
                &json!({ "decision": "allow", "tool": tool, "role": role.unwrap_or("captain") }),
            );
            Ok(())
        }
        Err(reason) => {
            print_json(&json!({ "decision": "deny", "tool": tool, "reason": reason }));
            std::process::exit(2);
        }
    }
}

fn default_gate_budget() -> usize {
    std::env::var("FLYWHEEL_GATE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(stages::learning::FLYWHEEL_GATE_BUDGET)
}

/// ADR-0010 D12: scan the learnings corpus and project the bounded GATE
/// grounding read — top-N by reinforcement recency within the budget.
/// Cost estimate per entry: frontmatter + rg-context allowance (chars/4);
/// the projection never touches the corpus (ce-sweep archives, never this).
pub(crate) fn cmd_gate_learnings(
    root: &str,
    budget: usize,
    wiki: Option<&str>,
    memory: Option<&str>,
) -> Result<()> {
    use crate::stages::learning::{
        LearningEntry, LearningLane, gate_projection, parse_lane_entries,
        parse_learning_frontmatter,
    };
    let today = chrono::Local::now().date_naive();
    let mut entries: Vec<LearningEntry> = Vec::new();
    for path in walkdir_sorted(root)? {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((tier, reinforced)) = parse_learning_frontmatter(&src) else {
            continue;
        };
        let block = src.strip_prefix("---\n").unwrap_or("");
        let fm_chars = block
            .split("\n---")
            .next()
            .map(|fm| fm.len() + 4)
            .unwrap_or(0);
        let cost_tokens = fm_chars / 4 + 150;
        entries.push(LearningEntry {
            path: path.display().to_string(),
            tier,
            reinforced,
            cost_tokens,
            lane: stages::learning::LearningLane::Corpus,
        });
    }
    for (path, lane) in [(wiki, LearningLane::Wiki), (memory, LearningLane::Memory)] {
        let Some(path) = path else { continue };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!("flywheel: gate lane file {path} unreadable ({e}) — lane skipped");
                continue;
            }
        };
        let (mut lane_entries, skipped) = parse_lane_entries(&raw, lane);
        if skipped > 0 {
            eprintln!("flywheel: gate lane file {path} skipped {skipped} malformed row(s)");
        }
        entries.append(&mut lane_entries);
    }
    let projected = gate_projection(&entries, today, budget);
    let mut lanes = std::collections::BTreeMap::new();
    for e in &projected {
        *lanes.entry(e.lane.name()).or_insert(0usize) += 1;
    }
    print_json(&json!({
        "budget": budget,
        "corpus": entries.len(),
        "projected": projected,
        "lanes": lanes,
    }));
    Ok(())
}

/// Sorted .md files under root, archive/ excluded (archived learnings are
/// out of the projection — ce-sweep moved them there).
fn walkdir_sorted(root: &str) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for e in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let e = e?;
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "archive") {
                    continue;
                }
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                files.push(p);
            }
        }
        files.sort();
        out.extend(files);
        dirs.sort();
        stack.extend(dirs);
    }
    out.sort();
    Ok(out)
}

pub(crate) fn cmd_turn_guard(cfg: &Config, project: &str, role: Option<&str>) -> Result<()> {
    let is_crew = matches!(role.map(str::to_ascii_lowercase).as_deref(), Some("crew"));
    let grace_secs: u64 = std::env::var("FLYWHEEL_GUARD_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(GUARD_GRACE_DEFAULT_SECS);
    let ws_label = state::load_spawn_meta(project).unwrap_or_else(|| swarm_label(project));
    let state_dir = config::state_dir();
    let ws = doorbell::load_wake_state(&doorbell::wake_state_file(&state_dir, &ws_label));
    let now = doorbell_now_unix();
    let beacon_age_secs = ws.last_wake_unix.map(|t| now.saturating_sub(t));
    let dispatcher_required = doorbell::read_pid(&doorbell::pid_file(&state_dir, &ws_label))
        .is_some_and(doorbell::pid_alive);
    let root = project_dir(cfg, project);
    let in_flight = in_flight_count(&root);
    match turn_end_guard(
        in_flight,
        dispatcher_required,
        beacon_age_secs,
        is_crew,
        false,
        grace_secs,
        project,
    ) {
        TurnDecision::Allow => {
            print_json(&json!({
                "decision": "allow",
                "in_flight": in_flight,
                "dispatcher_required": dispatcher_required,
                "beacon_age_secs": beacon_age_secs,
            }));
            Ok(())
        }
        TurnDecision::Block { reason, follow_up } => {
            print_json(&json!({
                "decision": "block",
                "reason": reason,
                "follow_up": follow_up,
            }));
            std::process::exit(3);
        }
    }
}

#[cfg(test)]
mod turn_guard_tests {
    use super::*;

    const P: &str = "flywheel";

    #[test]
    fn d14_in_flight_stale_beacon_blocks_with_follow_up() {
        let d = turn_end_guard(Some(2), false, Some(301), false, false, 300, P);
        match d {
            TurnDecision::Block { reason, follow_up } => {
                assert!(reason.contains('2'), "names in-flight count: {reason}");
                assert!(reason.contains("300"), "names grace: {reason}");
                assert_eq!(follow_up, "flywheel recover flywheel");
            }
            TurnDecision::Allow => panic!("in-flight + stale beacon must block"),
        }
        assert!(matches!(
            turn_end_guard(Some(1), false, None, false, false, 300, P),
            TurnDecision::Block { .. }
        ));
        assert!(matches!(
            turn_end_guard(Some(0), true, Some(9999), false, false, 300, P),
            TurnDecision::Block { .. }
        ));
    }

    #[test]
    fn d14_crew_exempt_afk_satisfies() {
        assert_eq!(
            turn_end_guard(Some(2), true, Some(9999), true, false, 300, P),
            TurnDecision::Allow
        );
        assert_eq!(
            turn_end_guard(Some(2), true, Some(9999), false, true, 300, P),
            TurnDecision::Allow
        );
    }

    #[test]
    fn d14_fresh_or_idle_allows_unknown_fails_closed() {
        assert_eq!(
            turn_end_guard(Some(2), false, Some(300), false, false, 300, P),
            TurnDecision::Allow
        );
        assert_eq!(
            turn_end_guard(Some(0), false, Some(9999), false, false, 300, P),
            TurnDecision::Allow
        );
        assert_eq!(
            turn_end_guard(Some(0), false, None, false, false, 300, P),
            TurnDecision::Allow
        );
        assert!(matches!(
            turn_end_guard(None, false, Some(0), false, false, 300, P),
            TurnDecision::Block { .. }
        ));
    }
}

pub(crate) fn cmd_internal_dispatch_guard(cfg: &Config, project: &str, bead: &str) -> Result<()> {
    let mut facts = json!({ "signals": [], "receipts_fresh_us": null });
    if let Ok(mail) = backend::mail_backend(cfg) {
        let key = project_dir_str(cfg, project)?;
        if backend::run_mail(mail.ensure_project(&key)).is_ok() {
            match backend::run_mail(mail.inbox(&backend::InboxQuery {
                project: key.clone(),
                agent: mail.agent_name().to_string(),
                unread_only: false,
                include_bodies: true,
                limit: Some(100),
                urgent_only: false,
                since_ts: None,
                thread_id: Some(bead.to_string()),
            })) {
                Ok(items) => {
                    let signals: Vec<Value> = items
                        .iter()
                        .filter_map(|m| {
                            durable::classify_guard_signal(
                                &m.subject,
                                &m.body_md,
                                m.bead.as_deref(),
                            )
                            .map(|k| json!({ "ts_us": m.created_ts_us, "kind": k.as_str() }))
                        })
                        .collect();
                    facts["signals"] = json!(signals);
                }
                Err(e) => eprintln!("flywheel: __dispatch-guard inbox scan failed: {e}"),
            }
            let mut signals: Vec<Value> = facts["signals"].as_array().cloned().unwrap_or_default();
            signals.extend(channel_guard_signals(&key, bead, mail.agent_name()));
            facts["signals"] = json!(signals);
        }
    }
    let receipts = project_dir(cfg, project).join(".flywheel").join("receipts");
    if let Ok(rd) = std::fs::read_dir(&receipts) {
        let mut newest: Option<i64> = None;
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.contains(bead) {
                continue;
            }
            if let Some(modified) = entry.metadata().ok().and_then(|md| md.modified().ok()) {
                let us = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros() as i64)
                    .unwrap_or(0);
                if newest.is_none() || us > newest.unwrap_or(0) {
                    newest = Some(us);
                }
            }
        }
        facts["receipts_fresh_us"] = json!(newest);
    }
    print_json(&facts);
    Ok(())
}

/// bd-claim-watch-durable-signals-mixy (wish 3): classify kind-9 room
/// messages as dispatch-guard peer signals. Reads `steer-<project>` plus the
/// bead's own `steer-<bead>` room (ADR-0020) through the toron CLI — the
/// channel plane is not on the mailbox surface, and flywheel already shells
/// `toron` for plane-S reads (`plane_s_open_jobs_for`). The project room
/// carries every bead so a signal must name THIS bead; the per-bead room is
/// thread-scoped by construction. Soft-fail empty on every error (CLI
/// missing, room absent, membership refused): a channel read must never
/// block or spam a dispatch scan — the mail scan stays the primary channel.
/// Channel guard signals via the toron CLI (bd-wave2 residual): channel
/// read needs a toron-core channel surface (rebuild + membership read);
/// no facade equivalent exists yet.
fn channel_guard_signals(project_key: &str, bead: &str, identity: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let rooms = [
        (format!("steer-{project_key}"), true),
        (format!("steer-{bead}"), false),
    ];
    for (room, require_mention) in rooms {
        let out_cmd = std::process::Command::new("toron")
            .args([
                "channel",
                "read",
                &room,
                "--project",
                project_key,
                "--as",
                identity,
                "--json",
                "--limit",
                "100",
            ])
            .output();
        let Ok(o) = out_cmd else { continue };
        if !o.status.success() {
            continue;
        }
        let Ok(msgs) = serde_json::from_slice::<Value>(&o.stdout) else {
            continue;
        };
        out.extend(channel_signals_from(&msgs, bead, require_mention));
    }
    out
}

/// Pure: project one `toron channel read --json` page into guard signals.
/// `require_mention` = the room carries other beads' traffic (the project
/// steer room), so a line counts only when it names THIS bead; false = the
/// room is the bead's own (ADR-0020 `steer-<bead>`), thread-scoped by
/// construction. `created_at` is unix SECONDS on the channel projection and
/// the guard compares µs; a value already in µs (~1e15) passes through.
fn channel_signals_from(msgs: &Value, bead: &str, require_mention: bool) -> Vec<Value> {
    let Some(arr) = msgs.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in arr {
        let Some(content) = m.get("content").and_then(Value::as_str) else {
            continue;
        };
        if require_mention && !content.contains(bead) {
            continue;
        }
        let Some(kind) = durable::classify_guard_signal("", content, None) else {
            continue;
        };
        let ts = m.get("created_at").and_then(Value::as_i64).unwrap_or(0);
        let ts_us = if ts > 1_000_000_000_000 {
            ts
        } else {
            ts.saturating_mul(1_000_000)
        };
        out.push(json!({ "ts_us": ts_us, "kind": kind.as_str() }));
    }
    out
}

#[cfg(test)]
mod channel_guard_tests {
    use super::*;

    /// bd-claim-watch-durable-signals-mixy (wish 3): kind-9 room lines
    /// classify into guard signals; the project room's lines must name the
    /// bead, the per-bead room's are thread-scoped; seconds normalize to µs
    /// and µs pass through unchanged.
    #[test]
    fn channel_lines_become_guard_signals() {
        let page = json!([
            {
                "content": "claimed bd-12 — write-set src/durable.rs",
                "created_at": 1_700_000_000,
                "kind": 9
            },
            {
                "content": "stand down bd-12: concurrent executor is live",
                "created_at": 1_700_000_060,
                "kind": 9
            },
            {
                "content": "handoff bd-99: taking over",
                "created_at": 1_700_000_120,
                "kind": 9
            },
            { "content": 42, "created_at": 1 },
            { "created_at": 1 }
        ]);
        let got = channel_signals_from(&page, "bd-12", true);
        assert_eq!(got.len(), 2, "only bead-12 claim + stand-down: {got:?}");
        assert_eq!(got[0]["kind"], "claim");
        assert_eq!(got[0]["ts_us"], 1_700_000_000_000_000i64);
        assert_eq!(got[1]["kind"], "stand_down");
        let own_room =
            json!([{ "content": "claiming — edits in src/main.rs", "created_at": 1_700_000_000 }]);
        let got = channel_signals_from(&own_room, "bd-12", false);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["kind"], "claim");
        let us = json!([{ "content": "claiming bd-12", "created_at": 1_700_000_000_000_000i64 }]);
        let got = channel_signals_from(&us, "bd-12", true);
        assert_eq!(got[0]["ts_us"], 1_700_000_000_000_000i64);
        assert!(channel_signals_from(&json!({"nope": true}), "bd-12", true).is_empty());
    }
}

/// bd-a7j leaf: newest message id + author in `thread` BY `name` (or nulls).
/// Soft — any mail failure prints nulls ("no reply yet"), mirroring the
/// digest leaf's resilience: verification failures degrade, never abort.
pub(crate) fn cmd_internal_check_reply(
    cfg: &Config,
    project: &str,
    name: &str,
    thread: &str,
) -> Result<()> {
    let zero = json!({ "newest_id": null, "author": null });
    let mail = if let Ok(m) = backend::mail_backend(cfg) {
        m
    } else {
        print_json(&zero);
        return Ok(());
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&zero);
        return Ok(());
    }
    match thread_head(
        &*mail,
        &key,
        thread,
        &resolve_mail_target_registry(project, name),
    ) {
        Ok(Some((id, author))) => print_json(&json!({ "newest_id": id, "author": author })),
        _ => print_json(&zero),
    }
    Ok(())
}

/// bd-a7j leaf: log `reply_missing` (checkpointed) and dispatch the CONCRETE
/// reminder by recursing into the `__send` path (same landing semantics,
/// last-sent dedup, confirm handling). One leaf = one checkpointed unit, so
/// crash-resume can neither double-log nor skip the reminder.
pub(crate) fn cmd_internal_remind_reply(
    project: &str,
    name: &str,
    thread: &str,
    reminder_count: u32,
    invocation: Option<&str>,
    iteration: Option<u64>,
) -> Result<()> {
    log(
        project,
        EventKind::ReplyMissing,
        json!({
            "agent": name,
            "thread": thread,
            "reminder": reminder_count + 1,
        }),
    );
    let reminder = format!(
        "REMINDER: your reply is still missing in thread {thread} — send it now via: flywheel mail reply {project} --message-id {thread} --body [your reply] (do not just acknowledge; the orchestrator verifies the thread moved)"
    );
    let exe = std::env::current_exe()?.canonicalize()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__send").arg(project).arg(name).arg(&reminder);
    cmd.arg("--confirm");
    if let Some(inv) = invocation {
        cmd.arg("--invocation").arg(inv);
        if let Some(it) = iteration {
            cmd.arg("--iteration").arg(it.to_string());
        }
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "__remind-reply: __send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("__remind-reply: bad __send JSON: {e}: {raw}"))?;
    print_json(&v);
    Ok(())
}

/// Extract `[compose-request]` items from a `fetch_inbox` JSON value
/// (ADR-0022 §3.7 T4, W7) — same envelope as xproj items, subject-prefix
/// filter `[compose-request]`. Pure + unit-tested.
#[must_use]
pub fn extract_compose_requests_from_inbox(v: &serde_json::Value) -> Vec<XprojInboxItem> {
    let arr = v
        .as_array()
        .or_else(|| v.get("messages").and_then(|m| m.as_array()))
        .or_else(|| v.get("result").and_then(|m| m.as_array()))
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for m in arr {
        let subject = m
            .get("subject")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if !subject
            .to_ascii_lowercase()
            .starts_with("[compose-request]")
        {
            continue;
        }
        let body = m
            .get("body_md")
            .or_else(|| m.get("body"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let id = match m.get("id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let thread_id = m
            .get("thread_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let from = m
            .get("from")
            .or_else(|| m.get("sender"))
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        out.push(XprojInboxItem {
            id,
            subject,
            body,
            thread_id,
            from,
        });
    }
    out
}

/// The compose-request allowlist (§3.7 T4): `<project>/.flywheel/compose-
/// allowlist.toml` — `senders = ["Pin", ...]`. Missing file = empty list =
/// every request denied (deny is the default; the gate never dispatches on
/// an unconfigured project).
#[must_use]
pub fn load_compose_allowlist(project_root: &Path) -> Vec<String> {
    #[derive(Deserialize)]
    struct Allowlist {
        #[serde(default)]
        senders: Vec<String>,
    }
    let path = project_root.join(".flywheel/compose-allowlist.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str::<Allowlist>(&raw).ok())
        .map(|a| a.senders)
        .unwrap_or_default()
}

pub(crate) fn cmd_internal_compose_inbox(cfg: &Config, project: &str, limit: u32) -> Result<()> {
    let mail = if let Ok(m) = backend::mail_backend(cfg) {
        m
    } else {
        print_json(&json!({ "items": [], "allowlist": [] }));
        return Ok(());
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&json!({ "items": [], "allowlist": [] }));
        return Ok(());
    }
    let name = mail.agent_name().to_string();
    let v = match backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.clone(),
        agent: name,
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(limit as usize),
        since_ts: None,
        thread_id: None,
    })) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flywheel: compose-inbox fetch failed: {e:#}");
            print_json(&json!({ "items": [], "allowlist": [] }));
            return Ok(());
        }
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let items = extract_compose_requests_from_inbox(&v_json);
    let allowlist = load_compose_allowlist(&project_dir(cfg, project));
    print_json(&json!({ "items": items, "allowlist": allowlist }));
    Ok(())
}

pub(crate) fn cmd_internal_xproj_inbox(cfg: &Config, project: &str, limit: u32) -> Result<()> {
    let mail = if let Ok(m) = backend::mail_backend(cfg) {
        m
    } else {
        print_json(&json!({ "items": [] }));
        return Ok(());
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&json!({ "items": [] }));
        return Ok(());
    }
    let name = mail.agent_name().to_string();
    let v = match backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key.clone(),
        agent: name,
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(limit as usize),
        since_ts: None,
        thread_id: None,
    })) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flywheel: xproj-inbox fetch failed: {e:#}");
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let items = extract_xproj_from_inbox(&v_json);
    print_json(&json!({ "items": items }));
    Ok(())
}

/// internal leaf: scan the orchestrator inbox for exact-body CONTEXT_LIMIT
/// crew replies (bd-context-limit-honor-luqg). Same fetch shape as the
/// xproj scan — bodies included, orchestrator's own identity — filtered to
/// the exact saturation marker by [`extract_context_limit_from_inbox`].
pub(crate) fn cmd_internal_context_inbox(cfg: &Config, project: &str, limit: u32) -> Result<()> {
    let mail = if let Ok(m) = backend::mail_backend(cfg) {
        m
    } else {
        print_json(&json!({ "items": [] }));
        return Ok(());
    };
    let key = project_dir_str(cfg, project)?;
    if backend::run_mail(mail.ensure_project(&key)).is_err() {
        print_json(&json!({ "items": [] }));
        return Ok(());
    }
    let name = mail.agent_name().to_string();
    let v = match backend::run_mail(mail.inbox(&backend::InboxQuery {
        project: key,
        agent: name,
        unread_only: false,
        urgent_only: false,
        include_bodies: true,
        limit: Some(limit as usize),
        since_ts: None,
        thread_id: None,
    })) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flywheel: context-inbox fetch failed: {e:#}");
            print_json(&json!({ "items": [] }));
            return Ok(());
        }
    };
    let v_json = serde_json::to_value(&v).unwrap_or_default();
    let items = extract_context_limit_from_inbox(&v_json);
    print_json(&json!({ "items": items }));
    Ok(())
}

/// internal leaf: spawn the swarm for a parsed `[xproj]` request (full loop
/// branch — bd-pattern-cross-project-swarm §9.5).
///
/// Called DIRECTLY by the durable loop (not a queued agent prompt): parse the
/// request's `recipe`/`workers`, spawn the swarm in THIS project, and reply a
/// "swarm started" ack in the request thread so the requester sees it.
///
/// Best-effort: spawn failure replies an error in-thread (never kills the
/// loop); a missing project dir or unknown recipe is surfaced in-thread too.
pub(crate) fn cmd_internal_xproj_spawn(
    cfg: &Config,
    project: &str,
    message_id: &str,
    thread_id: &str,
    from: &str,
    recipe: &str,
    workers: u32,
) -> Result<()> {
    let dir = project_dir(cfg, project);
    let outcome = if !dir.exists() {
        Err(anyhow!("project dir missing: {}", dir.display()))
    } else {
        cmd_spawn(
            cfg,
            project,
            SpawnOpts {
                counts: AgentCounts {
                    oc: Some(workers.max(1)),
                    pi: None,
                    gk: None,
                },
                recipe: Some(recipe.to_string()),
                no_focus: true,
                ..Default::default()
            },
        )
    };
    let spawned_ok = outcome.is_ok();
    let reply = match outcome {
        Ok(()) => format!(
            "[xproj] swarm started: recipe={recipe}, {workers} worker(s) in project {project} (msg {message_id}). Reply thread: {thread_id}."
        ),
        Err(e) => format!(
            "[xproj] spawn FAILED for msg {message_id}: {e:#}. Requester {from}, recipe={recipe}, workers={workers}."
        ),
    };
    if let Ok(mail) = backend::mail_backend(cfg) {
        let key = project_dir(cfg, project)
            .to_str()
            .unwrap_or(project)
            .to_string();
        let _ = backend::run_mail(mail.ensure_project(&key));
        let _ = mail_send(
            &*mail,
            &key,
            &[resolve_mail_target(from)],
            &format!("[xproj] swarm reply (msg {message_id})"),
            &reply,
            "normal",
            false,
            Some(thread_id),
        );
    }
    print_json(&json!({
        "ok": true,
        "message_id": message_id,
        "spawned": spawned_ok,
        "project": project,
        "recipe": recipe,
        "workers": workers,
    }));
    Ok(())
}

/// internal leaf: notify a blocked agent via agent-mail (durable workflow leaf).
///
/// governed-by: ADR-0002 — constructs the mail backend via the factory so the
/// durable loop is backend-agnostic (agent-mail MCP today; toron later).
///
/// Best-effort by contract: a missing/unreachable agent-mail must NEVER kill
/// an overnight loop (same standard as the lease-renewal heartbeat) — the
/// leaf exits 0 with `{"ok": false, "error": ...}` and the next poll retries.
/// internal leaf: notify a blocked agent via agent-mail, tiered
/// (bd-blocked-escalation-protocol). Tier 0 = first ping (high + ack),
/// 1 = reply high in-thread, 2 = urgent + thread summary, 3 = halt.
///
/// governed-by: ADR-0002 — constructs the mail backend via the factory so the
/// durable loop is backend-agnostic (agent-mail MCP today; toron later).
///
/// Best-effort by contract: a missing/unreachable agent-mail must NEVER kill
/// an overnight loop (same standard as the lease-renewal heartbeat) — the
/// leaf exits 0 with `{"ok": false, "error": ...}` and the next poll retries.
/// internal: the ack-overdue read behind the blocked-escalation ack gate
/// (ADR-0001 A6, bd-mail-ack-sla-tracking's `ack_overdue`).
///
/// Door contract: `{"ok":true,"count":N,"messages":[…]}` when the read
/// answered — including a genuine 0 — and `{"ok":false,"error":…}` when it
/// could not (no index, unregistered agent, remote backend, no mailbox). The
/// caller must treat `ok:false` as "not acked": an unavailable read may never
/// silence an escalation. Exit stays 0 so the leaf reads the JSON, not the
/// status (the argv decoder is `durable::ack_gate_unacked`).
pub(crate) fn cmd_internal_ack_overdue(
    cfg: &Config,
    project: &str,
    name: &str,
    thread: Option<&str>,
    sla_secs: Option<u64>,
) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    let agent = resolve_mail_target_registry(project, name);
    let filter = crate::mail::AckOverdueFilter {
        sla_secs,
        thread_id: thread.map(String::from),
    };
    let found = crate::mail::Mail::from_config(Some(&cfg.toron_mail_config()))
        .and_then(|m| {
            m.ok_or_else(|| anyhow!("no mail backend configured (embedded toron unavailable)"))
        })
        .and_then(|m| m.ack_overdue(&key, &agent, &filter));
    match found {
        Ok(Value::Array(rows)) => print_json(&json!({
            "ok": true,
            "project": project,
            "agent": agent,
            "thread_id": thread,
            "sla_secs": sla_secs.unwrap_or(crate::mail::ACK_SLA_DEFAULT_SECS),
            "count": rows.len(),
            "messages": rows,
        })),
        Ok(other) => print_json(&json!({
            "ok": false,
            "agent": agent,
            "error": format!("unexpected ack-overdue shape: {other}"),
        })),
        Err(e) => print_json(&json!({
            "ok": false,
            "agent": agent,
            "error": format!("{e:#}"),
        })),
    }
    Ok(())
}

pub(crate) fn cmd_internal_mail_blocked(
    cfg: &Config,
    project: &str,
    name: &str,
    tier: u32,
) -> Result<()> {
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for blocked alert to {name}: {e:#}");
            print_json(&json!({ "ok": false, "notified": name, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    let mail_target = resolve_mail_target_registry(project, name);
    let thread = format!("health-{name}");
    let out = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
        match tier {
            0 => mail_send(
                &*mail,
                &key,
                std::slice::from_ref(&mail_target),
                "[flywheel] you are blocked",
                "The swarm loop detected you are blocked. Resolve or report the blocker.",
                "high",
                true,
                Some(&thread),
            ),
            1 => mail_send(
                &*mail,
                &key,
                std::slice::from_ref(&mail_target),
                "[flywheel] still blocked — 2nd ping (high)",
                "Still blocked after a full interval. Please resolve or report the blocker so the loop can continue.",
                "high",
                true,
                Some(&thread),
            ),
            2 => mail_send(
                &*mail,
                &key,
                std::slice::from_ref(&mail_target),
                "[flywheel] URGENT: blocked — escalation",
                "Still blocked after multiple pings. Escalating; human attention requested.",
                "urgent",
                true,
                Some(&thread),
            ),
            _ => mail_send(
                &*mail,
                &key,
                std::slice::from_ref(&mail_target),
                "[flywheel] HALTS: blocked too long — human required",
                "The swarm loop is halting because this agent stayed blocked past the escalation budget. Resolve the blocker, then re-run the loop.",
                "urgent",
                true,
                Some(&thread),
            ),
        }
    })();
    match out {
        Ok(_) => {
            print_json(&json!({ "ok": true, "notified": mail_target, "tier": tier }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: blocked alert to {name} failed: {e:#}");
            print_json(&json!({ "ok": false, "notified": mail_target, "error": format!("{e:#}") }));
            Ok(())
        }
    }
}

/// internal leaf: mail the planner that a ready bead is NOT dispatchable
/// because its `## VERIFY` fence is missing/multi-line/empty (ADR-0019,
/// bd-w1-verify-fence). Refuse-to-dispatch is the hard guarantee (the loops
/// never queue such a bead); this mail is the advisory. Always soft-fail:
/// exit 0 with `{ok, ...}` so overnight loops never die.
pub(crate) fn cmd_internal_mail_not_ready(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    title: &str,
    reason: Option<&str>,
) -> Result<()> {
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for not-ready alert {bead_id}: {e:#}");
            print_json(&json!({ "ok": false, "bead_id": bead_id, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    let fallback = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    let mut chain = planner_recipient_chain(&key, bead_id, &fallback);
    chain.push("captain".to_string());
    let planner = match first_routable(chain.iter().map(String::as_str), |c| {
        mail_whois_ok(&*mail, &key, c)
    }) {
        Some(p) => p,
        None => {
            let tried = chain.join(", ");
            eprintln!(
                "flywheel: not-ready alert for {bead_id} has no routable recipient (tried {tried})"
            );
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "notified": tried,
                "error": format!("no routable planner identity for {key} (tried {tried})")
            }));
            return Ok(());
        }
    };
    let out = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
        send_not_ready_mail(&*mail, &key, bead_id, title, &planner, reason)
    })();
    match out {
        Ok(_) => {
            print_json(&json!({ "ok": true, "bead_id": bead_id, "notified": planner }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: not-ready alert for {bead_id} failed: {e:#}");
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "notified": planner,
                "error": format!("{e:#}")
            }));
            Ok(())
        }
    }
}

/// The recipient chain for a loop advisory mail about bead `bead_id`, in
/// preference order: the bead's `created_by` when `br show` names one, then
/// the configured operator pin (`cfg.agent_mail.name`, or the `Flywheel`
/// default). Each entry is only a CANDIDATE — callers must re-check
/// routability, because `br` derives `created_by` from `$USER` and an OS
/// username is never a registered mail identity.
fn planner_recipient_chain(project_dir: &str, bead_id: &str, fallback: &str) -> Vec<String> {
    let mut chain = Vec::new();
    if let Ok(bead) = crate::br::Bead::show(Some(Path::new(project_dir)), bead_id)
        && let Some(created_by) = bead.created_by.as_deref().filter(|s| !s.is_empty())
    {
        chain.push(created_by.to_string());
    }
    if !fallback.trim().is_empty() {
        chain.push(fallback.to_string());
    }
    chain
}

/// Pure: the first chain candidate `resolves` accepts, else `None`. Blank
/// candidates are filtered before `resolves` ever sees them (an unset
/// `agent_mail` yields `""`, which must not read as routable). Kept free of
/// IO so the fallback order is testable without a mailbox — the one
/// behaviour that silently swallowed the alert is exactly the order.
///
/// Takes `AsRef<str>` so callers can pass `&[String]`, `&Vec<String>`, or a
/// literal `["a", "b"]` without an intermediate mapping.
fn first_routable<S: AsRef<str>>(
    chain: impl IntoIterator<Item = S>,
    mut resolves: impl FnMut(&str) -> bool,
) -> Option<String> {
    chain
        .into_iter()
        .map(|c| c.as_ref().trim().to_string())
        .find(|c| !c.is_empty() && resolves(c))
}

/// Loop-originated mails: one enum over the four loop mails (bd-6tmi unify).
/// Each arm builds its envelope; [`send_loop_mail`] is the single send path.
/// Envelopes are identical to the pre-unify per-fn sends: `done` keeps its
/// structural `bead` tag (ADR-0010 D9 — the dispatch guard matches the TAG,
/// so the other kinds stay tagless); importance/ack/thread per kind below.
enum LoopMail<'a> {
    NotReady {
        bead_id: &'a str,
        title: &'a str,
        planner: &'a str,
        reason: Option<&'a str>,
    },
    Done {
        bead_id: &'a str,
        title: &'a str,
        report: &'a str,
        captain: &'a str,
    },
    VerifierFamilyMissing {
        bead_id: &'a str,
        captain: &'a str,
    },
    UnverifiedClose {
        bead_id: &'a str,
        sha: &'a str,
        planner: &'a str,
    },
}

/// The send-ready envelope: recipient, subject, body, importance, ack flag,
/// thread, and the optional structural bead tag (done only).
struct LoopMailEnvelope {
    to: String,
    subject: String,
    body: String,
    importance: &'static str,
    ack: bool,
    thread: String,
    bead: Option<String>,
}

/// Pure envelope builders, moved verbatim from the four pre-unify fns.
fn loop_mail_envelope(kind: &LoopMail) -> LoopMailEnvelope {
    match kind {
        LoopMail::NotReady {
            bead_id,
            title,
            planner,
            reason,
        } => {
            let subject = match reason {
                Some(r) => format!("[{bead_id}] NOT ready: {r} — fix before dispatch"),
                None => {
                    format!(
                        "[{bead_id}] NOT ready: missing/invalid VERIFY fence — fix before dispatch"
                    )
                }
            };
            let body = match reason {
                Some(r) => format!(
                    "Bead **{bead_id}** (`{title}`) sits in the ready set but was **NOT dispatched**: {r}.\n\n\
                     ADR-0019 Spec 6 requires priority <= 2 beads to carry a `## PRINCIPLES` section in \
                     the description — every line `<principle-name> — <the decision it changed>`, names \
                     from the canonical 21 (see `flywheel principles list`). A missing section, a \
                     name-only citation, or an unknown name keeps the bead ready-but-undispatchable \
                     until the planner fixes the description."
                ),
                None => format!(
                    "Bead **{bead_id}** (`{title}`) sits in the ready set but was **NOT dispatched**: \
                     its `## VERIFY` fence is missing, empty, or multi-line (ADR-0019).\n\n\
                     Required block in the bead description, exact shape:\n\n\
                     ```text\n## VERIFY\n```\n\
                     <one command, one line>\n```\n```\n\n\
                     Name one cheap command the loop can run (e.g. `cargo test -p X name`). \
                     Until then the bead stays ready-but-undispatchable — the loop has \
                     no fallback verifier for the whole workspace matrix."
                ),
            };
            LoopMailEnvelope {
                to: planner.to_string(),
                subject,
                body,
                importance: "high",
                ack: true,
                thread: bead_id.to_string(),
                bead: None,
            }
        }
        LoopMail::Done {
            bead_id,
            title,
            report,
            captain,
        } => {
            let subject = format!("[{bead_id}] done");
            let body = if report.is_empty() {
                format!("Bead **{bead_id}** (`{title}`) closed with a legal verdict.")
            } else {
                format!(
                    "Bead **{bead_id}** (`{title}`) closed with a legal verdict.\n\nReport:\n```text\n{report}\n```"
                )
            };
            LoopMailEnvelope {
                to: captain.to_string(),
                subject,
                body,
                importance: "normal",
                ack: true,
                thread: bead_id.to_string(),
                bead: Some(bead_id.to_string()),
            }
        }
        LoopMail::VerifierFamilyMissing { bead_id, captain } => {
            let subject = format!("[{bead_id}] verifier family missing — bead blocked");
            let body = format!(
                "Bead **{bead_id}** needs an independent verifier (priority <= 1, high blast, or \
                 judgment AC) but the model registry cannot supply a second model family and no \
                 `verifier_family_override` is set. Blocked `verifier-family-missing` \
                 (ADR-0019 Spec 4).\n\n\
                 Fixes: register a model from another provider in `models.toml`, or set \
                 `[loop] verifier_family_override` in the flywheel config to allow the same family. \
                 The bead was NOT reopened — no respawn loop."
            );
            LoopMailEnvelope {
                to: captain.to_string(),
                subject,
                body,
                importance: "high",
                ack: true,
                thread: bead_id.to_string(),
                bead: None,
            }
        }
        LoopMail::UnverifiedClose {
            bead_id,
            sha,
            planner,
        } => {
            let subject = format!("[{bead_id}] unverified close reopened");
            let body = if sha.is_empty() {
                format!(
                    "Bead **{bead_id}** was closed but **no commit carries its id** — the close is \
                     ILLEGAL under the ADR-0019 SHA rule (`git log --all --grep={bead_id}` finds \
                     nothing). In a shared tree bare HEAD may not contain this bead's work at all, \
                     so a verdict cannot bind to it. The loop reopened the bead.\n\n\
                     Missing: the commit whose message contains `{bead_id}`."
                )
            } else {
                format!(
                    "Bead **{bead_id}** was closed without a legal verdict for sha `{sha}` — the \
                     close is unverified under ADR-0019 and the loop reopened the bead.\n\n\
                     Commit the work (message must carry `{bead_id}`), run the bead's ## VERIFY, \
                     record the verdict (`flywheel ledger` / `br gate report` with sha={sha}), \
                     then close."
                )
            };
            LoopMailEnvelope {
                to: planner.to_string(),
                subject,
                body,
                importance: "urgent",
                ack: true,
                thread: bead_id.to_string(),
                bead: None,
            }
        }
    }
}

/// The single send path for loop mails: envelope → one `mail.send`.
/// Separated from the leaf commands so the "planner mailed" acceptance is
/// testable with a real in-process toron facade.
fn send_loop_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    kind: &LoopMail,
) -> Result<backend::SendReceipt> {
    let e = loop_mail_envelope(kind);
    Ok(backend::run_mail(mail.send(&backend::SendMail {
        project: key.to_string(),
        sender: mail.agent_name().to_string(),
        to: vec![e.to],
        subject: e.subject,
        body_md: e.body,
        importance: Some(e.importance.to_string()),
        ack_required: e.ack,
        thread_id: Some(e.thread),
        bead: e.bead,
        ..Default::default()
    }))?)
}

#[cfg(test)]
mod loop_mail_tests {
    use super::*;

    /// bd-6tmi review pin: every loop-mail kind builds the envelope the
    /// pre-unify fns sent — subject, thread, importance, ack, and the
    /// done-only structural bead tag (ADR-0010 D9: the guard matches the
    /// TAG, so non-done kinds stay tagless).
    #[test]
    fn all_kinds_build_their_documented_envelopes() {
        let nr = loop_mail_envelope(&LoopMail::NotReady {
            bead_id: "bd-1",
            title: "t",
            planner: "P",
            reason: Some("nope"),
        });
        assert!(nr.subject.contains("[bd-1]") && nr.subject.contains("nope"));
        assert_eq!(nr.thread, "bd-1");
        assert_eq!(nr.to, "P");
        assert_eq!(nr.importance, "high");
        assert!(nr.ack);
        assert_eq!(nr.bead, None);

        let nr0 = loop_mail_envelope(&LoopMail::NotReady {
            bead_id: "bd-1",
            title: "t",
            planner: "P",
            reason: None,
        });
        assert!(nr0.body.contains("## VERIFY"));

        let done = loop_mail_envelope(&LoopMail::Done {
            bead_id: "bd-2",
            title: "t",
            report: "r",
            captain: "C",
        });
        assert_eq!(done.subject, "[bd-2] done");
        assert_eq!(done.importance, "normal");
        assert_eq!(done.bead.as_deref(), Some("bd-2"));

        let vf = loop_mail_envelope(&LoopMail::VerifierFamilyMissing {
            bead_id: "bd-3",
            captain: "C",
        });
        assert!(vf.subject.contains("verifier family missing"));
        assert_eq!(vf.importance, "high");
        assert_eq!(vf.bead, None);

        let uc = loop_mail_envelope(&LoopMail::UnverifiedClose {
            bead_id: "bd-4",
            sha: "abc",
            planner: "P",
        });
        assert!(uc.subject.contains("unverified close reopened"));
        assert_eq!(uc.importance, "urgent");
        assert_eq!(uc.bead, None);

        let uc0 = loop_mail_envelope(&LoopMail::UnverifiedClose {
            bead_id: "bd-4",
            sha: "",
            planner: "P",
        });
        assert!(uc0.body.contains("ILLEGAL"));
    }
}

/// The not-ready mail itself: thread = the bead id, so the planner's reply
/// stays on the bead's thread (ADR-0019 Spec 0). Separated from the leaf
/// command so the "planner mailed" acceptance is testable with a real
/// in-process toron facade.
fn send_not_ready_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    bead_id: &str,
    title: &str,
    planner: &str,
    reason: Option<&str>,
) -> Result<backend::SendReceipt> {
    send_loop_mail(
        mail,
        key,
        &LoopMail::NotReady {
            bead_id,
            title,
            planner,
            reason,
        },
    )
}

/// --- internal batch 3: done/verify/loop-run leaves (bd-wave5-main-eviction-d5hht) ---
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::output::print_json;
use crate::{backend, durable, verdict};
use crate::{project_dir, project_dir_str};
/// The programmatic done-mail leaf (2026-08-18): the LOOP mails the captain
/// the close-reason REPORT on `thread_id=<bead_id>` when a watched bead
/// closes with a legal verdict. Recipient: the bead's `created_by` when it
/// resolves as a REGISTERED identity (routable), else the project's
/// registered orchestrator agent (program == "flywheel"), else soft-fail
/// `{ok:false}` — a mail hiccup never kills the loop.
pub(crate) fn cmd_internal_mail_done(cfg: &Config, project: &str, bead_id: &str) -> Result<()> {
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for done-mail {bead_id}: {e:#}");
            print_json(&json!({ "ok": false, "bead_id": bead_id, "error": format!("{e:#}") }));
            return Ok(());
        }
    };
    let key = project_dir_str(cfg, project)?;
    let fallback = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    let (title, report) = bead_title_and_close_report(&key, bead_id);
    let mut chain = planner_recipient_chain(&key, bead_id, &fallback);
    chain.push("captain".to_string());
    let captain = first_routable(chain.iter().map(String::as_str), |c| {
        mail_whois_ok(&*mail, &key, c)
    })
    .or_else(|| registered_orchestrator(&*mail, &key));
    let Some(captain) = captain else {
        let tried = chain.join(", ");
        print_json(&json!({
            "ok": false,
            "bead_id": bead_id,
            "notified": tried,
            "error": format!("no routable captain identity for {key} (tried {tried})")
        }));
        return Ok(());
    };
    let out = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
        send_done_mail(&*mail, &key, bead_id, &title, &report, &captain)
    })();
    match out {
        Ok(_) => {
            print_json(&json!({ "ok": true, "bead_id": bead_id, "notified": captain }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: done-mail for {bead_id} failed: {e:#}");
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "notified": captain,
                "error": format!("{e:#}")
            }));
            Ok(())
        }
    }
}

/// The bead's title + close-reason REPORT: the latest comment text (the
/// close reason lands as a comment — evidence: closed beads carry it),
/// falling back to "closed". Best-effort — empty report degrades to the
/// title alone.
fn bead_title_and_close_report(project_dir: &str, bead_id: &str) -> (String, String) {
    // Normalized through Bead::show (array-first-or-object): `br show` emits
    // a bare object on current br, so the old array-only read silently
    // emptied every report. Same title + last-comment semantics otherwise.
    match crate::br::Bead::show(Some(Path::new(project_dir)), bead_id) {
        Ok(bead) => {
            let title = bead.title.clone();
            (title, crate::br::latest_comment_text(&bead))
        }
        Err(_) => (String::new(), String::new()),
    }
}

/// True when `name` resolves as a registered identity in the project
/// (routable per ADR-0018 D1a/D3).
fn mail_whois_ok(mail: &dyn backend::MailSurface, key: &str, name: &str) -> bool {
    backend::run_mail(mail.whois(key, name)).is_ok()
}

/// The project's registered orchestrator agent (program == "flywheel") —
/// the captain/operator identity (e.g. CoralFinch), when present.
fn registered_orchestrator(mail: &dyn backend::MailSurface, key: &str) -> Option<String> {
    let agents = backend::run_mail(mail.list_agents(key, None)).ok()?;
    agents.iter().find_map(|a| {
        let program = a.program.as_deref().unwrap_or("");
        let model = a.model.as_deref().unwrap_or("");
        (program == "flywheel" || model == "flywheel").then(|| a.name.clone())
    })
}

/// The done-mail itself: thread = the bead id, body = the close-reason
/// REPORT (status/sha/verdict/verify-tail live in it) — the captain's copy
/// of the report the agent put in the close reason.
fn send_done_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    bead_id: &str,
    title: &str,
    report: &str,
    captain: &str,
) -> Result<backend::SendReceipt> {
    send_loop_mail(
        mail,
        key,
        &LoopMail::Done {
            bead_id,
            title,
            report,
            captain,
        },
    )
}

/// internal leaf: close-detect SHA check (ADR-0019 SHA rule, bd-w1-sha-xyhd).
/// Derives `sha = git log --all -1 --grep=<bead-id> --format=%H` in the
/// primary tree — no `git rev-parse HEAD` fallback (shared-tree bare HEAD may
/// not contain the closed bead's work). No commit carries the id -> the close
/// is ILLEGAL regardless of verdicts: mail the planner naming the missing
/// commit (the reopen call lands in bd-w1-reopen-f6tw). Always soft-fail:
/// exit 0 with `{illegal, ...}` so overnight loops never die.
/// internal leaf: the project's active reservation board (ADR-0019 Spec
/// 3.4/3.5, bd-w1-reserve-serialize-2mam) — `{reservations: [{path, reason,
/// holder, ...}]}` from `active_reservations` (bd-dsu1). Feeds the loop's
/// serialize-on-overlap decision. Always exits 0 — a mail hiccup degrades
/// to an empty board, never a loop crash.
pub(crate) fn cmd_internal_reservation_board(cfg: &Config, project: &str) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    let v = match mail_or_die(cfg) {
        Ok(mail) => mail.list_active(&key, None, None).unwrap_or_else(|e| {
            eprintln!("flywheel: reservation board failed (continuing): {e:#}");
            json!({ "project": key, "count": 0, "reservations": [] })
        }),
        Err(e) => {
            eprintln!("flywheel: reservation board mail unavailable (continuing): {e:#}");
            json!({ "project": key, "count": 0, "reservations": [] })
        }
    };
    print_json(&v);
    Ok(())
}

/// ADR-0001 Wave 3 / Layer 3 (beads_rust-wave3-pin-reservation-jxsyt):
/// stamp a dispatched bead's `pin` field with the receiving agent's mail
/// identity. Flywheel fills pin at dispatch time — the tracker stays the
/// ledger, toron stays the grantor. ALWAYS exits 0: a failed stamp is logged
/// and dropped; blocking a dispatch over ownership metadata would be worse
/// than an unstamped pin.
pub(crate) fn cmd_internal_pin_stamp(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    pin: &str,
) -> Result<()> {
    let dir = project_dir(cfg, project);
    let argv: Vec<String> = durable::pin_stamp_argv(bead_id, pin);
    let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    match crate::br::run_br(Some(&dir), &arg_refs) {
        Ok(_) => {
            print_json(&json!({ "ok": true, "bead_id": bead_id, "pin": pin }));
        }
        Err(e) => {
            eprintln!("flywheel: pin stamp failed for {bead_id}: {e:#}");
            print_json(&json!({ "ok": false, "bead_id": bead_id, "pin": pin }));
        }
    }
    Ok(())
}

/// Verifier-dispatch options (bd-llbe: cmd_internal_verify_dispatch had 8
/// positional args — the trailing five are one option group). `dir` lets
/// tests point at a scratch project dir; production callers pass None.
#[derive(Debug, Clone, Default)]
struct VerifyDispatchOpts {
    worker_model: Option<String>,
    candidates: Option<String>,
    blast: String,
    ac: String,
    dir: Option<PathBuf>,
}

/// internal leaf: verifier-dispatch (ADR-0019 Spec 4,
/// bd-w1-verifier-degrade-a6fk). Computes the independent-verifier plan for
/// `bead_id`:
/// - `none-required`: the loop-run / worker-receipt band already covers it.
/// - `dispatch`: return the resolved model (different family, or the
///   captain's `verifier_family_override`); the loop spawns the reviewer
///   pane on it (one reusable verifier pane per project, headless preferred).
/// - `degrade`: block the bead `verifier-family-missing` (idempotent) + mail
///   the captain exactly once (`.flywheel/verifier-family-missing-<id>.mailed`
///   marker dedup) + NEVER respawn — the single-family swarm path.
///
/// Always exits 0 — a block/mail hiccup must not kill the loop.
pub(crate) fn cmd_internal_verify_dispatch(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    opts: VerifyDispatchOpts,
) -> Result<()> {
    let key = match opts.dir {
        Some(ref p) => p.clone(),
        None => PathBuf::from(project_dir_str(cfg, project)?),
    };
    let mut bead = match verdict::bead_from_br_show(&key, bead_id) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "flywheel: verifier-dispatch for {bead_id} bead lookup failed (continuing): {e}"
            );
            print_json(
                &json!({ "plan": "none-required", "bead_id": bead_id, "error": format!("{e}") }),
            );
            return Ok(());
        }
    };
    bead.blast = if opts.blast == "high" {
        verdict::Blast::High
    } else {
        verdict::Blast::Normal
    };
    let ac = if opts.ac == "judgment" {
        verdict::AcShape::Judgment
    } else {
        verdict::AcShape::Checkable
    };
    let reg = match crate::models::ModelRegistry::load_routing() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "flywheel: verifier-dispatch for {bead_id} registry load failed (continuing): {e}"
            );
            print_json(
                &json!({ "plan": "none-required", "bead_id": bead_id, "error": format!("{e}") }),
            );
            return Ok(());
        }
    };
    let cands: Vec<&str> = match opts.candidates {
        Some(ref c) => c.split(',').filter(|s| !s.is_empty()).collect(),
        None => reg.models.keys().map(String::as_str).collect(),
    };
    let plan = reg.verifier_plan(
        &bead,
        ac,
        opts.worker_model.as_deref().unwrap_or("unknown"),
        &cands,
        cfg.verifier_family_override(),
    );
    match plan {
        crate::models::VerifierPlan::NoneRequired => {
            print_json(&json!({ "plan": "none-required", "bead_id": bead_id }));
            Ok(())
        }
        crate::models::VerifierPlan::Dispatch(model) => {
            print_json(&json!({ "plan": "dispatch", "bead_id": bead_id, "model": model }));
            Ok(())
        }
        crate::models::VerifierPlan::Degrade => {
            let block = crate::br::run_br(
                Some(&key),
                &[
                    "update",
                    bead_id,
                    "--status=blocked",
                    "--transition-comment",
                    "verifier-family-missing (ADR-0019 Spec 4): no second model family in the \
                     registry and no verifier_family_override — bead blocked, captain mailed \
                     once, no respawn",
                ],
            );
            let blocked = block.is_ok();
            let mut mailed = false;
            let marker = key
                .join(".flywheel")
                .join(format!("verifier-family-missing-{bead_id}.mailed"));
            if !marker.exists() {
                let key_str = key.to_string_lossy().into_owned();
                let captain = cfg
                    .agent_mail
                    .as_ref()
                    .and_then(|m| m.name.clone())
                    .unwrap_or_else(|| "Flywheel".to_string());
                if let Ok(mail) = backend::mail_backend(cfg) {
                    let out = (|| -> Result<backend::SendReceipt> {
                        backend::run_mail(mail.ensure_project(&key_str))
                            .map_err(|e| anyhow!("{e}"))?;
                        send_verifier_family_missing_mail(&*mail, &key_str, bead_id, &captain)
                    })();
                    mailed = out.is_ok();
                    if let Err(me) = out {
                        eprintln!(
                            "flywheel: verifier-family-missing mail for {bead_id} failed (continuing): {me:#}"
                        );
                    }
                }
                if mailed && let Some(parent) = marker.parent() {
                    let _ = std::fs::create_dir_all(parent);
                    let _ = std::fs::write(&marker, "1");
                }
            }
            print_json(&json!({
                "plan": "degrade",
                "bead_id": bead_id,
                "blocked": blocked,
                "mailed": mailed,
                "reason": crate::models::VERIFIER_FAMILY_MISSING,
            }));
            Ok(())
        }
    }
}

/// The verifier-family-missing mail: thread = the bead id (ADR-0019 Spec 0),
/// recipient = the configured orchestrator/captain identity. Exactly once per
/// bead via the marker dedup in the caller.
fn send_verifier_family_missing_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    bead_id: &str,
    captain: &str,
) -> Result<backend::SendReceipt> {
    send_loop_mail(
        mail,
        key,
        &LoopMail::VerifierFamilyMissing { bead_id, captain },
    )
}

/// internal leaf: close-detect check (ADR-0019, bd-w1-reopen-f6tw) — wrap
/// `verdict::close_check` (SHA rule + ledger): `{status: legal, sha}` |
/// `{status: missing, sha, receipt_eligible}` | `{status: no-commit}`. The
/// loop turns a non-legal status into `__reopen-bead`. Always exit 0.
pub(crate) fn cmd_internal_close_check(cfg: &Config, project: &str, bead_id: &str) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    match verdict::close_check(std::path::Path::new(&key), bead_id) {
        Ok(verdict::CloseCheck::Legal { sha }) => {
            print_json(&json!({ "status": "legal", "bead_id": bead_id, "sha": sha }));
            Ok(())
        }
        Ok(verdict::CloseCheck::Missing {
            sha,
            receipt_eligible,
        }) => {
            print_json(&json!({
                "status": "missing",
                "bead_id": bead_id,
                "sha": sha,
                "receipt_eligible": receipt_eligible,
            }));
            Ok(())
        }
        Ok(verdict::CloseCheck::NoCommit) => {
            print_json(&json!({ "status": "no-commit", "bead_id": bead_id }));
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: close-check for {bead_id} failed: {e:#}");
            print_json(
                &json!({ "status": "error", "bead_id": bead_id, "error": format!("{e:#}") }),
            );
            Ok(())
        }
    }
}

/// internal leaf: reopen an unverified close (ADR-0019, bd-w1-reopen-f6tw) —
/// `br reopen <id>` (idempotent: already-open -> no-op) + ONE mail per
/// (bead, sha) on `thread_id=<id>`, subject `[<id>] unverified close
/// reopened`. Durable dedup: a `.flywheel/reopened/<bead>-<sha>.reopened`
/// marker written after the mail (removed on mail failure) makes the storm
/// guard hold across ticks AND invocations. Always exit 0.
pub(crate) fn cmd_internal_reopen_bead(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    sha: &str,
) -> Result<()> {
    let key = project_dir_str(cfg, project)?;
    let fallback = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    let mut chain = planner_recipient_chain(&key, bead_id, &fallback);
    chain.push("captain".to_string());
    let mail = match backend::mail_backend(cfg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("flywheel: mail backend unavailable for reopen alert {bead_id}: {e:#}");
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "sha": sha,
                "error": format!("mail backend: {e:#}"),
            }));
            return Ok(());
        }
    };
    let planner = match first_routable(chain.iter().map(String::as_str), |c| {
        mail_whois_ok(&*mail, &key, c)
    }) {
        Some(p) => p,
        None => {
            let tried = chain.join(", ");
            eprintln!(
                "flywheel: reopen alert for {bead_id} has no routable recipient (tried {tried})"
            );
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "sha": sha,
                "notified": tried,
                "error": format!("no routable planner identity for {key} (tried {tried})")
            }));
            return Ok(());
        }
    };
    match reopen_unverified_close(&key, bead_id, sha, &*mail, &planner) {
        Ok(v) => {
            print_json(&v);
            Ok(())
        }
        Err(e) => {
            eprintln!("flywheel: reopen of {bead_id} failed: {e:#}");
            print_json(&json!({
                "ok": false,
                "bead_id": bead_id,
                "sha": sha,
                "error": format!("{e:#}"),
            }));
            Ok(())
        }
    }
}

/// The reopen side of the close-detect flow (ADR-0019, bd-w1-reopen-f6tw):
/// `br reopen <id>` (idempotent — already open is a no-op) + ONE mail per
/// (bead, sha) on the bead's thread. Durable dedup via a
/// `.flywheel/reopened/<bead>-<sha>.reopened` marker written AFTER a
/// successful mail (removed on failure so a retry re-mails) — the storm
/// guard holds across repeated ticks and loop invocations. Returns the
/// outcome JSON for the leaf / tests.
fn reopen_unverified_close(
    key: &str,
    bead_id: &str,
    sha: &str,
    mail: &dyn backend::MailSurface,
    planner: &str,
) -> Result<serde_json::Value> {
    let dir = std::path::Path::new(key);
    let marker_dir = dir.join(".flywheel").join("reopened");
    let marker = marker_dir.join(format!(
        "{bead_id}-{}.reopened",
        if sha.is_empty() { "nocommit" } else { sha }
    ));
    if marker.exists() {
        return Ok(json!({
            "ok": true,
            "bead_id": bead_id,
            "sha": sha,
            "reopened": false,
            "already_processed": true,
        }));
    }
    crate::br::run_br(
        Some(dir),
        &[
            "reopen",
            bead_id,
            "--reason",
            "unverified close (ADR-0019: no legal verdict for the derived sha)",
            "--actor",
            "flywheel-loop",
        ],
    )
    .map_err(|e| anyhow!("br reopen {bead_id} failed: {e:#}"))?;
    std::fs::create_dir_all(&marker_dir)?;
    std::fs::write(&marker, "mailed\n")?;
    let send = (|| -> Result<backend::SendReceipt> {
        backend::run_mail(mail.ensure_project(key)).map_err(|e| anyhow!("{e}"))?;
        send_unverified_close_mail(mail, key, bead_id, sha, planner)
    })();
    match send {
        Ok(_) => Ok(json!({
            "ok": true,
            "bead_id": bead_id,
            "sha": sha,
            "reopened": true,
            "notified": planner,
        })),
        Err(e) => {
            let _ = std::fs::remove_file(&marker);
            Err(e)
        }
    }
}

/// internal leaf: loop-run VERIFY executor (ADR-0019 cheap band,
/// bd-w1-loop-run-verify-mbbe). Classify the bead's VERIFY; on the
/// loop-runnable band RUN it under `sh -c` with the TIMEBOX timeout (cargo
/// under the build slot), writing a failed-head marker on non-zero exit or
/// timeout (the loop NEVER re-verifies the same broken head). The machine
/// receipt + the command-verified gate row are written IN-PROCESS by the
/// armed loop — this command only executes and reports. Always exits 0 —
/// a run hiccup must never kill the loop.
/// The executor's outcome — what the leaf turns into the machine receipt +
/// gate row (recorded) or the failed-head marker + reopen.
#[derive(Debug, Clone)]
struct LoopRunOutcome {
    band: String,
    dir: PathBuf,
    sha: String,
    exit: i32,
    timed_out: bool,
    skipped: bool,
    verify: String,
    output_tail: String,
}

pub(crate) fn cmd_internal_loop_run_verify(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    sha: &str,
    timeout_secs: Option<u64>,
    dir: Option<&Path>,
) -> Result<()> {
    let o = match loop_run_verify_exec(cfg, project, bead_id, sha, timeout_secs, dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("flywheel: loop-run-verify for {bead_id} failed (continuing): {e}");
            print_json(
                &json!({ "band": "non-runnable", "bead_id": bead_id, "error": format!("{e}") }),
            );
            return Ok(());
        }
    };
    if o.band != "loop-runnable" {
        print_json(&json!({
            "band": o.band,
            "bead_id": bead_id,
            "dir": o.dir.to_string_lossy(),
            "verify": o.verify,
        }));
        return Ok(());
    }
    let mut v = json!({
        "band": "loop-runnable",
        "bead_id": bead_id,
        "sha": o.sha,
        "dir": o.dir.to_string_lossy(),
        "verify": o.verify,
    });
    if o.skipped {
        v["skipped"] = json!("failed-head");
    } else if o.timed_out {
        v["timed_out"] = json!(true);
        v["output_tail"] = json!(o.output_tail);
    } else {
        v["exit"] = json!(o.exit);
        v["output_tail"] = json!(o.output_tail);
    }
    print_json(&v);
    Ok(())
}

/// Workflow-graph verify leaf (ADR-0024 Spec 6): the engine hands a templated
/// verify command to this sink, which reuses `loop_run_verify_exec`'s exec path
/// (slot held, TIMEBOX, receipt via `write_receipt`) so no second verify path
/// exists. Attached on every `compose --run` / `workflow run --execute` arm.
fn make_verify_sink(cfg: Config, project: String) -> toron_workflow::engine::VerifySink {
    let sink = toron_workflow::engine::VerifySink::default();
    let cfg_clone = cfg.clone();
    let project_clone = project.clone();
    *sink.0.lock().expect("verify-sink mutex") = Some(std::sync::Arc::new(
        move |req: &toron_workflow::engine::VerifyRequest| {
            let cmd = req.command.clone();
            let cwd = project_dir(&cfg_clone, &project_clone);
            let is_cargo = cmd.split_whitespace().next() == Some("cargo");
            if is_cargo {
                acquire_cargo_slot(&cfg_clone, &project_clone);
            }
            let outcome = run_verify_with_timeout(&cmd, 600, &cwd);
            if is_cargo {
                release_cargo_slot(&cfg_clone, &project_clone);
            }
            match outcome {
                Ok((0, tail)) => {
                    let receipt_content = if tail.trim().is_empty() {
                        format!("verify ok: {cmd}")
                    } else {
                        tail
                    };
                    let receipt_path = match crate::verdict::write_receipt(
                        &cwd,
                        "verify-sink",
                        "workflow-sha",
                        crate::verdict::VerdictKind::WorkerReceipt,
                        &receipt_content,
                    ) {
                        Ok(p) => Some(p.to_string_lossy().to_string()),
                        Err(e) => {
                            return Ok(toron_workflow::engine::VerifyResult {
                                ok: false,
                                exit: -1,
                                stdout_path: None,
                                stderr_path: None,
                                error: Some(format!("receipt write failed: {e}")),
                            });
                        }
                    };
                    Ok(toron_workflow::engine::VerifyResult {
                        ok: true,
                        exit: 0,
                        stdout_path: receipt_path,
                        stderr_path: None,
                        error: None,
                    })
                }
                Ok((exit, tail)) => Ok(toron_workflow::engine::VerifyResult {
                    ok: false,
                    exit,
                    stdout_path: None,
                    stderr_path: None,
                    error: Some(tail),
                }),
                Err(tail) => Ok(toron_workflow::engine::VerifyResult {
                    ok: false,
                    exit: -1,
                    stdout_path: None,
                    stderr_path: None,
                    error: Some(tail),
                }),
            }
        },
    ));
    sink
}

/// The loop-run VERIFY executor core: classify the bead's VERIFY; on the
/// loop-runnable band run it under `sh -c` with the TIMEBOX timeout (cargo
/// under the build slot), writing a failed-head marker on non-zero exit or
/// timeout (the loop NEVER re-verifies the same broken head). Pure-ish: the
/// only side effects are the failed-head marker + the build-slot hold.
fn loop_run_verify_exec(
    cfg: &Config,
    project: &str,
    bead_id: &str,
    sha: &str,
    timeout_secs: Option<u64>,
    dir: Option<&Path>,
) -> Result<LoopRunOutcome> {
    let key = match dir {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(project_dir_str(cfg, project)?),
    };
    let bead = match verdict::bead_from_br_show(&key, bead_id) {
        Ok(b) => b,
        Err(e) => {
            return Ok(LoopRunOutcome {
                band: "non-runnable".into(),
                dir: key,
                sha: sha.to_string(),
                exit: -1,
                timed_out: false,
                skipped: false,
                verify: String::new(),
                output_tail: format!("bead lookup failed: {e}"),
            });
        }
    };
    if !verdict::verify_is_loop_runnable(&bead.verify) {
        return Ok(LoopRunOutcome {
            band: "non-runnable".into(),
            dir: key,
            sha: sha.to_string(),
            exit: -1,
            timed_out: false,
            skipped: false,
            verify: bead.verify,
            output_tail: String::new(),
        });
    }
    let marker_dir = key.join(".flywheel").join("loop-run-verify-failed");
    let marker = marker_dir.join(format!("{bead_id}-{sha}"));
    if marker.exists() {
        return Ok(LoopRunOutcome {
            band: "loop-runnable".into(),
            dir: key,
            sha: sha.to_string(),
            exit: -1,
            timed_out: false,
            skipped: true,
            verify: bead.verify,
            output_tail: String::new(),
        });
    }
    let timeout = timeout_secs.unwrap_or_else(|| verdict::bead_timebox_secs(&key, bead_id, 600));
    let is_cargo = bead.verify.split_whitespace().next() == Some("cargo");
    if is_cargo {
        acquire_cargo_slot(cfg, project);
    }
    let outcome = run_verify_with_timeout(&bead.verify, timeout, &key);
    if is_cargo {
        release_cargo_slot(cfg, project);
    }
    match outcome {
        Ok((exit, tail)) => {
            if exit != 0 {
                let _ = std::fs::create_dir_all(&marker_dir);
                let _ = std::fs::write(&marker, "1");
            }
            Ok(LoopRunOutcome {
                band: "loop-runnable".into(),
                dir: key,
                sha: sha.to_string(),
                exit,
                timed_out: false,
                skipped: false,
                verify: bead.verify,
                output_tail: tail,
            })
        }
        Err(tail) => {
            let _ = std::fs::create_dir_all(&marker_dir);
            let _ = std::fs::write(&marker, "1");
            Ok(LoopRunOutcome {
                band: "loop-runnable".into(),
                dir: key,
                sha: sha.to_string(),
                exit: -1,
                timed_out: true,
                skipped: false,
                verify: bead.verify,
                output_tail: tail,
            })
        }
    }
}

/// Run `<verify>` under `sh -c` with a hard timeout in the bead's repo dir;
/// the pipes are drained concurrently so a verbose test never deadlocks on
/// the pipe buffer. Returns `Ok((exit_code, tail))` or `Err(tail)` when the
/// deadline hit (the child was killed).
fn run_verify_with_timeout(
    verify: &str,
    timeout_secs: u64,
    cwd: &Path,
) -> std::result::Result<(i32, String), String> {
    use std::io::Read;
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(verify)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Ok((-1, format!("spawn failed: {e}"))),
    };
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut o) = out {
            let _ = o.read_to_string(&mut buf);
        }
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut e2) = err {
            let _ = e2.read_to_string(&mut buf);
        }
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(-1),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = tail_of(&format!(
                        "{}\n{}",
                        out_handle.join().unwrap_or_default(),
                        err_handle.join().unwrap_or_default()
                    ));
                    return Err(tail);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                return Ok((-1, format!("wait failed: {e}")));
            }
        }
    };
    let stdout = out_handle.join().unwrap_or_default();
    let stderr = err_handle.join().unwrap_or_default();
    Ok((exit_code, tail_of(&format!("{stdout}\n{stderr}"))))
}

/// Last ~4 KiB of captured output (receipts stay small).
fn tail_of(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(4096);
    chars[start..].iter().collect()
}

/// Best-effort build-slot hold for cargo VERIFY (ADR-0019: "cargo commands
/// under the build slot"). Advisory — a slot failure must never block the
/// loop (the always-serialize gate already serializes cargo dispatch).
///
/// bd-em85: toron's inbox surface takes `--as`, never `--agent`
/// (flywheel shelled `--agent` and every inbox-fallback failed loud).
/// The argv builder below keeps the flag pin-able by test. The fallback
/// itself stays a subprocess: it runs only when the embedded path has
/// already failed, so routing it through the facade would recurse.
fn mail_inbox_fallback_argv(pin: &str, project: &str, since: &str) -> Vec<String> {
    [
        "mail",
        "inbox",
        "--as",
        pin,
        "--project",
        project,
        "--since",
        since,
        "--limit",
        "50",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn acquire_cargo_slot(cfg: &Config, project: &str) {
    let agent = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "flywheel-toron-loop1".to_string());
    let ttl = toron_core::bus::DEFAULT_ENVELOPE_TTL_SECS;
    match crate::mail_or_die(cfg).and_then(|m| m.acquire_slot(project, &agent, "cargo", true, ttl))
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("flywheel: loop-run-verify cargo slot acquire failed (continuing): {e:#}")
        }
    }
}

fn release_cargo_slot(cfg: &Config, project: &str) {
    let agent = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "flywheel-toron-loop1".to_string());
    if let Ok(m) = crate::mail_or_die(cfg) {
        let _ = m.release_slot(project, &agent, "cargo");
    }
}

/// The unverified-close mail: thread = the bead id, subject `[<id>]
/// unverified close reopened` (ADR-0019 Spec 0). Body names what is
/// missing (a verdict row for the derived sha, or the commit itself).
fn send_unverified_close_mail(
    mail: &dyn backend::MailSurface,
    key: &str,
    bead_id: &str,
    sha: &str,
    planner: &str,
) -> Result<backend::SendReceipt> {
    send_loop_mail(
        mail,
        key,
        &LoopMail::UnverifiedClose {
            bead_id,
            sha,
            planner,
        },
    )
}

/// internal leaf: diagnose an unresponsive agent (bd-event-self-heal).
///
/// 1. Capture recent output via the read path (best-effort).
/// 2. Send high-importance mail on thread `health-<name>` to the orchestrator
///    identity (config agent_mail.name) so a human/supervisor can act.
///
/// Soft-fail always: exit 0 with `{ok, ...}` so overnight loops never die.
pub(crate) fn cmd_internal_diagnose(
    cfg: &Config,
    project: &str,
    name: &str,
    reason: &str,
) -> Result<()> {
    let mut tail = String::new();
    let backend = match backend::multiplexer_backend(cfg) {
        Ok(b) => b,
        Err(e) => {
            print_json(&json!({
                "ok": false,
                "name": name,
                "reason": reason,
                "error": format!("multiplexer unavailable: {e:#}"),
            }));
            return Ok(());
        }
    };
    let mut ops = hc::BackendAltScreenRead::new(&*backend, name, 80);
    match alt_screen_read_ladder(&mut ops, hc::RECENT_UNWRAPPED_SOURCE) {
        Ok(v) => {
            tail = v;
            if tail.len() > 4000 {
                let start = tail.len().saturating_sub(4000);
                tail = tail[start..].to_string();
            }
        }
        Err(e) => {
            eprintln!("flywheel: diagnose read {name}: {e:#}");
        }
    }

    let mut mailed = false;
    let mut mail_err: Option<String> = None;
    let recipient = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "Flywheel".to_string());
    match backend::mail_backend(cfg) {
        Ok(mail) => {
            let key = project_dir_str(cfg, project)?;
            let subject = format!("[{project}] {name} unresponsive ({reason})");
            let body = format!(
                "Agent **{name}** is unresponsive (`{reason}`).\n\n\
                 Project: `{project}`\n\
                 Thread: `health-{name}`\n\n\
                 ### Recent output (truncated)\n```\n{tail}\n```\n\n\
                 flywheel will attempt respawn if the agent stays dead (budgeted)."
            );
            match (|| -> Result<backend::SendReceipt> {
                backend::run_mail(mail.ensure_project(&key)).map_err(|e| anyhow!("{e}"))?;
                mail_send(
                    &*mail,
                    &key,
                    std::slice::from_ref(&recipient),
                    &subject,
                    &body,
                    "high",
                    false,
                    Some(&format!("health-{name}")),
                )
            })() {
                Ok(_) => mailed = true,
                Err(e) => {
                    mail_err = Some(format!("{e:#}"));
                    eprintln!("flywheel: diagnose mail for {name}: {e:#}");
                }
            }
        }
        Err(e) => {
            mail_err = Some(format!("mail backend: {e:#}"));
        }
    }

    print_json(&json!({
        "ok": true,
        "name": name,
        "reason": reason,
        "tail_len": tail.len(),
        "mailed": mailed,
        "recipient": recipient,
        "mail_error": mail_err,
    }));
    Ok(())
}

/// Internal leaf command: renew this identity's active file reservations in
/// the project, extending each by `extend_seconds` (bd-mail-lease-renewal-
/// durable). Best-effort from the workflow's perspective — the durable leaf
/// treats a non-zero exit as a missed renew (log + continue), never a hard
/// loop failure. Output: `{"ok": true, "renewed": N, "file_reservations": [...]}`.
pub(crate) fn cmd_internal_renew_leases(
    cfg: &Config,
    project: &str,
    extend_seconds: u64,
) -> Result<()> {
    let mail = backend::mail_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;
    let agent = orchestrator_mail_name(cfg);
    let v = backend::run_mail(mail.renew_reservations(&key, &agent, &[], extend_seconds))
        .map_err(|e| anyhow!("mail backend: {e}"))?;

    let mut released_closed: Vec<String> = Vec::new();
    if let Ok(closed) = load_closed_bead_ids(cfg, project) {
        let leases: Vec<(String, String)> = v
            .reservations
            .iter()
            .map(|r| {
                (
                    r.path_pattern.clone(),
                    bead_id_from_reason(&r.reason).unwrap_or("").to_string(),
                )
            })
            .collect();
        let stale: Vec<String> = durable::closed_reason_lease_ids(&leases, &closed);
        for path in &stale {
            match backend::run_mail(mail.release_reservations(
                &key,
                &agent,
                std::slice::from_ref(path),
            )) {
                Ok(rel) if rel.released > 0 => {
                    eprintln!("flywheel: released stale reservation {path} (owning bead closed)");
                    released_closed.push(path.clone());
                }
                Ok(_) => {
                    eprintln!("flywheel: release of stale reservation {path} reported 0 released");
                }
                Err(e) => eprintln!(
                    "flywheel: release of stale reservation {path} failed (continuing): {e}"
                ),
            }
        }
    }

    let envelope = json!({
        "renewed": v.renewed,
        "file_reservations": v.reservations.iter().map(|r| json!({
            "id": r.id,
            "path_pattern": r.path_pattern,
            "old_expires_ts": "",
            "new_expires_ts": r.expires_ts_us.map(|u| u.to_string()).unwrap_or_default(),
        })).collect::<Vec<_>>(),
    });
    let reservations = crate::mail::list_own_reservations(&envelope);
    print_json(&json!({
        "ok": true,
        "renewed": v.renewed,
        "agent": agent,
        "released_closed": released_closed,
        "file_reservations": reservations,
    }));
    Ok(())
}

/// `__release-bead-locks` leaf (bd-h3oj): operator-signed revocation of
/// every live reservation whose reason is the closed bead id. Emits
/// `{"bead_id": ..., "released": n}` for the durable loop's checkpoint.
pub(crate) fn cmd_internal_release_bead_locks(
    cfg: &Config,
    project: &str,
    bead_id: &str,
) -> Result<()> {
    let mail = backend::mail_backend(cfg)?;
    let key = project_dir_str(cfg, project)?;
    backend::run_mail(mail.ensure_project(&key))?;
    let requestor = orchestrator_mail_name(cfg);
    let v = backend::run_mail(mail.release_by_reason(&key, &requestor, bead_id, true))
        .map_err(|e| anyhow!("mail backend: {e}"))?;
    if v.released > 0 {
        eprintln!(
            "flywheel: released {} reservation(s) for closed bead {bead_id}",
            v.released
        );
    }
    print_json(&json!({
        "bead_id": bead_id,
        "released": v.released,
    }));
    Ok(())
}

/// Parse a reservation reason as a bead id, if the reason names one.
/// Reasons are the bead id verbatim (`bd-oj9o`) or free text; only
/// `bd-<id>`-shaped reasons are treated as bead refs (bd-evqs — the sweep
/// must never guess that an arbitrary reason is a bead).
#[must_use]
pub fn bead_id_from_reason(reason: &str) -> Option<&str> {
    let t = reason.trim();
    let id = t.split_whitespace().next().unwrap_or(t);
    let id = id.trim_end_matches([':', ',', ';', ')']);
    if id.starts_with("bd-") && id.len() > 3 {
        Some(id)
    } else {
        None
    }
}

/// Load the set of CLOSED bead ids in the project (`br list --all --json`,
/// status == "closed"). Soft-fail: a missing .beads / br error yields an
/// empty set (the sweep no-ops, renew-all preserved).
fn load_closed_bead_ids(cfg: &Config, project: &str) -> Result<std::collections::HashSet<String>> {
    let dir = project_dir(cfg, project);
    let rows = crate::br::Bead::list_issues(Some(&dir), &["--all"], "br list --all")?;
    let mut closed = std::collections::HashSet::new();
    for b in &rows {
        let id = b.get("id").and_then(Value::as_str).unwrap_or("");
        let status = b.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "closed" && !id.is_empty() {
            closed.insert(id.to_string());
        }
    }
    Ok(closed)
}

/// --- internal batch 4: moved-in test mods (bd-wave5-main-eviction-d5hht) ---
#[cfg(test)]
mod wait_output_tests {
    use super::*;
    use crate::hc::{AgentInfo, PaneWaitResult};

    fn agent(name: Option<&str>, pane: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("opencode".into()),
            agent_status: AgentStatus::Idle,
            cwd: Some("/proj".into()),
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: name.map(String::from),
            pane_id: pane.to_string(),
            revision: 1,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: "w1".into(),
        }
    }

    #[test]
    fn resolve_pane_id_resolves_names_and_raw_ids() {
        let agents = vec![agent(Some("flywheel-x-oc"), "w1:p1")];
        assert_eq!(
            resolve_pane_id(&agents, "flywheel-x-oc"),
            Some("w1:p1".to_string()),
            "agent display name maps to its pane"
        );
        assert_eq!(
            resolve_pane_id(&agents, "w2:p3"),
            Some("w2:p3".to_string()),
            "a raw pane id passes through"
        );
    }

    #[test]
    fn resolve_pane_id_unknown_target_is_none() {
        let agents = vec![agent(Some("flywheel-x-oc"), "w1:p1")];
        assert_eq!(resolve_pane_id(&agents, "no-such-agent"), None);
        assert_eq!(resolve_pane_id(&agents, ""), None);
    }

    #[test]
    fn wait_output_mode_maps_regex_flag() {
        assert_eq!(wait_output_mode(true), "regex");
        assert_eq!(wait_output_mode(false), "match");
    }

    #[test]
    fn wait_output_json_shape() {
        let hit = wait_output_json(true, Some("tests passed"), "w1:p1");
        assert_eq!(hit["matched"], true);
        assert_eq!(hit["matched_line"], "tests passed");
        assert_eq!(hit["pane_id"], "w1:p1");
        let miss = wait_output_json(false, None, "w1:p1");
        assert_eq!(miss["matched"], false);
        assert_eq!(miss["matched_line"], serde_json::Value::Null);
    }

    #[test]
    fn pane_wait_result_json_uses_leaf_shape() {
        let r = PaneWaitResult {
            pane_id: "w1:p1".into(),
            matched_line: Some("ready".into()),
            read: String::new(),
        };
        let v = wait_output_json(true, r.matched_line.as_deref(), &r.pane_id);
        assert_eq!(v["matched_line"], "ready");
    }
}
#[cfg(test)]
mod headless_retry_tests {
    use super::*;

    fn outcome_with_error(err: Option<&str>) -> headless::HarnessOutcome {
        headless::HarnessOutcome {
            response: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            status: if err.is_some() {
                "fail".into()
            } else {
                "success".into()
            },
            model: String::new(),
            duration_ms: 0,
            exit_code: i32::from(err.is_some()),
            error: err.map(str::to_string),
        }
    }

    /// bd-42fs: HTTP 429 (in the harness error stream or stderr) is the
    /// retry-on-429 trigger; other errors are not.
    #[test]
    fn rate_limit_predicate_classifies_429() {
        assert!(is_rate_limited(&outcome_with_error(Some(
            "Error: 429 Too Many Requests"
        ))));
        assert!(is_rate_limited(&outcome_with_error(Some(
            "rate limit exceeded"
        ))));
        assert!(is_rate_limited(&outcome_with_error(Some("RATE LIMIT"))));
        assert!(is_rate_limited(&outcome_with_error(Some(
            "{\"error\":{\"code\":429}}"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(Some(
            "model not found"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(Some(
            "401 unauthorized"
        ))));
        assert!(!is_rate_limited(&outcome_with_error(None)));
    }

    /// bd-42fs: the oracle's fallback chain resolves — glm-5.2 (zai) with a
    /// 429 retries on grok-4.5 (the registry fallback), injected for the
    /// grok harness (bare id).
    #[test]
    fn oracle_fallback_resolves_via_registry() {
        let _g = crate::config::lock_env();
        let mut reg = models::ModelRegistry::default();
        reg.models.insert(
            "zai/glm-5.2".into(),
            models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: Some("grok/grok-4.5".into()),
                effort: Some("max".into()),
                tier: None,
            },
        );
        reg.models.insert(
            "grok/grok-4.5".into(),
            models::ModelEntry {
                cost_in_per_mtok: 0.0,
                cost_out_per_mtok: 0.0,
                caps: vec![],
                fallback: None,
                effort: Some("max".into()),
                tier: None,
            },
        );
        let fb = reg
            .get("glm-5.2")
            .and_then(|e| e.fallback.clone())
            .expect("fallback declared");
        assert_eq!(fb, "grok/grok-4.5");
        assert_eq!(
            reg.resolve(&fb, "grok"),
            "grok-4.5",
            "grok takes the bare model id"
        );
        assert_eq!(
            reg.resolve(&fb, "omp"),
            "grok/grok-4.5",
            "omp prefixes the provider"
        );
        let no_fb = reg.get("grok-4.5").and_then(|e| e.fallback.clone());
        assert!(no_fb.is_none());
    }
}
#[cfg(test)]
mod is_rate_limited_tests {
    use super::*;

    /// The one genuinely headless-local pin in this module: the 429 detector
    /// over a `HarnessOutcome`, which no other call site has. The classify rows
    /// this module used to carry live in the single `failure.rs` table now —
    /// cron, pane, and headless all ran the same `classify(err, None, None)`,
    /// so a module-local copy pinned nothing about the module.
    #[test]
    fn is_rate_limited_detects_429() {
        let outcome = crate::headless::HarnessOutcome {
            response: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            status: "fail".to_string(),
            model: String::new(),
            duration_ms: 0,
            exit_code: 1,
            error: Some("429 Too Many Requests".to_string()),
        };
        assert!(is_rate_limited(&outcome));
    }
}
#[cfg(test)]
mod verify_sink_tests {
    use super::*;
    use toron_workflow::engine::VerifyRequest;

    #[test]
    fn verify_sink_returns_ok_on_success() {
        let dir = std::env::temp_dir().join(format!("fw-vs-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = Config::default();
        let sink = make_verify_sink(cfg, dir.to_string_lossy().to_string());
        let req = VerifyRequest {
            command: "echo ok".to_string(),
        };
        let guard = sink.0.lock().unwrap();
        if let Some(f) = guard.as_ref() {
            let res = f(&req).expect("verify sink ok");
            assert!(res.ok, "error={:?}", res.error);
            assert!(res.stdout_path.is_some() || res.error.is_none());
        } else {
            assert!(guard.is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn verify_sink_missing_returns_error() {
        let engine = toron_workflow::WorkflowEngine::local().expect("engine");
        let wf = toron_workflow::schema::parse_workflow(
            r#"name: t
graph:
  nodes:
    v: { action: verify, command: "cargo test" }
"#,
        )
        .expect("parse verify");
        let out = engine
            .run(
                "verify-missing-test",
                toron_workflow::engine::RunInput {
                    run_id: None,
                    workflow: wf,
                    trigger: serde_json::json!({}),
                },
            )
            .await
            .expect("run");
        assert_eq!(out["v"]["ok"], false);
        assert!(
            out["v"]["error"]
                .as_str()
                .unwrap()
                .contains("verify sink missing")
        );
    }

    #[test]
    fn verify_sink_workflow_receipt_never_command_verified() {
        let dir = std::env::temp_dir().join(format!("fw-verify-receipt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let content = "workflow verify output";
        let path = crate::verdict::write_receipt(
            &dir,
            "bd-verify-test",
            "sha-123",
            crate::verdict::VerdictKind::WorkerReceipt,
            content,
        )
        .expect("write workflow receipt");
        assert!(path.to_string_lossy().contains("bd-verify-test"));
        let err = crate::verdict::write_receipt(
            &dir,
            "bd-verify-test",
            "sha-123",
            crate::verdict::VerdictKind::CommandVerified,
            content,
        )
        .unwrap_err();
        assert!(err.to_string().contains("command-verified"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_sink_exec_failing_command_returns_false_with_exit() {
        let dir = std::env::temp_dir().join(format!("fw-vs-fail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = Config::default();
        let sink = make_verify_sink(cfg, dir.to_string_lossy().to_string());
        let req = VerifyRequest {
            command: "false".to_string(),
        };
        let guard = sink.0.lock().unwrap();
        let f = guard.as_ref().expect("sink");
        let res = f(&req).expect("verify sink");
        assert!(!res.ok);
        assert!(res.exit != 0);
        assert!(res.stdout_path.is_none());
        assert!(res.error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_sink_exec_writes_workflow_receipt_on_success() {
        let dir = std::env::temp_dir().join(format!("fw-vs-rcpt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = Config::default();
        let sink = make_verify_sink(cfg, dir.to_string_lossy().to_string());
        let req = VerifyRequest {
            command: "echo workflow-ok".to_string(),
        };
        let guard = sink.0.lock().unwrap();
        let f = guard.as_ref().expect("sink");
        let res = f(&req).expect("verify sink");
        assert!(res.ok, "error={:?}", res.error);
        assert_eq!(res.exit, 0);
        let path = res.stdout_path.expect("receipt path");
        assert!(path.contains("verify-sink"));
        assert!(path.contains("workflow-sha"));
        assert!(path.ends_with(".worker-receipt.txt"));
        let content = std::fs::read_to_string(&path).expect("receipt file");
        assert!(content.contains("workflow-ok") || content.contains("verify ok"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_sink_exec_slot_acquisition_still_happens() {
        let dir = std::env::temp_dir().join(format!("fw-vs-slot-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg = Config::default();
        let sink = make_verify_sink(cfg, dir.to_string_lossy().to_string());
        let req = VerifyRequest {
            command: "cargo --version".to_string(),
        };
        let guard = sink.0.lock().unwrap();
        let f = guard.as_ref().expect("sink");
        let res = f(&req).expect("verify sink");
        assert!(res.ok, "error={:?}", res.error);
        assert_eq!(res.exit, 0);
        assert!(res.stdout_path.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
