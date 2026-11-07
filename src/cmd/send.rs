//! `flywheel send` + delivery helpers (split from `main.rs`, bd-wave5).
//! One home for the send verbs, the --wait stall ladder, and the send
//! row/receipt helpers the verbs share.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use toron_plane::backend::{BackendError, error::BackendErrorCode};

use crate::cmd::doorbell::doorbell_note_observed_wake;
use crate::cmd::internal::skill_injection_prefix;
use crate::config::Config;
#[cfg(test)]
use crate::hc::SendVerdict;
use crate::hc::{AgentInfo, AgentStatus, Client, WaitOptions};
use crate::output::{dim, print_json, status_paint};
use crate::{
    agent_ref, backend, comms_send, durable, failure, prime_line_via_send_text, profile, state,
};

/// `flywheel send --via-inbox` (ADR-0041, build-plan §5): the brief goes to
/// each recipient's comms inbox as ONE `task@1` envelope through toron's
/// `mail.append` effect; the pane receives only the constant wake line. No
/// typing, no stall resend, no respawn: the envelope is the delivery.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_send_via_inbox(
    cfg: &Config,
    project: &str,
    msg: Option<&str>,
    to: &str,
    stage: Option<&str>,
    params: &[String],
    force: bool,
    from: Option<&str>,
    legacy_flags: (bool, bool, bool, bool, bool, bool, Option<u64>),
) -> Result<()> {
    if !comms_send::enabled(cfg) {
        return Err(comms_send::SendError::Disabled.into());
    }
    let (wait, respawn, raw, ff, plane, unsafe_plane, timeout) = legacy_flags;
    if wait || respawn || raw || ff || plane || unsafe_plane || timeout.is_some() {
        bail!(
            "--via-inbox delivers through the inbox: --wait/--timeout-ms/--respawn-on-stall/--raw/--fire-and-forget/--plane/--unsafe-plane do not apply (nothing sent)"
        );
    }
    let text = match (stage, msg) {
        (Some(s), m) => crate::combine_stage_message(
            crate::stage_prompt_with_params_project(s, params, &crate::project_dir(cfg, project))?,
            m,
        ),
        (None, Some(m)) => m.to_string(),
        (None, None) => bail!("provide --stage NAME or a message text"),
    };
    let param = |k: &str| {
        params
            .iter()
            .find_map(|p| p.strip_prefix(&format!("{k}=")).map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    let task_id = param("task_id").unwrap_or_else(mint_task_id);
    let bead = param("bead");
    let scope: Vec<String> = param("scope")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !force {
        let policy = crate::config::Policy::load()?;
        if let Some(p) = policy.violation(&text) {
            bail!(
                "refused by policy guard (pattern: {p:?}). Pass --force to override, or edit policy: flywheel policy remove"
            );
        }
    }
    let task = comms_send::task_from_text(&text, bead.as_deref(), scope);
    let rows = comms_send::send(
        cfg,
        comms_send::Request {
            project,
            from,
            to,
            task_id: &task_id,
            task,
        },
    )?;
    crate::log(
        project,
        state::EventKind::Send,
        json!({ "to": to, "via": "inbox", "task_id": task_id, "results": rows }),
    );
    print_json(&comms_send::rows_json(&rows));
    if rows
        .iter()
        .any(|r| r.state != "appended" && r.state != "already-appended")
    {
        bail!(
            "comms send: not every envelope settled (see rows). Never resent blind: rerun with --param task_id={task_id} to reconcile the same msg ids"
        );
    }
    Ok(())
}

/// bd-rex-default-plane-xn1t: the default send path on a non-hc plane.
/// The caller (`cmd_send`) runs the shared prep (stage text, task id, policy
/// guard, plane tripwires) and passes the rendered text in. This arm then:
/// resolve through the selected backend's `agent_list` (shared exact rule +
/// registry), dispatch through `backend.prompt`, and `--wait` through
/// `backend.prompt_coded` (agent_wait). A semantic reject stays a reject —
/// no hc retry exists on this arm, and nothing here mentions hc: with
/// a dead socket and a rex selection, every failure names the rex plane.
/// Rows keep the `prior_status`/`landed` JSON fields. Exit 0 means every
/// prompt was accepted (or, with `--wait`, settled); anything else is loud.
/// Divergence from the hc arm, by acceptance contract: no P0-A fail-loud
/// gate and no doorbell last_wake gate here — a prompt-ok row is the dispatch
/// receipt, the same contract as `--via-inbox` `queued`/`woke`.
#[cfg(feature = "backend-superlogical")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_send_rex(
    cfg: &Config,
    project: &str,
    to: &str,
    text: &str,
    stage: Option<&str>,
    wait: bool,
    timeout_ms: Option<u64>,
    force: bool,
    raw: bool,
    fire_and_forget: Option<&str>,
) -> Result<()> {
    let backend = backend::multiplexer_backend(cfg)?;
    let workspaces = crate::find_workspaces_dyn(&*backend, project)?;
    let ws_ids: Vec<&str> = workspaces.iter().map(|w| w.workspace_id.as_str()).collect();
    let agents = backend.agent_list()?;
    let in_ws: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| ws_ids.contains(&a.workspace_id.as_str()))
        .collect();
    if in_ws.is_empty() {
        let labels: Vec<&str> = workspaces.iter().map(|w| w.label.as_str()).collect();
        bail!("no agents in {labels:?} — is the swarm spawned?");
    }
    let mut registry = state::load_spawn_registry(project);
    let targets = crate::resolve_send_targets_dyn(&in_ws, &registry, project, to)?;
    if !force {
        let names: Vec<String> = targets
            .iter()
            .map(|a| crate::agent_target(a).to_string())
            .collect();
        if let Some(group) = bare_short_group_matches(project, to, &names) {
            bail!(
                "--to {to} matches {} agents ({}): a bare short reaches the whole crew and would race one prompt across all of them. Use an exact name or numbered short (e.g. flywheel-{}-oc, oc2) to target one agent, or pass --force to broadcast deliberately.",
                group.len(),
                group.join(", "),
                crate::project_base(project)
            );
        }
    }
    let timeout = timeout_ms.or_else(|| wait.then(|| cfg.timeout_ms()));
    if wait {
        eprintln!(
            "flywheel: --wait: expecting settle (idle|done|blocked), waiting up to {}s…",
            timeout.unwrap_or_default() / 1000
        );
    }
    let catalog = profile::ProfileCatalog::load();
    let mut registry_dirty = false;
    let mut results: Vec<Value> = Vec::new();
    for a in &targets {
        let prior = a.agent_status.as_str().to_string();
        let target = a.pane_id.clone();
        let name = a.name.clone().unwrap_or_else(|| target.clone());
        let entry = registry.iter_mut().find(|e| {
            e.mail_identity.as_deref() == Some(name.as_str())
                || e.pane_id == target
                || e.name == name
        });
        let hosted =
            agent_ref::binding_location_for(project, &name) == agent_ref::BindingLocation::Hosted;
        let (send_text, cond_profile, skills_injected) = match &catalog {
            Ok(c) => condition_send_target(entry, c, text, raw),
            Err(e) => {
                eprintln!("flywheel: profile catalog unavailable ({e:#}) — sending raw");
                (text.to_string(), None, false)
            }
        };
        if skills_injected {
            registry_dirty = true;
        }
        if text.starts_with("GOAL ") && send_text.len() > durable::DISPATCH_PROMPT_CAP {
            bail!(
                "flywheel: dispatched prompt for {} is {} bytes — over the {} byte cap (send conditioning; NOT sent)",
                durable::bead_id_from_prompt(text).unwrap_or("<unknown>"),
                send_text.len(),
                durable::DISPATCH_PROMPT_CAP,
            );
        }
        if wait {
            let wait_opts = WaitOptions {
                timeout_ms: timeout,
                until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
            };
            match backend.prompt_coded(&target, &send_text, Some(&wait_opts)) {
                Ok(landing) => {
                    let settled: Option<String> = landing.status.map(|s| s.as_str().to_string());
                    results.push(send_result_row(
                        crate::agent_target(a),
                        &prior,
                        &a.pane_id,
                        cond_profile.as_deref(),
                        settled.as_deref(),
                        hosted,
                    ));
                }
                Err(e) => {
                    results.push(wait_failure_row(
                        crate::agent_target(a),
                        &prior,
                        &a.pane_id,
                        cond_profile.as_deref(),
                        None,
                    ));
                    crate::log(
                        project,
                        state::EventKind::Send,
                        json!({
                            "to": to, "stage": stage, "text_len": text.len(), "wait": wait,
                            "raw": raw, "fire_and_forget": fire_and_forget, "results": results
                        }),
                    );
                    if !std::io::stdout().is_terminal() {
                        print_json(&json!({ "results": results }));
                    }
                    return Err(wait_failure_error(
                        crate::agent_target(a),
                        &e,
                        None,
                        project,
                    ));
                }
            }
        } else if let Err(e) = backend.prompt(&target, &send_text, None) {
            let err = format!("{e}");
            // Steering friction observed live: a working pane refuses the
            // prompt with no next step in the message. Name the queued door
            // (mail rings on idle) and the cancel key instead of dead-ending.
            if err.contains("not at a prompt") {
                bail!(
                    "{err}\n  hint: the pane is working — queue it with \
                     `flywheel mail send {project} --to <pin> --wake tasked --subject S --body B` \
                     (rings when idle), or cancel the tool call with `flywheel interrupt`."
                );
            }
            return Err(e.into());
        } else {
            results.push(send_result_row(
                crate::agent_target(a),
                &prior,
                &a.pane_id,
                cond_profile.as_deref(),
                None,
                hosted,
            ));
        }
        if std::io::stdout().is_terminal() {
            let shown = results
                .last()
                .and_then(|r| r.get("settled_status").and_then(|v| v.as_str()))
                .unwrap_or(&prior);
            println!(
                "{} {}",
                dim(crate::agent_target(a)),
                status_paint(crate::hc::parse_agent_status(shown).unwrap_or(AgentStatus::Unknown))
            );
        }
    }
    if registry_dirty && let Err(e) = state::save_spawn_registry(project, &registry) {
        eprintln!("flywheel: cannot persist skills_injected for send targets: {e:#}");
    }
    crate::log(
        project,
        state::EventKind::Send,
        json!({
            "to": to, "stage": stage, "text_len": text.len(), "wait": wait,
            "raw": raw, "fire_and_forget": fire_and_forget, "results": results
        }),
    );
    if !std::io::stdout().is_terminal() {
        print_json(&json!({ "results": results }));
    }
    if wait
        && !results
            .iter()
            .any(|r| r.get("landed") == Some(&json!(true)))
    {
        bail!(
            "send --wait produced no landed:true receipt — refuse invent-green exit 0. Re-run when the agent can settle."
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_send(
    cfg: &Config,
    project: &str,
    msg: Option<&str>,
    to: &str,
    stage: Option<&str>,
    params: &[String],
    wait: bool,
    timeout_ms: Option<u64>,
    force: bool,
    respawn_on_stall: bool,
    raw: bool,
    fire_and_forget: Option<String>,
    plane: Option<&str>,
    unsafe_plane: bool,
) -> Result<()> {
    let text = match (stage, msg) {
        (Some(s), m) => crate::combine_stage_message(
            crate::stage_prompt_with_params_project(s, params, &crate::project_dir(cfg, project))?,
            m,
        ),
        (None, Some(m)) => m.to_string(),
        (None, None) => bail!("provide --stage NAME or a message text"),
    };
    let task_id = match params.iter().find(|p| p.starts_with("task_id=")) {
        Some(kv) => kv.trim_start_matches("task_id=").to_string(),
        None => mint_task_id(),
    };
    let bead = params
        .iter()
        .find(|p| p.starts_with("bead="))
        .map(|kv| kv.trim_start_matches("bead=").to_string())
        .filter(|b| !b.is_empty());
    let text = format!(
        "{}\n\n{text}",
        crate::dispatch_header(&task_id, bead.as_deref(), stage)
    );

    // safety guard: refuse destructive prompts unless --force
    if !force {
        let policy = crate::config::Policy::load()?;
        if let Some(p) = policy.violation(&text) {
            bail!(
                "refused by policy guard (pattern: {p:?}). Pass --force to override, or edit policy: flywheel policy remove"
            );
        }
    }

    crate::check_send_plane_tripwires(&text, plane, unsafe_plane, raw)?;

    #[cfg(feature = "backend-superlogical")]
    if backend::resolve_plane(cfg).plane != backend::Plane::Hc {
        return cmd_send_rex(
            cfg,
            project,
            to,
            &text,
            stage,
            wait,
            timeout_ms,
            force,
            raw,
            fire_and_forget.as_deref(),
        );
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, project)?;
    let mut targets = crate::resolve_agents(&mut c, &ws_id, project, to);
    if targets.is_err() {
        let backend = backend::multiplexer_backend(cfg)?;
        for tok in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let _ = crate::rebind_from_registry(&*backend, project, tok, &ws_id);
        }
        if let Ok(live) = c.agent_list() {
            let entries = state::load_spawn_registry(project);
            let mut rebound = false;
            for a in live
                .iter()
                .filter(|a| a.workspace_id == ws_id && a.name.is_none())
            {
                if let Some(e) = entries.iter().find(|e| e.pane_id == a.pane_id)
                    && c.agent_rename(&a.pane_id, &crate::terminal_agent_name(&e.name))
                        .is_ok()
                {
                    rebound = true;
                }
            }
            if rebound {
                std::thread::sleep(Duration::from_millis(400));
            }
        }
        if targets.is_err() {
            eprintln!(
                "flywheel: send --to {to}: unresolvable after rebind — settling for a fresh registration"
            );
            targets = crate::settle_retry(3, 2000, || {
                crate::resolve_agents(&mut c, &ws_id, project, to)
            });
        }
    }
    let targets = targets?;

    if !force {
        let names: Vec<String> = targets
            .iter()
            .map(|a| crate::agent_target(a).to_string())
            .collect();
        if let Some(group) = bare_short_group_matches(project, to, &names) {
            bail!(
                "--to {to} matches {} agents ({}): a bare short reaches the whole crew and would race one prompt across all of them. Use an exact name or numbered short (e.g. flywheel-{}-oc, oc2) to target one agent, or pass --force to broadcast deliberately.",
                group.len(),
                group.join(", "),
                crate::project_base(project)
            );
        }
    }

    let timeout = timeout_ms.or_else(|| wait.then(|| cfg.timeout_ms()));
    if wait {
        eprintln!(
            "flywheel: --wait: expecting settle (idle|done|blocked), waiting up to {}s…",
            timeout.unwrap_or_default() / 1000
        );
    }
    let mut registry = state::load_spawn_registry(project);
    let catalog = profile::ProfileCatalog::load();
    let mut registry_dirty = false;
    let mut results: Vec<Value> = Vec::new();
    let emit_rows = |results: &[Value]| {
        crate::log(
            project,
            state::EventKind::Send,
            json!({
                "to": to, "stage": stage, "text_len": text.len(), "wait": wait,
                "raw": raw, "fire_and_forget": fire_and_forget, "delivery_id": delivery_id_of(&fire_and_forget), "results": results
            }),
        );
        if !std::io::stdout().is_terminal() {
            print_json(&json!({ "results": results }));
        }
    };
    let wake_before_unix = if wait {
        let ws_label =
            state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
        crate::doorbell::load_wake_state(&crate::doorbell::wake_state_file(
            &crate::config::state_dir(),
            &ws_label,
        ))
        .last_wake_unix
    } else {
        None
    };
    for a in &targets {
        let wait_opts = wait.then(|| WaitOptions {
            timeout_ms: timeout,
            until: vec![AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked],
        });
        // Prompt by pane address: hc's AgentInfo.name is a MAIL name stamped on
        // labels, not the terminal name, so `terminal_get(<name>)` says
        // "no terminal" (live hc send failure). The address is always valid.
        let mut target = a.pane_id.clone();
        let name = a.name.clone().unwrap_or_else(|| target.clone());
        let hosted =
            agent_ref::binding_location_for(project, &name) == agent_ref::BindingLocation::Hosted;
        let entry = registry.iter_mut().find(|e| e.name == name);
        let (send_text, cond_profile, skills_injected) = match &catalog {
            Ok(c) => condition_send_target(entry, c, &text, raw),
            Err(e) => {
                eprintln!("flywheel: profile catalog unavailable ({e:#}) — sending raw");
                (text.clone(), None, false)
            }
        };
        if skills_injected {
            registry_dirty = true;
        }
        if text.starts_with("GOAL ") && send_text.len() > durable::DISPATCH_PROMPT_CAP {
            bail!(
                "flywheel: dispatched prompt for {} is {} bytes — over the {} byte cap (send conditioning; NOT sent)",
                durable::bead_id_from_prompt(&text).unwrap_or("<unknown>"),
                send_text.len(),
                durable::DISPATCH_PROMPT_CAP,
            );
        }
        let mut stalls_seen = 0usize;
        let baseline_seq = a.state_change_seq;
        let baseline_completion = a.completion_seq.unwrap_or(0);
        let delivery_key = delivery_id_of(&fire_and_forget)
            .map(str::to_string)
            .unwrap_or_else(|| format!("send:{name}:{baseline_seq}"));
        let mut pane_retried = false;
        let agent = loop {
            match c.agent_prompt(&target, &send_text, wait_opts.clone()) {
                Ok(agent) => break agent,
                Err(e) if is_agent_not_ready(&e.code) => {
                    match not_ready_retry_target(&target, a, pane_retried) {
                        Some(pane) => {
                            pane_retried = true;
                            eprintln!(
                                "flywheel: {target} agent_not_ready — retrying via pane id {pane}"
                            );
                            target = pane;
                        }
                        None => {
                            if try_send_text_delivery(&mut c, project, &name, &target, &send_text) {
                                if wait {
                                    bail!(
                                        "send --wait: recipe-harness pane delivery for {name} succeeded but no settled wait observation — refuse invent-green landed (GS-037 prompt-wait-invent-landed)"
                                    );
                                }
                                if let Some(agent) = c.agent_list().ok().and_then(|list| {
                                    list.iter()
                                        .find(|ag| {
                                            ag.pane_id == target
                                                || ag.name.as_deref() == Some(name.as_str())
                                        })
                                        .cloned()
                                }) {
                                    break agent;
                                }
                            }
                            return Err(e.into());
                        }
                    }
                }
                Err(e)
                    if matches!(&e.code, BackendErrorCode::Other(c) if c == "agent_not_found")
                        && !pane_retried
                        && !target.contains(':') =>
                {
                    let entry = state::load_spawn_registry(project)
                        .iter()
                        .find(|en| en.name == name)
                        .cloned();
                    match entry {
                        Some(en) => {
                            let _ = c.agent_rename(&en.pane_id, &crate::terminal_agent_name(&name));
                            std::thread::sleep(Duration::from_millis(400));
                            pane_retried = true;
                            eprintln!(
                                "flywheel: {target} agent_not_found — rebound from registry, retrying via {}",
                                en.pane_id
                            );
                            target = en.pane_id;
                        }
                        None => return Err(e.into()),
                    }
                }
                Err(e) => {
                    let cf = failure::classify(&e.to_string(), None, None);
                    if cf.reroute_eligible
                        || cf.kind == failure::FailureKind::RateLimit
                        || cf.kind == failure::FailureKind::Timeout
                    {
                        eprintln!(
                            "flywheel: send classified kind={} evidence={}",
                            cf.kind, cf.evidence
                        );
                        if let Some(profile) = cond_profile.as_deref() {
                            crate::record_reroute_audit(project, 0, profile, cf.kind, &cf.evidence);
                        }
                    }
                    match stall_step(&e.code, stalls_seen, respawn_on_stall) {
                        None => {
                            if wait && e.code == BackendErrorCode::Timeout {
                                return wait_failure_exit(
                                    &mut c,
                                    &target,
                                    &name,
                                    a,
                                    cond_profile.as_deref(),
                                    project,
                                    &mut results,
                                    &e,
                                    &emit_rows,
                                );
                            }
                            return Err(e.into());
                        }
                        Some(StallStep::Retry) => {
                            let probe = probe_prompt_after_stall(
                                &mut c,
                                &target,
                                &send_text,
                                baseline_seq,
                                baseline_completion,
                            );
                            stalls_seen += 1;
                            if stall_may_resend(probe) {
                                eprintln!(
                                    "flywheel: {target} agent_prompt_stalled — prompt proven absent (delivery_key={delivery_key}); one resend"
                                );
                                std::thread::sleep(Duration::from_millis(200));
                            } else {
                                eprintln!(
                                    "flywheel: {target} agent_prompt_stalled — refusing blind resubmit (probe={probe:?}, delivery_key={delivery_key}; read-before-retry)"
                                );
                                if wait {
                                    return wait_failure_exit(
                                        &mut c,
                                        &target,
                                        &name,
                                        a,
                                        cond_profile.as_deref(),
                                        project,
                                        &mut results,
                                        &e,
                                        &emit_rows,
                                    );
                                }
                                return Err(stall_giveup_error(&target, project));
                            }
                        }
                        Some(StallStep::Respawn) => {
                            let kinds = crate::spawn_kinds_from_log(project);
                            let kind = kinds.get(&name).map(String::as_str).unwrap_or("opencode");
                            eprintln!(
                                "flywheel: {target} stalled again — respawning ({kind}) and re-sending (--respawn-on-stall)"
                            );
                            crate::cmd_internal_respawn(cfg, project, &name, kind, true)?;
                            stalls_seen += 1;
                        }
                        Some(StallStep::GiveUp) => {
                            eprintln!(
                                "flywheel: {target} agent_prompt_stalled after retries — best-effort pane send-text (not a wait receipt; GS-037 prompt --wait honesty)"
                            );
                            let _ = try_send_text_delivery(
                                &mut c, project, &name, &a.pane_id, &send_text,
                            );
                            if wait {
                                return wait_failure_exit(
                                    &mut c,
                                    &target,
                                    &name,
                                    a,
                                    cond_profile.as_deref(),
                                    project,
                                    &mut results,
                                    &e,
                                    &emit_rows,
                                );
                            }
                            return Err(stall_giveup_error(&target, project));
                        }
                    }
                }
            }
        };
        let status = agent.agent_status.as_str();
        if std::io::stdout().is_terminal() {
            println!(
                "{} {}",
                dim(crate::agent_target(a)),
                status_paint(crate::hc::parse_agent_status(status).unwrap_or(AgentStatus::Unknown))
            );
        }
        results.push(send_result_row(
            crate::agent_target(a),
            a.agent_status.as_str(),
            &a.pane_id,
            cond_profile.as_deref(),
            wait.then_some(status),
            hosted,
        ));
    }

    if registry_dirty && let Err(e) = state::save_spawn_registry(project, &registry) {
        eprintln!("flywheel: cannot persist skills_injected for send targets: {e:#}");
    }

    emit_rows(&results);

    let any_landed = results
        .iter()
        .any(|r| r.get("landed") == Some(&json!(true)));
    if wait && !any_landed {
        bail!(
            "send --wait produced no landed:true receipt — refuse invent-green exit 0 (GS-037 P0-A). Re-run when the agent can settle, or pass --fire-and-forget to acknowledge no receipt."
        );
    }
    if wait && any_landed {
        let ws_label =
            state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
        for r in &results {
            if r.get("landed") != Some(&json!(true)) {
                continue;
            }
            let settled = r
                .get("settled_status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let status_l = settled.to_ascii_lowercase();
            if status_l != "working"
                && status_l != "idle"
                && status_l != "done"
                && status_l != "blocked"
            {
                continue;
            }
            let target = r.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let pane_id = r.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
            let pin = registry
                .iter()
                .find(|e| {
                    e.name == target
                        || e.pane_id == pane_id
                        || e.pane_id == target
                        || e.mail_identity.as_deref() == Some(target)
                })
                .and_then(|e| e.mail_identity.clone())
                .or_else(|| {
                    if target.is_empty() {
                        None
                    } else {
                        Some(target.to_string())
                    }
                });
            if let Some(pin) = pin
                && let Err(e) = doorbell_note_observed_wake(&ws_label, &pin)
            {
                eprintln!(
                    "flywheel: observed-wake stamp FAILED after send --wait land for pin={pin}: {e:#} — refuse silent drop (GS-039 P0-1)"
                );
            }
        }
        let after = crate::doorbell::load_wake_state(&crate::doorbell::wake_state_file(
            &crate::config::state_dir(),
            &ws_label,
        ));
        let before_unix = wake_before_unix;
        if after.last_wake_unix.is_none() {
            bail!(
                "send --wait reported landed:true but doorbell last_wake did not advance (still never) — independent wake receipt missing (GS-037 P0-C / GS-039 P0-1). Wake stamp must come from doorbell/agent wake observation (Status working/idle or delivered prompt), not cmd_send. Robot-mode: never-wake after meaningful dispatch is a HARD FAIL — inspect `flywheel doorbell status` / wake state under the workspace label."
            );
        }
        match (before_unix, after.last_wake_unix) {
            (None, Some(_)) => {}
            (Some(b), Some(a)) if a > b => {}
            (Some(b), Some(a)) if a == b => {
                bail!(
                    "send --wait landed:true but doorbell last_wake did not advance (still {a}) — independent wake receipt missing (GS-037 P0-C). Wake stamp must come from doorbell/agent wake observation, not cmd_send."
                );
            }
            (Some(b), Some(a)) => {
                bail!(
                    "send --wait landed but doorbell last_wake went backwards ({b} → {a}) — wake state corrupt (GS-037 P0-C)"
                );
            }
            _ => {
                bail!(
                    "send --wait landed:true but doorbell last_wake did not advance — independent wake receipt missing (GS-037 P0-C)"
                );
            }
        }
    }

    if !wait && fire_and_forget.is_none() {
        bail!(
            "send dispatched without land/wake receipt — refuse invent-green exit 0 (GS-037 P0-A). Re-run with --wait (receipt-backed) or pass --fire-and-forget to acknowledge no receipt."
        );
    }
    Ok(())
}

/// Delay before the single automatic retry after `agent_prompt_stalled`.
/// Legacy name kept only as documentation of the old blind-sleep budget.
/// Stall recovery no longer sleeps this long before a blind resend; it reads
/// first (see [`probe_prompt_after_stall`]).
#[allow(dead_code)]
const STALL_RETRY_DELAY_SECS: u64 = 10;

/// One step of the send --wait stall-recovery ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallStep {
    /// First stall: wait, then re-send once.
    Retry,
    /// Second stall with --respawn-on-stall: respawn under the same name,
    /// then re-send.
    Respawn,
    /// Exhausted: fail with actionable recovery guidance.
    GiveUp,
}

/// bd-task-id-threading-xl2k: `task-<short-hash>` — stable, greppable,
/// collision-unlikely at crew scale (time + a process counter).
pub(crate) fn mint_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let h = format!("{:016x}", nanos as u64 ^ (n << 48));
    format!("task-{}", &h[..8])
}

/// ADR-0010 D1 (bd-w98c): normalize `--fire-and-forget` to an optional
/// caller-supplied delivery id. Bare flag = opt-out without an id;
/// `--fire-and-forget <id>` = idempotent re-enqueue key (same id re-sent
/// is the same delivery, never a second ladder entry).
pub(crate) fn delivery_id_of(fire_and_forget: &Option<String>) -> Option<&str> {
    match fire_and_forget {
        Some(id) if !id.is_empty() => Some(id.as_str()),
        _ => None,
    }
}

/// One send-result row. `dispatched: true` means the prompt was handed to
/// hc; it is NOT a receipt. GS-037 P0-A: fire-and-forget must not invent
/// green — `receipt:false` without --wait, and cmd_send exits nonzero unless
/// `--fire-and-forget` (or wait landed + wake). `landed` + `settled_status`
/// appear ONLY after --wait/--confirm observed the prompt land.
fn send_result_row(
    target: &str,
    prior_status: &str,
    pane_id: &str,
    profile: Option<&str>,
    settled: Option<&str>,
    hosted: bool,
) -> Value {
    let mut v = json!({
        "target": target,
        "dispatched": true,
        "receipt": settled.is_some(),
        "prior_status": prior_status,
        "pane_id": pane_id,
        "profile": profile,
    });
    if let Some(s) = settled {
        v["landed"] = json!(true);
        v["settled_status"] = json!(s);
    } else {
        v["landed"] = json!(false);
    }
    let tier = if hosted {
        crate::captain_directory::Tier::Pull
    } else {
        crate::captain_directory::Tier::Push
    };
    v["delivery"] = json!(
        crate::captain_directory::delivery_of(
            crate::captain_directory::AppendState::Appended,
            tier,
            settled.map(|_| true),
        )
        .as_str()
    );
    v
}

/// bd-8vci: row for a --wait send that ended WITHOUT a settled observation
/// (timeout/stall) after hc accepted the bytes. The prompt MAY have
/// started, so `landed` is deliberately ABSENT (not false) — ambiguity is
/// the honest report. `observed_status` carries the one lifecycle read-back
/// taken at failure time; omitted when that query itself failed.
pub(crate) fn wait_failure_row(
    target: &str,
    prior_status: &str,
    pane_id: &str,
    profile: Option<&str>,
    observed_status: Option<&str>,
) -> Value {
    let mut v = json!({
        "target": target,
        "dispatched": true,
        "receipt": false,
        "prior_status": prior_status,
        "pane_id": pane_id,
        "profile": profile,
    });
    if let Some(s) = observed_status {
        v["observed_status"] = json!(s);
    }
    v["delivery"] = json!(crate::captain_directory::Delivery::Unknown.as_str());
    v
}

/// bd-8vci: the nonzero-exit error for an ambiguous wait outcome — names the
/// observed status, the may-have-started ambiguity, and the recovery hint.
/// Never worded as a clean failure with zero evidence.
pub(crate) fn wait_failure_error(
    target: &str,
    cause: &BackendError,
    observed_status: Option<&str>,
    project: &str,
) -> anyhow::Error {
    anyhow::anyhow!(
        "send --wait: {target} gave no settled observation ({cause}) — the prompt MAY have started (observed_status: {}); treat as ambiguous, not a clean failure. {}",
        observed_status.unwrap_or("unknown"),
        stall_recovery_hint(project),
    )
}

/// bd-enov (F9/F15): adaptive-settle budget for the `--wait` failure path.
/// The opencode observation channel lags the wait windows badly enough that
/// EVERY oc send read as ambiguous while the pane was visibly working seconds
/// later — which trains captains to ignore the receipt entirely. A bounded
/// post-timeout poll for the `working` transition recovers the landed fact
/// WITHOUT inventing green (only an observed transition counts).
const WAIT_SETTLE_POLLS: u32 = 4;
const WAIT_SETTLE_POLL_SECS: u64 = 3;

/// F9/F15 (bd-enov): does a post-timeout status read prove the prompt landed?
/// Only a transition from a NON-working baseline to `working` — the lagging
/// observation channel catching up. Pure; `before` is the pane's status at
/// send time, `observed` the read-back's lowercase status.
pub(crate) fn wait_advance_landed(before: AgentStatus, observed: &str) -> bool {
    before != AgentStatus::Working && observed == "working"
}

/// F7/F8 (bd-enov): is the LATEST composed-run row for `run_id` non-terminal
/// (`running`)? Read from the same project event log `workflow history` uses.
/// Only the last row counts — a completed run also carries an earlier
/// `running` row, and a finished key must stay re-runnable.
pub(crate) fn running_run_row_exists(project: &str, run_id: &str) -> bool {
    let Ok(events) = state::read_log(project) else {
        return false;
    };
    events
        .iter()
        .rfind(|e| {
            e.kind == state::EventKind::Workflow
                && e.data.get("run_id").and_then(Value::as_str) == Some(run_id)
        })
        .and_then(|e| e.data.get("status").and_then(Value::as_str))
        == Some("running")
}

/// Keeps a composed run's pre-spawn lifecycle row terminal across every exit.
pub(crate) struct ComposeRunLifecycle {
    project: String,
    workflow: String,
    run_id: String,
    terminal: bool,
}

impl ComposeRunLifecycle {
    pub(crate) fn start(
        project: &str,
        workflow: &str,
        run_id: &str,
        crew: impl serde::Serialize,
    ) -> Self {
        crate::log(
            project,
            state::EventKind::Workflow,
            json!({
                "workflow": workflow,
                "status": "running",
                "crew": crew,
                "run_id": run_id,
                "mode": "pane",
                "started": state::now_iso(),
            }),
        );
        Self {
            project: project.to_string(),
            workflow: workflow.to_string(),
            run_id: run_id.to_string(),
            terminal: false,
        }
    }

    pub(crate) fn fail(&mut self, error: &str) {
        if self.terminal {
            return;
        }
        crate::log(
            &self.project,
            state::EventKind::Workflow,
            json!({
                "workflow": self.workflow,
                "status": "failed",
                "run_id": self.run_id,
                "mode": "pane",
                "error": error,
            }),
        );
        self.terminal = true;
    }

    pub(crate) fn finish(&mut self, data: Value) {
        if self.terminal {
            return;
        }
        crate::log(&self.project, state::EventKind::Workflow, data);
        self.terminal = true;
    }
}

impl Drop for ComposeRunLifecycle {
    fn drop(&mut self) {
        if !self.terminal {
            self.fail("compose host exited before terminal transition");
        }
    }
}

/// One lifecycle read-back for a pane, by target or name.
fn read_pane_status(c: &mut Client, target: &str, name: &str) -> Option<String> {
    c.agent_list().ok().and_then(|list| {
        list.iter()
            .find(|ag| ag.pane_id == target || ag.name.as_deref() == Some(name))
            .map(|ag| ag.agent_status.as_str().to_string())
    })
}

/// bd-8vci: wait timeout/stall after hc accepted the send. Before declaring
/// ambiguity, poll briefly for the opencode lagging channel to catch up with a
/// `working` transition (F9/F15); when it does, the prompt LANDED — emit the
/// landed row and return Ok. Otherwise one read-back becomes the row's
/// `observed_status`, the ambiguous row is pushed into `results`, the caller's
/// row emitter runs, and the returned error exits nonzero with the hint.
#[allow(clippy::too_many_arguments)]
fn wait_failure_exit(
    c: &mut Client,
    target: &str,
    name: &str,
    a: &AgentInfo,
    profile: Option<&str>,
    project: &str,
    results: &mut Vec<Value>,
    cause: &BackendError,
    emit: &dyn Fn(&[Value]),
) -> anyhow::Result<()> {
    let mut observed = read_pane_status(c, target, name);
    if !observed
        .as_deref()
        .is_some_and(|s| wait_advance_landed(a.agent_status, s))
    {
        for _ in 0..WAIT_SETTLE_POLLS {
            std::thread::sleep(Duration::from_secs(WAIT_SETTLE_POLL_SECS));
            observed = read_pane_status(c, target, name);
            if observed
                .as_deref()
                .is_some_and(|s| wait_advance_landed(a.agent_status, s))
            {
                break;
            }
        }
    }
    if observed
        .as_deref()
        .is_some_and(|s| wait_advance_landed(a.agent_status, s))
    {
        let status = observed.as_deref().unwrap_or("working");
        let hosted =
            agent_ref::binding_location_for(project, name) == agent_ref::BindingLocation::Hosted;
        results.push(send_result_row(
            target,
            a.agent_status.as_str(),
            &a.pane_id,
            profile,
            Some(status),
            hosted,
        ));
        emit(results);
        eprintln!(
            "flywheel: {target} settled after the wait window (observed {status}) — prompt landed"
        );
        return Ok(());
    }
    results.push(wait_failure_row(
        target,
        a.agent_status.as_str(),
        &a.pane_id,
        profile,
        observed.as_deref(),
    ));
    emit(results);
    Err(wait_failure_error(
        target,
        cause,
        observed.as_deref(),
        project,
    ))
}

#[cfg(test)]
mod running_run_row_tests {
    use super::running_run_row_exists;
    use crate::state::EventKind;

    /// F7/F8 (bd-enov): the adopt-or-refuse guard fires only while the LATEST
    /// row for the run is `running`; a terminal row re-opens the key.
    #[test]
    fn guard_reads_only_the_latest_row() {
        let _g = crate::config::test_state_home();
        let project = format!("f7f8-{}", std::process::id());
        let run = "fw-composed-abc";
        assert!(!running_run_row_exists(&project, run), "no row yet");
        crate::log(
            &project,
            EventKind::Workflow,
            serde_json::json!({"workflow": "wf", "status": "running", "run_id": run}),
        );
        assert!(running_run_row_exists(&project, run), "running row is live");
        crate::log(
            &project,
            EventKind::Workflow,
            serde_json::json!({"workflow": "wf", "status": "completed", "run_id": run}),
        );
        assert!(
            !running_run_row_exists(&project, run),
            "terminal row re-opens the key"
        );
        assert!(!running_run_row_exists(&project, "fw-composed-other"));
    }
}

#[cfg(test)]
mod wait_advance_tests {
    use super::{AgentStatus, wait_advance_landed};

    /// F9/F15 (bd-enov): only a transition from a non-working baseline to
    /// `working` proves a lagging oc prompt landed; a pane that was already
    /// working stays ambiguous (no invented green).
    #[test]
    fn only_idle_to_working_advance_counts() {
        assert!(wait_advance_landed(AgentStatus::Idle, "working"));
        assert!(wait_advance_landed(AgentStatus::Blocked, "working"));
        assert!(wait_advance_landed(AgentStatus::Done, "working"));
        assert!(
            !wait_advance_landed(AgentStatus::Working, "working"),
            "no advance"
        );
        assert!(!wait_advance_landed(AgentStatus::Idle, "idle"));
        assert!(!wait_advance_landed(AgentStatus::Idle, "blocked"));
        assert!(!wait_advance_landed(AgentStatus::Idle, "unknown"));
    }
}

#[cfg(test)]
mod task_id_tests {
    use super::mint_task_id;

    /// bd-task-id-threading-xl2k: minted ids are `task-<8 hex>` and unique
    /// across a burst (the completion mail threads on them — a collision
    /// would merge two unrelated tasks' history).
    #[test]
    fn task_ids_are_prefixed_unique_and_greppable() {
        let a = mint_task_id();
        assert!(a.starts_with("task-"), "prefix: {a}");
        assert_eq!(a.len(), "task-".len() + 8, "shape: {a}");
        assert!(
            a["task-".len()..].chars().all(|c| c.is_ascii_hexdigit()),
            "hex suffix: {a}"
        );
        let b = mint_task_id();
        assert_ne!(a, b, "consecutive mints differ");
    }
}

/// bd-pin-public-plane-s-easy.9: interrupt of a WORKING agent needs --force
/// — a captain must not nudge a working agent off its task because the send
/// JSON showed prior idle (the false-fail root cause). Pure: `force=false`
/// + working → Err naming --force; everything else passes.
pub(crate) fn interrupt_guard_working(status: AgentStatus, force: bool) -> Result<()> {
    if !force && status == AgentStatus::Working {
        bail!(
            "agent is working — interrupting would cancel its current task. \
             Leave it; a working agent is not stuck (prior idle in a send JSON \
             is not a miss). Pass --force to interrupt deliberately."
        );
    }
    Ok(())
}

/// bd-rdy: pure stall-classification for the interactive send --wait path.
/// `stalls_seen` = stalls already consumed (0 = first stall). Returns None
/// for non-stall codes so the caller propagates the backend error unchanged.
pub(crate) fn stall_step(
    code: &BackendErrorCode,
    stalls_seen: usize,
    respawn_on_stall: bool,
) -> Option<StallStep> {
    if *code != BackendErrorCode::AgentPromptStalled {
        return None;
    }
    match stalls_seen {
        0 => Some(StallStep::Retry),
        1 if respawn_on_stall => Some(StallStep::Respawn),
        _ => Some(StallStep::GiveUp),
    }
}

/// Outcome of the post-stall pane read (toron wait.rs: read → decide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallProbe {
    /// Text absent from pane read AND no state_change_seq advance — may resend.
    ProvenAbsent,
    /// Prompt text visible and/or seq advanced — do NOT resubmit.
    PresentOrActive,
    /// Could not read the pane / seq — fail closed (no blind resend).
    Unreadable,
}

/// Pure classifier for stall read-before-retry.
/// `read`: None = read failed; Some(true) = needle visible; Some(false) = absent.
/// `seq_advanced`: None = could not observe seq; Some(bool) = compared to baseline.
pub(crate) fn classify_stall_probe(read: Option<bool>, seq_advanced: Option<bool>) -> StallProbe {
    match (read, seq_advanced) {
        (Some(true), _) | (_, Some(true)) => StallProbe::PresentOrActive,
        (Some(false), Some(false)) | (Some(false), None) => StallProbe::ProvenAbsent,
        (None, Some(false)) | (None, None) => StallProbe::Unreadable,
    }
}

/// Resend only when the prompt is proven absent.
pub(crate) fn stall_may_resend(probe: StallProbe) -> bool {
    matches!(probe, StallProbe::ProvenAbsent)
}

/// Distinctive needle for presence checks (UTF-8 safe, bounded).
pub(crate) fn prompt_presence_needle(text: &str) -> String {
    text.chars().take(48).collect()
}

/// I/O boundary: read pane + seq, then classify. Never blind-resubmits.
fn probe_prompt_after_stall(
    c: &mut Client,
    target: &str,
    send_text: &str,
    baseline_seq: u64,
    baseline_completion: u64,
) -> StallProbe {
    let needle = prompt_presence_needle(send_text);
    let read = match c.agent_read(target, "screen", Some(120)) {
        Ok(content) => Some(!needle.is_empty() && content.contains(&needle)),
        Err(_) => match c.agent_read(target, "output", Some(80)) {
            Ok(content) => Some(!needle.is_empty() && content.contains(&needle)),
            Err(_) => None,
        },
    };
    let seq_advanced = c.agent_list().ok().and_then(|list| {
        list.into_iter()
            .find(|ag| ag.pane_id == target || ag.name.as_deref() == Some(target))
            .map(|ag| {
                ag.state_change_seq > baseline_seq
                    || ag.completion_seq.unwrap_or(0) > baseline_completion
            })
    });
    classify_stall_probe(read, seq_advanced)
}

/// bd-rdy: the give-up error for a twice-stalled send — names the recovery
/// commands instead of silently dropping the prompt.
pub(crate) fn stall_giveup_error(target: &str, project: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "agent {target} produced no observed state change twice (agent_prompt_stalled) — refusing invent-green landed (GS-037 prompt --wait honesty). The agent is not responding. {}\nAlternatively re-run with --respawn-on-stall to respawn {target} under the same name and re-send automatically.",
        stall_recovery_hint(project),
    )
}

/// bd-rdy: actionable recovery options for a stalled swarm.
fn stall_recovery_hint(project: &str) -> String {
    format!(
        "Recovery options:\n  flywheel recover {project}   respawn dead agents from the spawn registry\n  flywheel kill {project} --force   hard-reset the swarm"
    )
}

/// bd-4pxy: hc reports `agent_not_ready` when a still-booting agent
/// rejects a prompt addressed by NAME. hc accepts pane ids too, and a
/// booting agent answers its pane even before the name binding settles —
/// so the send path retries once via the pane id. `None` when the pane-id
/// retry already happened (bounded — never an infinite retry loop).
pub(crate) fn not_ready_retry_target(
    name_used: &str,
    agent: &AgentInfo,
    pane_retried: bool,
) -> Option<String> {
    if pane_retried || name_used == agent.pane_id {
        None
    } else {
        Some(agent.pane_id.clone())
    }
}

/// bd-4pxy: hc's `agent_not_ready` protocol code arrives unmapped
/// (BackendErrorCode::Other). Everything else (stalls, timeouts, connect)
/// keeps its existing handling.
pub(crate) fn is_agent_not_ready(code: &BackendErrorCode) -> bool {
    matches!(code, BackendErrorCode::Other(c) if c == "agent_not_ready")
}

/// H2 (bd-harness-jcode): hc's `agent.prompt` gates on a recognized
/// lifecycle-authority kind; recipe-launched harnesses (`pa`, `jcode`, ...)
/// report state via their own hooks but have no hc kind, so their panes
/// reject `agent.prompt` with `agent_not_ready` even while idle. Deliver the
/// prompt through the pane primitives instead: `pane.send_text` + Enter —
/// the same path the boot prime uses. Returns `Ok(true)` when the text was
/// delivered (the caller treats it as a successful send: jcode/prime-agent
/// TUIs submit on Enter), `Ok(false)` when this kind has no recipe (caller
/// falls through to the normal error), `Err` on delivery failure.
fn try_send_text_delivery(
    c: &mut Client,
    project: &str,
    name: &str,
    pane_id: &str,
    text: &str,
) -> bool {
    let recipe_kind = state::load_spawn_registry(project)
        .iter()
        .find(|en| en.name == name)
        .map(|en| en.harness.clone().unwrap_or_default())
        .or_else(|| crate::spawn_kinds_from_log(project).get(name).cloned());
    let Some(kind) = recipe_kind else {
        return false;
    };
    let has_recipe = profile::ProfileCatalog::load()
        .ok()
        .and_then(|cat| cat.harness(&kind).and_then(|h| h.spawn_cmd.clone()))
        .is_some();
    if !has_recipe {
        return false;
    }
    eprintln!(
        "flywheel: {name} ({pane_id}) is a recipe-harness pane (kind {kind}) — delivering via pane.send_text"
    );
    prime_line_via_send_text(c, pane_id, text)
}

/// bd-4pxy: a launch_pending agent is NOT idle — it is still booting
/// (status surfaces must not report it as idle).
pub(crate) fn status_label(launch_pending: bool, status: &str) -> String {
    if launch_pending {
        "launching".to_string()
    } else {
        status.to_string()
    }
}

/// bd-4pxy: the spawn-readiness failure — names the `launch_pending` field
/// and every agent still pending. `None` when all agents are launch-ready.
pub(crate) fn launch_pending_error(agents: &[AgentInfo]) -> Option<String> {
    let pending: Vec<&AgentInfo> = agents
        .iter()
        .filter(|a| crate::is_still_booting(a))
        .collect();
    if pending.is_empty() {
        return None;
    }
    let list = pending
        .iter()
        .map(|a| {
            format!(
                "{} ({})",
                a.name.as_deref().unwrap_or("<unnamed>"),
                a.pane_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "swarm not ready: launch_pending=true for {} — agent(s) still booting; wait and re-run, or check the agent logs (a harness that cannot start stays launch_pending)",
        list
    ))
}

pub(crate) fn bare_short_group_matches(
    project: &str,
    to: &str,
    names: &[String],
) -> Option<Vec<String>> {
    if !to
        .split(',')
        .any(|t| crate::to_want_token(project, t.trim()).broaden)
    {
        return None;
    }
    let matched: Vec<String> = names
        .iter()
        .filter(|n| {
            to.split(',').any(|t| {
                let w = crate::to_want_token(project, t.trim());
                crate::name_matches_want(n, &w)
            })
        })
        .cloned()
        .collect();
    if matched.len() > 1 {
        Some(matched)
    } else {
        None
    }
}

/// bd-98gd: Spec C everywhere — condition ONE captain `flywheel send`
/// target. The target's spawn-registry entry carries its profile; the
/// profile's behavior preamble is prepended via conditioned_prompt (the
/// SAME conditioning the `__send` leaf applies), with the skill-load
/// prefix on first dispatch (registry-tracked `skills_injected`). Returns
/// `(send_text, Some(profile) when conditioned, skills_injected_now)`.
/// Raw text when the target has no profiled entry, its profile is unknown
/// to the catalog, or `--raw` was passed — byte-identical legacy behavior.
pub(crate) fn condition_send_target(
    entry: Option<&mut state::SpawnEntry>,
    catalog: &profile::ProfileCatalog,
    text: &str,
    raw: bool,
) -> (String, Option<String>, bool) {
    let Some(entry) = entry else {
        return (text.to_string(), None, false);
    };
    let Some(profile_name) = entry.profile.as_deref() else {
        return (text.to_string(), None, false);
    };
    if raw {
        let framed = format!(
            "AUTHORITATIVE OPERATOR OBJECTIVE (open-mode / --raw; GS-039 P0-2 — NOT chatter; execute now; outranks stale captain diagnose TASK-IDs):

{text}"
        );
        return (framed, None, false);
    }
    let Some(profile_obj) = catalog.get(profile_name) else {
        eprintln!(
            "flywheel: send --to {} profile @{profile_name} unknown (flywheel profile list) — sending raw",
            entry.name
        );
        return (text.to_string(), None, false);
    };
    let mut send_base = text.to_string();
    let mut skills_injected = false;
    if let Some(prefix) = skill_injection_prefix(entry, &profile_obj.skills) {
        send_base = format!("{prefix}{send_base}");
        entry.skills_injected = true;
        skills_injected = true;
    }
    let send_text = crate::headless::conditioned_prompt(&profile_obj.behavior, &send_base);
    (send_text, Some(profile_name.to_string()), skills_injected)
}

#[cfg(test)]
mod send_conditioning_tests {
    use super::*;

    fn catalog_with(behavior: &str, skills: Vec<&str>) -> profile::ProfileCatalog {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "scout".to_string(),
            profile::Profile {
                harness: "opencode".into(),
                behavior: behavior.into(),
                skills: skills.into_iter().map(str::to_string).collect(),
                ..Default::default()
            },
        );
        profile::ProfileCatalog {
            profiles,
            harnesses: Default::default(),
        }
    }

    #[test]
    fn profiled_target_gets_behavior_preamble() {
        let catalog = catalog_with(
            "You are a SCOUT. VERIFIED/INFERRED labeling mandatory.",
            vec![],
        );
        let mut entry =
            state::SpawnEntry::profiled("w:p1", "crew-scout", "scout", "opencode", None);
        let (text, profile, injected) =
            condition_send_target(Some(&mut entry), &catalog, "echo who you are", false);
        assert!(
            text.starts_with("You are a SCOUT."),
            "behavior preamble must lead: {text:?}"
        );
        assert!(
            text.contains("echo who you are"),
            "mission preserved: {text:?}"
        );
        assert_eq!(
            profile.as_deref(),
            Some("scout"),
            "conditioned profile reported"
        );
        assert!(!injected, "no skills on this profile");
    }

    #[test]
    fn unprofiled_target_stays_raw() {
        let catalog = catalog_with("You are a SCOUT.", vec![]);
        let mut entry = state::SpawnEntry::legacy("w:p2", "plain-oc");
        let (text, profile, _) =
            condition_send_target(Some(&mut entry), &catalog, "echo who you are", false);
        assert_eq!(text, "echo who you are", "byte-identical legacy send");
        assert_eq!(profile, None);
        let (text, profile, _) = condition_send_target(None, &catalog, "echo who you are", false);
        assert_eq!(text, "echo who you are");
        assert_eq!(profile, None);
    }

    #[test]
    fn raw_flag_bypasses_conditioning() {
        let catalog = catalog_with("You are a SCOUT.", vec![]);
        let mut entry =
            state::SpawnEntry::profiled("w:p3", "crew-scout", "scout", "opencode", None);
        let (text, profile, _) =
            condition_send_target(Some(&mut entry), &catalog, "echo who you are", true);
        assert!(
            !text.contains("You are a SCOUT."),
            "--raw bypasses profile conditioning: {text:?}"
        );
        assert!(
            text.contains("AUTHORITATIVE OPERATOR OBJECTIVE") && text.contains("GS-039 P0-2"),
            "--raw is framed AUTHORITATIVE: {text:?}"
        );
        assert!(
            text.contains("echo who you are"),
            "mission preserved verbatim: {text:?}"
        );
        assert_eq!(profile, None);
    }

    #[test]
    fn unknown_profile_falls_back_raw() {
        let catalog = catalog_with("You are a SCOUT.", vec![]);
        let mut entry =
            state::SpawnEntry::profiled("w:p4", "crew-ghost", "no-such-profile", "opencode", None);
        let (text, profile, _) =
            condition_send_target(Some(&mut entry), &catalog, "echo who you are", false);
        assert_eq!(text, "echo who you are", "unknown profile never conditions");
        assert_eq!(profile, None);
    }

    #[test]
    fn skill_prefix_injected_once_then_not_repeated() {
        let catalog = catalog_with("You are a SCOUT.", vec!["accretive"]);
        let mut entry =
            state::SpawnEntry::profiled("w:p5", "crew-skilled", "scout", "opencode", None);
        let (text, profile, injected) =
            condition_send_target(Some(&mut entry), &catalog, "survey the repo", false);
        assert!(injected, "first dispatch carries the skill prefix");
        assert!(
            text.starts_with("You are a SCOUT."),
            "behavior preamble leads: {text:?}"
        );
        assert!(
            text.contains("Read skill://accretive before starting."),
            "skill prefix present: {text:?}"
        );
        assert!(
            text.contains("survey the repo"),
            "mission preserved: {text:?}"
        );
        assert_eq!(profile.as_deref(), Some("scout"));
        assert!(
            entry.skills_injected,
            "flag persisted on the registry entry"
        );
        let (text2, _, injected2) =
            condition_send_target(Some(&mut entry), &catalog, "survey the repo", false);
        assert!(!injected2, "prefix delivered once (bd-eign.10 advisory)");
        assert!(
            !text2.contains("Read skill://"),
            "no repeat prefix: {text2:?}"
        );
        assert!(text2.contains("You are a SCOUT."));
    }
}

#[cfg(test)]
mod send_result_row_tests {
    use super::*;

    /// bd-pin-public-plane-s-easy.9: fire-and-forget send JSON carries
    /// `dispatched: true` + `prior_status` (the hc status at target
    /// time) — never a bare `status` a captain parses as a miss when the
    /// agent was idle at dispatch. No `landed` without --wait/--confirm.
    #[test]
    fn fire_and_forget_row_is_dispatched_not_landed() {
        let row = send_result_row(
            "flywheel-toron-coder",
            "idle",
            "w1:p1",
            Some("coder"),
            None,
            false,
        );
        assert_eq!(row["target"], "flywheel-toron-coder");
        assert_eq!(row["dispatched"], true, "dispatch is recorded");
        assert_eq!(
            row["receipt"], false,
            "GS-037 P0-A: no receipt without --wait"
        );
        assert_eq!(row["prior_status"], "idle", "names the status as prior");
        assert!(
            row.get("status").is_none(),
            "no bare status field to misread as a miss"
        );
        assert_eq!(row["landed"], false, "landing only after --wait/--confirm");
        assert_eq!(row["delivery"], "queued");
    }

    /// bd-pin-public-plane-s-easy.9: --wait/--confirm adds `landed: true` +
    /// the settled status — the prompt was observed to land (agent reached a
    /// settled state), not just dispatched.
    #[test]
    fn wait_row_reports_landed_with_settled_status() {
        let row = send_result_row(
            "flywheel-toron-coder",
            "idle",
            "w1:p1",
            None,
            Some("done"),
            false,
        );
        assert_eq!(row["dispatched"], true);
        assert_eq!(row["receipt"], true);
        assert_eq!(row["prior_status"], "idle");
        assert_eq!(row["landed"], true, "--wait observed landing");
        assert_eq!(row["settled_status"], "done");
        assert_eq!(row["delivery"], "woke");
    }

    /// bd-pin-public-plane-s-easy.9: a working agent at target time is a
    /// valid dispatch target — prior_status working must not read as a
    /// failure (the captain false-fail root cause).
    #[test]
    fn row_preserves_working_prior_status() {
        let row = send_result_row(
            "flywheel-toron-coder",
            "working",
            "w1:p1",
            None,
            None,
            false,
        );
        assert_eq!(row["dispatched"], true);
        assert_eq!(row["prior_status"], "working");
        assert_eq!(row["delivery"], "queued");
    }

    /// bd-adr0006-cosmetic-sweep-q0g0: a confirm TIMEOUT is AMBIGUOUS (the
    /// prompt may have started — cg6.5), so the leaf JSON must NOT claim a
    /// definitive `landed: false`. The 98io graduation run surfaced this:
    /// the verify-stage reviewer's 45s confirm expired, the step output read
    /// `landed: false`, and the run still completed green — the flag lied.
    #[test]
    fn confirm_send_json_timeout_is_ambiguous_not_false() {
        let v = crate::cmd::internal::confirm_send_json("reviewer-1", SendVerdict::TimedOut);
        assert_eq!(v["reason"], "timeout");
        assert!(
            v["landed"].is_null(),
            "timeout is unknown, never a definitive false: {v}"
        );
        assert_eq!(v["ambiguous"], true);
        let outcome: durable::SendOutcome = serde_json::from_value(v).expect("parse");
        assert_eq!(outcome.landed, None);
        assert_eq!(
            durable::escalate(&outcome, 9),
            durable::Escalation::Accept,
            "timeout stays an accept at any failure count"
        );
    }

    /// bd-adr0006-cosmetic-sweep-q0g0: the other verdicts keep their exact
    /// wire shape — only the timeout flag was dishonest.
    #[test]
    fn confirm_send_json_landed_and_stalled_unchanged() {
        let landed = crate::cmd::internal::confirm_send_json(
            "a",
            SendVerdict::Landed(Some(AgentStatus::Working)),
        );
        assert_eq!(landed["landed"], true);
        assert_eq!(landed["status"], "working");
        assert_eq!(landed["reason"], "landed");
        let stalled = crate::cmd::internal::confirm_send_json("a", SendVerdict::Stalled);
        assert_eq!(
            stalled["landed"], false,
            "stall IS a definitive non-landing"
        );
        assert_eq!(stalled["reason"], "stalled");
        let coded = crate::cmd::internal::confirm_send_json(
            "a",
            SendVerdict::Coded("agent_not_found".into()),
        );
        assert_eq!(coded["landed"], false);
        assert_eq!(coded["reason"], "error:agent_not_found");
    }

    /// bd-pin-public-plane-s-easy.9: interrupt of a working agent needs
    /// --force — a captain must not nudge a working agent off its task
    /// because the send JSON showed prior idle. Pure guard, unit-tested.
    #[test]
    fn interrupt_of_working_agent_needs_force() {
        let err = interrupt_guard_working(AgentStatus::Working, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "names the escape hatch: {err}");
        assert!(
            interrupt_guard_working(AgentStatus::Working, true).is_ok(),
            "--force permits the interrupt"
        );
        assert!(
            interrupt_guard_working(AgentStatus::Idle, false).is_ok(),
            "idle needs no force"
        );
    }
}

#[cfg(test)]
mod send_failure_log_tests {
    /// bd-n9hy T1 (red first): every __send dispatch failure must keep the
    /// raw stdout+stderr bytes plus exit status — the loop leaf at ~19237
    /// today formats stdout only, so a bare `{ok:false}` with stderr on a
    /// fresh pane is impossible to misdiagnose.
    #[test]
    fn send_failure_keeps_raw_bytes() {
        let line = crate::format_send_failure("coder-1", "", "hc: agent_not_ready", false, 1);
        assert!(line.contains("coder-1"), "names the target: {line:?}");
        assert!(
            line.contains("hc: agent_not_ready"),
            "stderr bytes kept: {line:?}"
        );
        assert!(line.contains("status=1"), "exit status kept: {line:?}");
    }
}

#[cfg(test)]
mod first_dispatch_gate_tests {
    use super::*;

    fn agent_row(booting: bool) -> AgentInfo {
        AgentInfo {
            agent: None,
            agent_status: AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: !booting,
            launch_pending: booting,
            name: Some("crew-x-planner".to_string()),
            pane_id: "w9:p1".to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "w9:t1".to_string(),
            terminal_id: "term-1".to_string(),
            terminal_title: None,
            workspace_id: "w9".to_string(),
        }
    }

    /// bd-83as/bd-mcax: first dispatch = skills not yet injected (fresh
    /// spawn). Landed history never silences the gate — terminal names are
    /// stable across respawns, so a landed record would mute it forever.
    #[test]
    fn first_dispatch_predicate() {
        assert!(crate::cmd::internal::is_first_dispatch(false));
        assert!(!crate::cmd::internal::is_first_dispatch(true));
    }

    /// bd-mcax (red first): the gate decision — first dispatch gates when
    /// the agent is missing (fresh pane still binding) or still booting.
    /// Later dispatches, and found-and-ready agents, never gate.
    #[test]
    fn gate_decision_covers_missing_and_booting() {
        assert!(crate::cmd::internal::should_gate_first_dispatch(true, None));
        assert!(crate::cmd::internal::should_gate_first_dispatch(
            true,
            Some(&agent_row(true))
        ));
        assert!(!crate::cmd::internal::should_gate_first_dispatch(
            true,
            Some(&agent_row(false))
        ));
        assert!(!crate::cmd::internal::should_gate_first_dispatch(
            false, None
        ));
        assert!(!crate::cmd::internal::should_gate_first_dispatch(
            false,
            Some(&agent_row(true))
        ));
    }

    /// bd-mcax (red first): scripted missing then present proceeds —
    /// the live gap was a missing row failing instantly.
    #[test]
    fn missing_then_present_proceeds() {
        let script = [vec![], vec![], vec![agent_row(false)]];
        let calls = std::cell::Cell::new(0);
        let out = crate::cmd::internal::await_agent_ready(
            || {
                let i = calls.get();
                calls.set(i + 1);
                Ok(script[i.min(script.len() - 1)].clone())
            },
            "crew-x-planner",
            "w9",
            std::time::Duration::from_secs(10),
        )
        .expect("present on third listing");
        assert_eq!(calls.get(), 3);
        assert!(!out.launch_pending);
    }

    /// bd-83as T2 (red first): scripted not-ready then ready proceeds and
    /// observes every listing.
    #[test]
    fn first_dispatch_waits_for_readiness() {
        let script = [
            vec![agent_row(true)],
            vec![agent_row(true)],
            vec![agent_row(false)],
        ];
        let calls = std::cell::Cell::new(0);
        let out = crate::cmd::internal::await_agent_ready(
            || {
                let i = calls.get();
                calls.set(i + 1);
                Ok(script[i.min(script.len() - 1)].clone())
            },
            "crew-x-planner",
            "w9",
            std::time::Duration::from_secs(10),
        )
        .expect("ready on third listing");
        assert_eq!(calls.get(), 3);
        assert!(!out.launch_pending);
    }

    /// bd-83as T2 (red first): a pane that never leaves boot fails typed
    /// inside the bound — never hangs, never a bare ok:false downstream.
    #[test]
    fn boot_timeout_is_typed() {
        let err = crate::cmd::internal::await_agent_ready(
            || Ok(vec![agent_row(true)]),
            "crew-x-planner",
            "w9",
            std::time::Duration::from_millis(120),
        )
        .expect_err("must time out");
        assert!(err.to_string().contains("still booting"), "{err:?}");
    }
}

#[cfg(test)]
mod send_receipt {
    use super::{wait_failure_error, wait_failure_row};
    use toron_plane::backend::BackendError;
    use toron_plane::backend::error::BackendErrorCode;

    /// bd-8vci: a --wait timeout/stall row is deliberately AMBIGUOUS —
    /// receipt:false, NO `landed` key (the prompt may have started), and the
    /// one lifecycle read-back surfaced as `observed_status`.
    #[test]
    fn send_receipt_wait_failure_row_is_ambiguous_not_clean_failure() {
        let row = wait_failure_row("flywheel-oc", "idle", "%3", Some("pi"), Some("working"));
        assert_eq!(row["target"], "flywheel-oc");
        assert_eq!(row["dispatched"], true);
        assert_eq!(row["receipt"], false);
        assert_eq!(row["observed_status"], "working");
        assert!(
            row.get("landed").is_none(),
            "landed must be absent on an ambiguous wait failure, not false: {row}"
        );
        let row = wait_failure_row("flywheel-oc", "idle", "%3", None, None);
        assert!(row.get("observed_status").is_none());
        assert!(row["profile"].is_null());
        assert_eq!(row["delivery"], "unknown");
    }

    /// bd-8vci: the wait-failure error must read as ambiguous (prompt MAY
    /// have started) and carry the recovery hint — nonzero exit intent
    /// without a clean-failure claim.
    #[test]
    fn send_receipt_wait_failure_error_names_status_and_recovery() {
        let cause = BackendError::new(
            BackendErrorCode::Timeout,
            "no lifecycle change within 30000ms",
        );
        let err = wait_failure_error("flywheel-oc", &cause, Some("working"), "flywheel");
        let msg = err.to_string();
        assert!(msg.contains("MAY have started"), "{msg}");
        assert!(msg.contains("observed_status: working"), "{msg}");
        assert!(
            msg.contains("flywheel recover flywheel"),
            "recovery hint: {msg}"
        );
        let unknown = wait_failure_error("flywheel-oc", &cause, None, "flywheel");
        assert!(unknown.to_string().contains("observed_status: unknown"));
    }
}

#[cfg(test)]
mod adr0010_d1_tests {
    // governed-by: ADR-0010 D1 (bd-w98c) — fire-and-forget delivery-id bind.
    use super::*;

    #[test]
    fn adr0010_d1_delivery_id_normalizes_flag_shape() {
        assert_eq!(delivery_id_of(&None), None, "absent flag = no id");
        assert_eq!(
            delivery_id_of(&Some(String::new())),
            None,
            "bare --fire-and-forget = opt-out without an id"
        );
        assert_eq!(
            delivery_id_of(&Some("d-77".into())),
            Some("d-77"),
            "--fire-and-forget <id> = idempotent re-enqueue key"
        );
    }
}

#[cfg(test)]
mod reservation_identity_tests {
    use toron_core::mailbox::guard::{Reservation, SHARED_SAFE_PATHS, guard_check_strict};

    /// bd-2mdo RED: an actor token that names a REGISTERED mailbox identity
    /// passes through VERBATIM — it is an address, not an hc slug to
    /// re-derive. The hash fallback minted RainyElk for the literally-
    /// registered e2e-zord (e2e_serialize_worktree canary), a stranger no
    /// crew knows and the one-ladder rule forbids.
    #[test]
    fn actor_resolution_passthrough_for_registered_identity() {
        let env = |k: &str| -> Option<String> { (k == "AGENT_NAME").then(|| "oc5".to_string()) };
        let roster = vec!["e2e-zord".to_string(), "FoggyDesert".to_string()];
        assert_eq!(
            crate::cmd::mail::resolve_mail_actor_roster("proj", Some("e2e-zord"), &env, &roster)
                .as_deref(),
            Some("e2e-zord"),
            "registered explicit --as must not be hash-mangled"
        );
        assert_eq!(
            crate::cmd::mail::resolve_mail_actor_roster("proj", None, &env, &roster).as_deref(),
            Some(crate::resolve_mail_target_registry("proj", "oc5")).as_deref(),
        );
        let env_pin =
            |k: &str| -> Option<String> { (k == "AGENT_NAME").then(|| "FoggyDesert".to_string()) };
        assert_eq!(
            crate::cmd::mail::resolve_mail_actor_roster("proj", None, &env_pin, &roster).as_deref(),
            Some("FoggyDesert"),
            "registered pin must survive unchanged"
        );
        assert_eq!(
            crate::cmd::mail::resolve_mail_actor_roster("proj", None, &|_| None, &roster),
            None
        );
    }

    /// THE failing sequence from tonight, as a self-test: the wrapper binds
    /// a lease as identity X; the guard attributes the commit to X — the
    /// strict sweep-guard must PASS its own holder's commit and BLOCK any
    /// other attribution (the ~6 blocked commits failure mode).
    #[test]
    fn reservation_identity_holder_passes_own_commit_guard() {
        let holder = crate::authority::select_agent_identity(None, &|k| {
            (k == "AGENT_NAME").then(|| "IronSummit".to_string())
        })
        .expect("env-bound identity");
        let staged = vec!["src/main.rs".to_string(), "src/stages.rs".to_string()];
        let own = vec![Reservation {
            holder: holder.clone(),
            pattern: "src/**".to_string(),
            exclusive: true,
        }];
        let blocks = guard_check_strict(Some(&holder), &staged, &own, SHARED_SAFE_PATHS);
        assert!(
            blocks.is_empty(),
            "own lease must not block own commit: {blocks:?}"
        );
        let other = guard_check_strict(Some("Flywheel"), &staged, &own, SHARED_SAFE_PATHS);
        assert!(!other.is_empty(), "foreign attribution must block");
    }
}
