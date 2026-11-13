//! `flywheel add` / `flywheel steer` verbs + preflight gate (split from
//! `main.rs`, bd-wave5). Crew creation lives here; the send verbs live in
//! [`super::send`].

use std::collections::{BTreeMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

#[cfg(test)]
use crate::cmd::internal::skill_injection_prefix;
use crate::cmd::send::launch_pending_error;
use crate::config::Config;
use crate::hc::{AgentInfo, Client};
use crate::output::{dim, kind_paint, print_json};
#[cfg(test)]
use crate::pane_write;
#[cfg(feature = "backend-superlogical")]
use crate::partition_workspaces;
use crate::state::EventKind;
#[cfg(test)]
use crate::uncommitted_decision_docs;
use crate::{
    ProfileLaunchPlan, SpawnOpts, agent_name_n, bare_model_launch, bind_pane_mail_identity,
    bind_spawn_role, boot_prime_line, check_project_spelling, cmd_arrive, comms_inbox_on,
    dead_panes, ensure_and_join_channels, free_spawn_ordinal, is_still_booting, log,
    needs_in_arg_prime, note_role_bind_result, pane_takes_hc_stamp, peer_prompt_template,
    pinned_mail_identity, preflight_kind_integration, prime_bootstrap, prime_line_via_send_text,
    probe_agent_liveness, profile_launch_plan, project_dir, project_workspaces,
    publish_presence_for_registry, read_integration_statuses, recipe_counts, refuse_unpinned_spawn,
    slugify_label, spawn_item_kind, spawn_items_from_plan, spawn_join_notice, spawn_model_rows,
    start_agent_with_retry_profile, swarm_label, utf8_path, verify_or_respawn_pane,
};
use crate::{
    agent_ref, backend, captain_directory, config, doorbell, hc, models, pane, profile,
    request_receipts, state,
};

/// bd-add-plane-dnzz: rex-plane add. The hc arm below builds an
/// `hc::Client` and its workspace list, so a rex-plane swarm is invisible
/// ("no flywheel-owned swarm") and `spawn --join` never gains a pane.
/// This arm resolves the owned workspace through the multiplexer backend
/// (`partition_workspaces` + `add_target_from_split`, the same partition
/// spawn uses) and creates panes through it too (`pane_split` + `pane_run`
/// with the env-prefixed command, the same shape `cmd_spawn_rex` uses —
/// the trait's `pane_split` carries no env map, so the pin rides as an
/// `env K=V` prefix via `rex_env_command`). hc-only gates (shell-prompt
/// wait, foreground wait, integration preflight, send-text prime) are
/// skipped: rex blocks report readiness via `agent_wait` and the prime
/// rides `pane_run`.
pub(crate) fn cmd_add_rex(
    cfg: &Config,
    project: &str,
    counts: &crate::AgentCounts,
    kinds: &[(String, u32)],
    settle_secs: u64,
    request_key: Option<&str>,
) -> Result<()> {
    crate::check_project_spelling(project, "add")?;
    // A mutating door must not pave over a corrupt fleet: refuse first.
    state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let plan = crate::recipe_counts(None, counts, kinds)?;
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    let add_base = format!("{}-add", swarm_label(project));
    let add_subject = match request_key {
        Some(key) => crate::request_receipts::label_for_request_key(&add_base, key),
        None => crate::slugify_label(&add_base).0,
    };
    let add_plan = crate::request_receipts::plan_digest(
        &plan
            .iter()
            .map(|(short, n)| (cfg.resolve_kind(short), *n, None))
            .collect::<Vec<_>>(),
    );
    if let Some(key) = request_key {
        match crate::request_receipts::spawn_gate(
            crate::request_receipts::load_spawn_row(project, &add_subject).as_ref(),
            &add_plan,
        ) {
            crate::request_receipts::Gate::Proceed => {}
            crate::request_receipts::Gate::Replay => {
                print_json(&json!({
                    "request_key": key,
                    "slot": add_subject,
                    "already_added": true,
                    "replay": true,
                }));
                eprintln!(
                    "flywheel: add request key {key:?} already settled this slot ({add_subject}) — \
                     replaying, no second add. Its panes: flywheel list --json."
                );
                return Ok(());
            }
            crate::request_receipts::Gate::InFlight => bail!(
                "add request key {key:?} is IN FLIGHT (slot {add_subject}) — a second dispatch \
                 could add panes into a workspace that is still receiving the first. Inspect it \
                 with `flywheel status {project}`, or retry with a fresh --request-key."
            ),
            crate::request_receipts::Gate::Conflict => bail!(
                "add request key {key:?} was already used for a DIFFERENT plan (the recorded plan \
                 digest differs) — refusing to hand back another add's slot under this key. Use a \
                 fresh --request-key for the new plan."
            ),
        }
    }
    let add_effect = match request_key {
        Some(_) => agent_ref::AgentEffectGuard::begin_with(
            agent_ref::AgentEffect::prepare(project, &add_subject, agent_ref::AgentPhase::Spawn)
                .with_plan_digest(add_plan),
        )?,
        None => {
            agent_ref::AgentEffectGuard::begin(project, &add_subject, agent_ref::AgentPhase::Spawn)?
        }
    };
    let dir = crate::project_dir(cfg, project);
    let backend = backend::multiplexer_backend(cfg)?;
    let ws_list = backend.workspace_list()?;
    let agents = backend.agent_list().unwrap_or_default();
    let split = crate::partition_workspaces(
        project,
        ws_list,
        &agents,
        state::load_spawn_meta(project).as_deref(),
    );
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    let ws_id = crate::add_target_from_split(&split, &agents, project)?;
    let existing: Vec<AgentInfo> = agents
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    let panes = backend.pane_list(Some(&ws_id))?;
    let root = panes
        .first()
        .ok_or_else(|| anyhow!("workspace {ws_id} has no panes"))?
        .pane_id
        .clone();
    let mut ordinals: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for a in &existing {
        let Some(name) = a.name.as_deref() else {
            continue;
        };
        for (short, _) in &plan {
            if let Some(o) = crate::agent_ordinal(name, project, short) {
                ordinals
                    .entry(short.clone())
                    .and_modify(|m| *m = (*m).max(o))
                    .or_insert(o);
            }
        }
    }
    let mut prev = root;
    let mut spawned: Vec<Value> = Vec::new();
    for (short, n) in &plan {
        let kind = cfg.resolve_kind(short);
        for _ in 0..*n {
            let ordinal = ordinals.entry(short.clone()).or_insert(0);
            *ordinal += 1;
            let name = crate::agent_name_n(project, short, *ordinal);
            let mail_identity = crate::pinned_mail_identity(cfg, project, &name, &kind);
            if mail_identity.is_none() {
                crate::refuse_unpinned_spawn(&name, None, backend::mail_backend(cfg).is_ok())?;
            }
            let identity = mail_identity.as_deref().unwrap_or(&name);
            let mut add_args: Vec<String> = Vec::new();
            if crate::needs_in_arg_prime(&kind) {
                add_args.push(crate::boot_prime_line().to_string());
            }
            let pane_env = crate::hc::plane_launch_env(cfg, mail_identity.as_deref());
            let cwd = crate::utf8_path(&dir, "workspace dir")?;
            let split_pane = backend
                .pane_split(&prev, "down", Some(&cwd))
                .map_err(|e| anyhow!("add {name}: rex split failed: {e:#}"))?;
            prev = split_pane.pane_id.clone();
            let command = crate::pane::rex_env_command(&pane_env, &kind, &add_args);
            if let Err(e) = backend.pane_run(&prev, &command) {
                bail!("add {name}: rex run failed on {prev}: {e:#}");
            }
            if let Err(e) = backend.agent_rename(&prev, Some(identity)) {
                eprintln!("flywheel: warn: add rename {prev} → {identity} failed: {e:#}");
            }
            if let Err(e) = backend.agent_wait(
                &prev,
                &[
                    crate::hc::AgentStatus::Idle,
                    crate::hc::AgentStatus::Working,
                ],
                Some(90_000),
            ) {
                eprintln!("flywheel: warn: {name} not ready within 90s of add: {e:#}");
            }
            if settle_secs > 0 {
                std::thread::sleep(Duration::from_secs(settle_secs));
            }
            if let Err(e) =
                backend.pane_run(&prev, &crate::doorbell::identity_prime(&name, identity))
            {
                eprintln!("flywheel: warn: rex prime for {name} ({prev}) failed: {e:#}");
            }
            crate::bind_pane_mail_identity(cfg, project, &name, &prev, mail_identity.as_deref());
            crate::bind_spawn_role(cfg, project, None, short, mail_identity.as_deref())?;
            println!("  {} {}", kind_paint(&kind), prev);
            spawned.push(json!({ "name": name, "kind": kind, "pane_id": prev }));
        }
    }
    let registry = state::load_spawn_registry(project);
    if let Err(e) = state::save_agent_names(&dir, &registry) {
        eprintln!("flywheel: cannot save .flywheel/agent-names.json: {e:#}");
    }
    if let Err(e) = state::publish_agent_bindings(project, &registry) {
        eprintln!("flywheel: cannot publish shared agent bindings: {e:#}");
    }
    crate::log(
        project,
        state::EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    add_effect.settle(Some(&format!("workspace={ws_id} added={}", spawned.len())));
    Ok(())
}

pub(crate) fn cmd_add(
    cfg: &Config,
    project: &str,
    counts: &crate::AgentCounts,
    kinds: &[(String, u32)],
    settle_secs: u64,
    request_key: Option<&str>,
) -> Result<()> {
    crate::check_project_spelling(project, "add")?;
    // A mutating door must not pave over a corrupt fleet: refuse first.
    state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    // One plane resolution for the whole door, with spawn's guard: a typo'd
    // backend refuses before any side effect (the hc arm below dials the
    // socket directly, bypassing the factory), and an explicit rex pin with no
    // CLI refuses here instead of after the effect row is journaled.
    let plane = crate::backend::resolve_plane(cfg);
    crate::backend::factory::require_mux_for_spawn(plane.configured.as_str())?;
    if plane.plane == crate::backend::Plane::Rex {
        if !plane.rex_reachable {
            bail!(
                "add: {} — install the rex CLI or select the fallback (FLYWHEEL_BACKEND=hc)",
                plane.reason
            );
        }
        return cmd_add_rex(cfg, project, counts, kinds, settle_secs, request_key);
    }
    let plan = crate::recipe_counts(None, counts, kinds)?;
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    let add_base = format!("{}-add", swarm_label(project));
    let add_subject = match request_key {
        Some(key) => crate::request_receipts::label_for_request_key(&add_base, key),
        None => crate::slugify_label(&add_base).0,
    };
    let add_plan = crate::request_receipts::plan_digest(
        &plan
            .iter()
            .map(|(short, n)| (cfg.resolve_kind(short), *n, None))
            .collect::<Vec<_>>(),
    );
    if let Some(key) = request_key {
        match crate::request_receipts::spawn_gate(
            crate::request_receipts::load_spawn_row(project, &add_subject).as_ref(),
            &add_plan,
        ) {
            crate::request_receipts::Gate::Proceed => {}
            crate::request_receipts::Gate::Replay => {
                print_json(&json!({
                    "request_key": key,
                    "slot": add_subject,
                    "already_added": true,
                    "replay": true,
                }));
                eprintln!(
                    "flywheel: add request key {key:?} already settled this slot ({add_subject}) — \
                     replaying, no second add. Its panes: flywheel list --json."
                );
                return Ok(());
            }
            crate::request_receipts::Gate::InFlight => bail!(
                "add request key {key:?} is IN FLIGHT (slot {add_subject}) — a second dispatch \
                 could add panes into a workspace that is still receiving the first. Inspect it \
                 with `flywheel status {project}`, or retry with a fresh --request-key."
            ),
            crate::request_receipts::Gate::Conflict => bail!(
                "add request key {key:?} was already used for a DIFFERENT plan (the recorded plan \
                 digest differs) — refusing to hand back another add's slot under this key. Use a \
                 fresh --request-key for the new plan."
            ),
        }
    }
    let add_effect = match request_key {
        Some(_) => agent_ref::AgentEffectGuard::begin_with(
            agent_ref::AgentEffect::prepare(project, &add_subject, agent_ref::AgentPhase::Spawn)
                .with_plan_digest(add_plan),
        )?,
        None => {
            agent_ref::AgentEffectGuard::begin(project, &add_subject, agent_ref::AgentPhase::Spawn)?
        }
    };
    let dir = crate::project_dir(cfg, project);
    let mut c = crate::hc::Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::add_target_workspace(&mut c, project)?;
    let existing: Vec<AgentInfo> = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect();
    let panes = c.pane_list()?;
    let root = panes
        .first()
        .ok_or_else(|| anyhow!("workspace {ws_id} has no panes"))?;
    let mut ordinals: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for a in &existing {
        let Some(name) = a.name.as_deref() else {
            continue;
        };
        for (short, _) in &plan {
            if let Some(o) = crate::agent_ordinal(name, project, short) {
                ordinals
                    .entry(short.clone())
                    .and_modify(|m| *m = (*m).max(o))
                    .or_insert(o);
            }
        }
    }
    let mut prev = root.pane_id.clone();
    let mut spawned: Vec<Value> = Vec::new();
    let integration_statuses = crate::read_integration_statuses();
    let mut preflighted_kinds: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (short, n) in &plan {
        let kind = cfg.resolve_kind(short);
        if preflighted_kinds.insert(kind.clone()) {
            crate::preflight_kind_integration(&mut c, &kind, &integration_statuses);
        }
        for _ in 0..*n {
            let ordinal = ordinals.entry(short.clone()).or_insert(0);
            *ordinal += 1;
            let name = crate::agent_name_n(project, short, *ordinal);
            let mail_identity = crate::pinned_mail_identity(cfg, project, &name, &kind);
            if mail_identity.is_none() {
                crate::refuse_unpinned_spawn(&name, None, backend::mail_backend(cfg).is_ok())?;
            }
            let pane_env = crate::hc::plane_launch_env(cfg, mail_identity.as_deref());
            let p = crate::hc::pane_split(
                &c,
                &prev,
                "down",
                Some(&crate::utf8_path(&dir, "workspace dir")?),
                Some(&pane_env),
            )?;
            prev = p.pane_id.clone();
            let mut add_args: Vec<String> = Vec::new();
            if crate::needs_in_arg_prime(&kind) {
                add_args.push(crate::boot_prime_line().to_string());
            }
            let shell_ready = crate::hc::wait_shell_prompt(&c, &prev, Duration::from_secs(15));
            if !shell_ready {
                eprintln!(
                    "flywheel: warn: shell for {name} ({prev}) did not render a prompt \
                     within 15s — the launch command may be mangled by the booting shell"
                );
            }
            let agent = crate::start_agent_with_retry(&mut c, &name, &kind, &prev, &add_args)
                .with_context(|| {
                    format!(
                        "add {name}: pane {prev} left behind — close it with `hc terminal close {prev}`"
                    )
                })?;
            let foreground =
                crate::hc::wait_agent_foreground(&c, &prev, &kind, &name, Duration::from_secs(15));
            if !foreground {
                match crate::hc::pane_process_info(&c, &prev)
                    .map(|pi| crate::hc::pane_agent_alive(&pi, &kind))
                {
                    Ok(Some(false)) => bail!(
                        "add {name}: agent process gone on {prev} — undriven shell left behind; close it with `hc terminal close {prev}`"
                    ),
                    Ok(_) => eprintln!(
                        "flywheel: warn: {name} ({prev}) not foreground within 15s — boot unverified; check with `hc terminal process-info --pane {prev}`"
                    ),
                    Err(e) => eprintln!(
                        "flywheel: warn: {name} ({prev}) liveness probe failed: {e:#} — boot unverified"
                    ),
                }
            }
            if settle_secs > 0 {
                std::thread::sleep(Duration::from_secs(settle_secs));
            }
            if !crate::needs_in_arg_prime(&kind) {
                crate::prime_line_via_send_text(
                    &mut c,
                    &prev,
                    &crate::doorbell::identity_prime(
                        &name,
                        mail_identity.as_deref().unwrap_or(&name),
                    ),
                );
                prime_bootstrap(&mut c, &prev, &kind);
            }
            crate::bind_pane_mail_identity(
                cfg,
                project,
                &name,
                &agent.pane_id,
                mail_identity.as_deref(),
            );
            crate::bind_spawn_role(cfg, project, None, short, mail_identity.as_deref())?;
            println!("  {} {}", crate::output::kind_paint(&kind), agent.pane_id);
            spawned
                .push(json!({ "name": agent.name, "kind": agent.agent, "pane_id": agent.pane_id }));
        }
    }
    let registry = state::load_spawn_registry(project);
    if let Err(e) = state::save_agent_names(&dir, &registry) {
        eprintln!("flywheel: cannot save .flywheel/agent-names.json: {e:#}");
    }
    if let Err(e) = state::publish_agent_bindings(project, &registry) {
        eprintln!("flywheel: cannot publish shared agent bindings: {e:#}");
    }
    crate::log(
        project,
        state::EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    add_effect.settle(Some(&format!("workspace={ws_id} added={}", spawned.len())));
    Ok(())
}

pub(crate) fn preflight_gate(
    cfg: &Config,
    skip: bool,
    mode: crate::preflight::Mode,
    project: Option<&str>,
) -> Result<()> {
    if skip {
        return Ok(());
    }
    let checks = crate::preflight::run(cfg, mode, project);
    if crate::preflight::all_pass(&checks) {
        return Ok(());
    }
    eprint!("{}", crate::preflight::render(&checks));
    bail!("preflight failed — fix the above or pass --skip-preflight");
}

/// `flywheel preflight --mode <m> [--project <p>] [--work] [--json]`
pub(crate) fn cmd_preflight(
    cfg: &Config,
    mode: &str,
    project: Option<&str>,
    work: bool,
    json: bool,
) -> Result<()> {
    let pf_mode = crate::preflight::Mode::named(mode, work).ok_or_else(|| {
        anyhow::anyhow!("unknown preflight mode: {mode} (steer|loop|workflow|cron)")
    })?;
    let checks = crate::preflight::run(cfg, pf_mode, project);
    if json {
        print_json(&crate::preflight::to_json(&checks));
    } else {
        print!("{}", crate::preflight::render(&checks));
    }
    if !crate::preflight::all_pass(&checks) {
        std::process::exit(1);
    }
    Ok(())
}

/// ADR-0019 D10 hosted park arm. The caller is hosted (`location == hosted`
/// for `--as`, no rex block). New key: write `Prepared` then `Parked` with
/// the typed payload and return handled — no fact, no dispatch. Same key
/// while parked: `in_flight`. Past the bound: settle `Unknown` with
/// `approval_expired` and mail the asker. A denied key replays
/// `approval_denied` and never dispatches. Returns true when the steer is
/// fully answered here; false means the normal gate path continues.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_steer_hosted_park(
    project_owned: &str,
    pin: &str,
    objective: &str,
    direct: bool,
    crew: Option<&str>,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
    request_key: Option<&str>,
) -> Result<bool> {
    let key = request_key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("steer: hosted pin {pin} parks for approval — pass --request-key <key>")
        })?;
    let slot = crate::request_receipts::door_slot(project_owned, "steer", key);
    let payload = crate::request_receipts::SteerPayload {
        objective: objective.to_string(),
        crew: crew.unwrap_or("-").to_string(),
        direct,
        reuse: reuse.unwrap_or("-").to_string(),
        retain,
        retire_orphans,
        project: project_owned.to_string(),
        bead_id: None,
    };
    let digest = payload.digest();
    match crate::request_receipts::load_door_receipt(project_owned, "steer", &slot) {
        None => {
            if crate::request_receipts::has_other_parked_steer(project_owned, pin, &slot) {
                print_json(&json!({
                    "approval": "in_flight",
                    "request_key": key,
                    "slot": slot,
                    "reason": "another steer is already parked for this pin",
                }));
                bail!(
                    "steer: a parked steer is already awaiting approval for pin {pin} — in_flight"
                );
            }
            crate::request_receipts::park_steer(project_owned, &slot, &digest, payload, pin)?;
            print_json(&json!({
                "approval": "required",
                "request_key": key,
                "slot": slot,
                "asker": pin,
            }));
            eprintln!(
                "flywheel: steer parked for operator approval (slot {slot}) — run `flywheel captain approve {project_owned} --request-key {key} --as <operator-pin> --decision approve|deny`"
            );
            Ok(true)
        }
        Some(row) if row.state == agent_ref::EffectState::Parked => {
            let parked_at = row.parked_at_ms.ok_or_else(|| {
                anyhow::anyhow!(
                    "steer: parked row for slot {slot} has no park timestamp — approval_mismatch"
                )
            })?;
            if crate::request_receipts::approval_expired(
                parked_at,
                crate::request_receipts::now_ms(),
            ) {
                let mut expired = row;
                expired.state = agent_ref::EffectState::Unknown;
                expired.evidence = Some("approval_expired".to_string());
                crate::request_receipts::journal_door_row(project_owned, &expired)?;
                crate::mail_approval_notice(project_owned, pin, key)?;
                print_json(&json!({
                    "approval": "expired",
                    "request_key": key,
                    "slot": slot,
                }));
                return Ok(true);
            }
            print_json(&json!({
                "approval": "in_flight",
                "request_key": key,
                "slot": slot,
            }));
            bail!("steer: slot {slot} is parked awaiting approval — in_flight");
        }
        Some(row)
            if row.state == agent_ref::EffectState::Settled
                && row.evidence.as_deref() == Some("denied") =>
        {
            print_json(&json!({
                "approval": "approval_denied",
                "request_key": key,
                "slot": slot,
                "replay": true,
            }));
            eprintln!(
                "flywheel: steer key already denied (slot {slot}) — approval_denied, not dispatching"
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_steer(
    cfg: &Config,
    objective: &str,
    direct: bool,
    crew: Option<&str>,
    project: Option<&str>,
    reuse: Option<&str>,
    retain: bool,
    retire_orphans: bool,
    json: bool,
    yes: bool,
    request_key: Option<&str>,
    as_agent: Option<&str>,
) -> Result<()> {
    if objective.trim().is_empty() {
        bail!("steer: objective must not be empty");
    }
    let explicit_project = project.is_some();
    let project_owned = crate::cron::detect_project(cfg, project).ok_or_else(|| {
        anyhow::anyhow!(
            "steer: --project is required for AUTHORITATIVE MaterialContext path              (pass --project <slug> or run from a workspace/<project> cwd)"
        )
    })?;
    crate::check_project_spelling(&project_owned, "steer")?;
    if !explicit_project {
        eprintln!("flywheel: steer: inheriting --project={project_owned} from workspace cwd");
    }
    let project: Option<&str> = Some(project_owned.as_str());
    let hosted_pin: Option<&str> = match as_agent {
        Some(pin)
            if crate::request_receipts::is_hosted_caller(agent_ref::binding_location_for(
                &project_owned,
                pin,
            )) =>
        {
            Some(pin)
        }
        _ => None,
    };
    if let Some(pin) = hosted_pin
        && cmd_steer_hosted_park(
            &project_owned,
            pin,
            objective,
            direct,
            crew,
            reuse,
            retain,
            retire_orphans,
            request_key,
        )?
    {
        return Ok(());
    }
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    let direct_flag = if direct { "direct" } else { "goal" };
    let retain_flag = if retain { "retain" } else { "reap" };
    let orphans_flag = if retire_orphans { "orphans" } else { "keep" };
    let steer_digest = crate::request_receipts::payload_digest(
        "steer-v1",
        &[
            objective,
            crew.unwrap_or("-"),
            direct_flag,
            reuse.unwrap_or("-"),
            retain_flag,
            orphans_flag,
        ],
    );
    let mut steer_receipt: Option<crate::request_receipts::DoorReceiptGuard> = None;
    if let Some(key) = request_key {
        let slot = crate::request_receipts::door_slot(&project_owned, "steer", key);
        match crate::request_receipts::receipt_gate(
            crate::request_receipts::load_door_receipt(&project_owned, "steer", &slot).as_ref(),
            &steer_digest,
        ) {
            crate::request_receipts::Gate::Proceed => {
                steer_receipt = Some(crate::request_receipts::DoorReceiptGuard::begin(
                    &project_owned,
                    "steer",
                    &slot,
                    &steer_digest,
                )?);
            }
            crate::request_receipts::Gate::Replay => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "request_key": key,
                        "slot": slot,
                        "already_steered": true,
                        "replay": true,
                    }))?
                );
                eprintln!(
                    "flywheel: steer request key {key:?} already settled this slot ({slot}) — \
                     replaying, no second dispatch. Its run: flywheel history --project \
                     {project_owned}."
                );
                return Ok(());
            }
            crate::request_receipts::Gate::InFlight => bail!(
                "steer request key {key:?} is IN FLIGHT (slot {slot}) — a second dispatch could \
                 start a second run under the same objective. Inspect it with `flywheel status \
                 {project_owned}`, or retry with a fresh --request-key once the first settles."
            ),
            crate::request_receipts::Gate::Conflict => bail!(
                "steer request key {key:?} was already used for a DIFFERENT objective or mode (the \
                 recorded payload digest differs) — refusing to hand back another steer's slot \
                 under this key. Use a fresh --request-key for the new steer."
            ),
        }
    }
    let summary = format!("AUTHORITATIVE OPERATOR OBJECTIVE: {objective}");
    if let Err(e) =
        crate::append_pending_fact(&crate::config::state_dir(), &project_owned, &summary)
    {
        if let Some(g) = steer_receipt.take() {
            g.mark_failed(&format!("material fact queue failed: {e:#}"));
        }
        bail!(
            "steer: could not queue material MaterialContext fact for project {project_owned}: {e:#}"
        );
    }
    let mut crew_owned = crew.map(|s| s.to_string());
    let mut role_model_owned: Vec<String> = Vec::new();
    let mut role_harness_owned: Vec<String> = Vec::new();
    if !json && yes && crew.is_some() {
        eprintln!(
            "flywheel: warn: --yes is redundant with an explicit --crew (briefing already skipped)"
        );
    }
    let brief = crate::briefing_enabled(json, yes, crew, std::io::stdin().is_terminal());
    if brief {
        if let Some(p) = project {
            let ws_label = state::load_spawn_meta(p).unwrap_or_else(|| crate::swarm_label(p));
            // Wave11: label-pinned entry (same single world read as assemble).
            let snap =
                crate::snapshot::assemble_with_label(cfg, p, &ws_label, crate::doorbell_now_unix());
            eprintln!("{}", crate::snapshot::bearings_line(&snap));
        }
        let mut ctx =
            crate::steer_briefing_context(objective, if direct { "goal-direct" } else { "goal" })?;
        crate::enrich_suggestions_live(
            &mut ctx,
            objective,
            &crate::authority::RuntimeAuthority::resolve(),
        );
        let outcome = crate::steer_brief::run_briefing(
            &ctx,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
        )?;
        match outcome {
            crate::steer_brief::BriefingOutcome::Abort => {
                if let Some(g) = steer_receipt.take() {
                    g.mark_failed("aborted by the operator at the briefing gate");
                }
                bail!("steer: aborted by the operator at the briefing gate");
            }
            crate::steer_brief::BriefingOutcome::Go {
                budget,
                role_models,
                role_harnesses,
            } => {
                crew_owned = Some(budget.to_string());
                role_model_owned = role_models
                    .iter()
                    .map(|(role, model)| format!("{role}={model}"))
                    .collect();
                role_harness_owned = role_harnesses
                    .iter()
                    .map(|(role, kind)| format!("{role}={kind}"))
                    .collect();
            }
        }
    }
    match crate::check_bead_file_hold(cfg, &project_owned, objective)? {
        crate::HoldVerdict::Clear => {}
        crate::HoldVerdict::Held => {
            if let Some(g) = steer_receipt.take() {
                g.settle_file_held();
            }
            print_json(&json!({
                "file_hold": true,
                "objective": objective,
            }));
            eprintln!("flywheel: steer: target bead is file-held — not dispatching");
            return Ok(());
        }
        crate::HoldVerdict::Unknown(msg) => {
            if let Some(g) = steer_receipt.take() {
                g.mark_unknown(&msg);
            }
            bail!("steer: file-hold check could not notify the holder: {msg}");
        }
    }
    let run = crate::steer_run_cmd(
        objective,
        direct,
        crew_owned.as_deref(),
        project,
        reuse,
        retain,
        retire_orphans,
        json,
        &role_model_owned,
        &role_harness_owned,
    );
    let out = crate::cmd_workflow(cfg, &run);
    if let Some(g) = steer_receipt.take() {
        match &out {
            Ok(()) => g.settle(crate::request_receipts::Delivery::Queued),
            Err(e) => g.mark_unknown(&format!("steer dispatch failed: {e:#}")),
        }
    }
    out
}

/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
/// flywheel ADR-0022 phase 1: the dry-run plan builder.
///
/// Resolves exactly as the real spawn does (`spawn_model_rows` /
/// `bare_model_launch` provenance plus `headless::interactive_model_plan`
/// per item) and returns the `flywheel-plan.v1` without touching any pane,
/// registry, mail, or journal state. Provenance: bare `--model` => User;
/// profile model => Profile; implied slot model => Slot; coupled default =>
/// HarnessDefault; no model at all => `model: null`.
///
/// Pure except for catalog/registry/profile lookups, which are reads.
/// A dry-run never refuses: where the real spawn would bail (unknown
/// profile), the plan builder errors the same way; where it would fall back
/// to "(harness default)", the plan says `model: null`.
/// ADR-0022 phase 2b (bd-7rg0f): refuse a plan with any `blocked` field.
/// Pure decision: returns every blocked seat field with its remedy so the
/// CLI can print them and exit non-zero before launching anything.
/// An all-honored (or all-degraded) plan proceeds.
pub(crate) fn refuse_blocked(plan: &crate::swarm_plan::Plan) -> Result<(), Vec<String>> {
    let mut blocked = Vec::new();
    for seat in &plan.seats {
        for (field_name, field) in [
            ("model", &seat.model),
            ("effort", &seat.effort),
            ("mode", &seat.mode),
            ("phase", &seat.phase),
        ] {
            if let Some(f) = field
                && let crate::swarm_plan::FieldStatus::Blocked { remedy } = &f.status
            {
                blocked.push(format!(
                    "{} {} {} `{}` is blocked: {}",
                    seat.role, seat.harness, field_name, f.value, remedy
                ));
            }
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(blocked)
    }
}

/// ADR-0022 D5 consent gate: a real spawn with a non-empty `needs_consent`
/// refuses without `--yes` (same place as [`refuse_blocked`]). With `--yes`
/// it proceeds; blocked rows stay refused by `refuse_blocked` — a single
/// refusal path, no duplication.
pub(crate) fn consent_gate(plan: &crate::swarm_plan::Plan, yes: bool) -> Result<(), Vec<String>> {
    if yes || plan.needs_consent.is_empty() {
        return Ok(());
    }
    let mut lines = Vec::new();
    for item in &plan.needs_consent {
        lines.push(format!(
            "consent required: {} {} ({})",
            item.seat,
            item.field,
            item.reason.as_str()
        ));
    }
    lines.push(
        "re-run with --yes to proceed (flywheel plan apply does not exist yet — use the equivalent spawn flags instead)"
            .to_string(),
    );
    Err(lines)
}

/// Resolve one seat's model ask into its plan field (ADR-0022 D4 phase
/// 3a). The ask goes through `resolve_ask` (retirement, ambiguity, and
/// registry membership); the flag then goes through the existing launch
/// resolution so the delivery check sees exactly what would launch.
/// Returns the field plus the launch flag, so the caller can reuse the
/// flag for the effort delivery check without resolving twice.
fn resolve_model_field(
    registry: &models::ModelRegistry,
    harness: &str,
    ask: &str,
    from: crate::swarm_plan::Provenance,
    effort: Option<&str>,
) -> (crate::swarm_plan::Field, String) {
    use crate::models::{RegistryMembership, Resolution};
    use crate::swarm_plan::{resolved_field, unresolved_field};
    match registry.resolve_ask(ask, harness) {
        Resolution::Resolved {
            id,
            registry: reg,
            was,
        } => {
            let flag = registry.resolve(&id, harness);
            let launch = crate::headless::interactive_model_plan(harness, &flag, effort);
            let delivered =
                launch.args().iter().any(|a| a == &flag) || launch.env_pin() == Some(flag.as_str());
            let marker = match reg {
                RegistryMembership::Registered => None,
                RegistryMembership::Unregistered => Some("unregistered".to_string()),
            };
            (
                resolved_field(harness, flag.clone(), from, delivered, marker, was),
                flag,
            )
        }
        Resolution::Ambiguous { ask, candidates } => {
            let message = format!(
                "ambiguous model `{ask}` — did you mean one of: {}",
                candidates.join(", ")
            );
            (
                unresolved_field(harness, &ask, from, message),
                String::new(),
            )
        }
        Resolution::Retired { ask, remedy } => {
            (unresolved_field(harness, &ask, from, remedy), String::new())
        }
    }
}

pub(crate) fn build_dry_run_plan(
    cfg: &Config,
    items: &[crate::SpawnItem],
    catalog: &profile::ProfileCatalog,
    registry: &models::ModelRegistry,
    bare_model: &str,
    bare_provider: &str,
) -> Result<crate::swarm_plan::Plan> {
    use crate::swarm_plan::{Provenance, seat_status};
    let mut plan = crate::swarm_plan::Plan::new("swarm");
    for item in items {
        let (harness, model, effort) = if item.profile.is_some() {
            let p = catalog
                .get(item.profile.as_deref().unwrap_or(""))
                .ok_or_else(|| {
                    anyhow!(
                        "unknown profile '@{}' (flywheel profile list)",
                        item.profile.as_deref().unwrap_or("")
                    )
                })?;
            let (harness, resolved) = registry.resolve_profile_route(&p.harness, &p.model)?;
            let model = if resolved.is_empty() {
                None
            } else if p.model.is_empty() {
                // The profile names no model: fall back like a bare item.
                bare_fallback_model(registry, &harness)
            } else {
                Some((resolved, Provenance::Profile))
            };
            let effort = if p.effort.is_empty() {
                None
            } else {
                Some((p.effort.clone(), Provenance::Profile))
            };
            (harness, model, effort)
        } else {
            let harness = spawn_item_kind(cfg, item, catalog, registry)?;
            if bare_model.is_empty() {
                let fallback = bare_fallback_model(registry, &harness);
                (harness, fallback, None)
            } else {
                let lookup = if bare_provider.is_empty() {
                    bare_model.to_string()
                } else {
                    format!("{bare_provider}/{bare_model}")
                };
                // Pass the raw ask: resolve_ask classifies it (unique /
                // ambiguous / unregistered / retired); resolution to the
                // launch flag happens inside resolve_model_field.
                (harness, Some((lookup, Provenance::User)), None)
            }
        };
        let effort_str = effort.as_ref().map(|(value, _)| value.as_str());
        let (model_field, flag) = match model {
            None => (None, String::new()),
            Some((ask, from)) => {
                let (field, flag) = resolve_model_field(registry, &harness, &ask, from, effort_str);
                (Some(field), flag)
            }
        };
        let launch = crate::headless::interactive_model_plan(&harness, &flag, effort_str);
        let (_, effort_field) = seat_status(&harness, None, effort, &launch);
        // Spawn carries no mode/phase ask today: profiles name neither,
        // and stage contracts (the phase source) arrive with workflows.
        // Wired through the field fns so the values flow when a source
        // lands; today they render as null.
        let mode_field = crate::swarm_plan::mode_field(&harness, None);
        let phase_field = crate::swarm_plan::phase_field(&harness, None);
        plan.seats.push(crate::swarm_plan::Seat {
            role: item.short.clone(),
            count: item.count,
            harness,
            model: model_field,
            effort: effort_field,
            mode: mode_field,
            phase: phase_field,
            placement: "pane".to_string(),
        });
    }
    // ADR-0022 D5: the plan carries its computed consent set and the
    // effective policy into `--dry-run --json` and the launch gates below.
    let policy = cfg.consent_policy();
    let est = crate::swarm_plan::estimate_usd_per_mtok(&plan, registry);
    plan.needs_consent = crate::swarm_plan::needs_consent(&plan, &policy, est, cfg.crew_max());
    plan.policy = Some(policy);
    Ok(plan)
}

/// Bare-item model fallback in launch order (`bare_model_launch`): the
/// coupled default first (what the launch actually pins), then the implied
/// slot model, else none (`model: null` in the plan).
fn bare_fallback_model(
    registry: &models::ModelRegistry,
    harness: &str,
) -> Option<(String, crate::swarm_plan::Provenance)> {
    if let Some(id) = registry.coupled_default_model(harness)
        && !id.is_empty()
    {
        return Some((id, crate::swarm_plan::Provenance::HarnessDefault));
    }
    if let Some(slot) = models::implied_slot_name_for_harness(harness)
        .and_then(|name| registry.effective_slot_model(name))
        && !slot.is_empty()
    {
        return Some((slot, crate::swarm_plan::Provenance::Slot));
    }
    None
}

pub(crate) fn cmd_spawn(cfg: &Config, project: &str, opts: SpawnOpts) -> Result<()> {
    check_project_spelling(project, "spawn")?;
    // A mutating door must not pave over a corrupt fleet: refuse first.
    state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let dir = project_dir(cfg, project);
    if !dir.exists() {
        let mut msg = format!(
            "project dir missing: {} (run: flywheel init {project})",
            dir.display()
        );
        if let Some(parent) = dir.parent().filter(|p| p.is_dir())
            && let Ok(rd) = std::fs::read_dir(parent)
        {
            let mut names: Vec<String> = rd
                .filter_map(std::result::Result::ok)
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().to_str().map(String::from))
                .filter(|n| !n.starts_with('.'))
                .collect();
            names.sort();
            if !names.is_empty() {
                msg.push_str(&format!(
                    "
projects available: {}",
                    names
                        .iter()
                        .take(10)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        bail!("{msg}");
    }
    let plane = crate::backend::resolve_plane(cfg);
    crate::backend::factory::require_mux_for_spawn(plane.configured.as_str())?;
    if plane.plane == crate::backend::Plane::Rex {
        #[cfg(feature = "backend-superlogical")]
        {
            if !plane.rex_reachable {
                bail!(
                    "spawn: {} — install the rex CLI or select the fallback (FLYWHEEL_BACKEND=hc)",
                    plane.reason
                );
            }
            return cmd_spawn_rex(cfg, project, opts);
        }
        #[cfg(not(feature = "backend-superlogical"))]
        bail!(
            "spawn: the rex pane plane needs the backend-superlogical feature (this is a lean hc-only build)"
        );
    }
    let kind_count_escape =
        opts.counts.oc.unwrap_or(0) + opts.counts.pi.unwrap_or(0) + opts.counts.gk.unwrap_or(0) > 0
            || !opts.kinds.is_empty()
            || !opts.bare_model.is_empty();
    if kind_count_escape && !opts.escape {
        eprintln!(
            "flywheel: spawn with --oc/--model/--provider is the escape hatch; default multi-lane is `workflow compose --parts wave-swarm --run`. Pass --escape to silence."
        );
    }
    let plan = match recipe_counts(opts.recipe.as_deref(), &opts.counts, &opts.kinds) {
        Ok(p) => p,
        Err(_e) if opts.profile_name.is_some() => Vec::new(),
        Err(e) => return Err(e),
    };
    let catalog = profile::ProfileCatalog::load()?;
    let model_reg = models::ModelRegistry::load_routing()?;
    let profile_plan = match opts.profile_name.as_deref() {
        Some(p) => {
            let profile = catalog
                .get(p)
                .ok_or_else(|| anyhow!("unknown profile '@{p}' (flywheel profile list)"))?;
            Some((p, profile, opts.profile_count))
        }
        None => None,
    };
    let items = spawn_items_from_plan(plan, profile_plan);
    if items.is_empty() {
        bail!("nothing to spawn: pass --oc/--pi/--gk counts, --recipe NAME, or --profile NAME");
    }
    let plan = build_dry_run_plan(
        cfg,
        &items,
        &catalog,
        &model_reg,
        &opts.bare_model,
        &opts.bare_provider,
    )?;
    if opts.dry_run {
        if opts.json {
            print_json(&serde_json::to_value(&plan)?);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).unwrap_or_else(|_| "{}".into())
            );
        }
        return Ok(());
    }
    if let Err(lines) = consent_gate(&plan, opts.yes) {
        for line in &lines {
            eprintln!("consent: {line}");
        }
        anyhow::bail!(
            "spawn refused: {} consent item(s) need operator sign-off — re-run with --yes",
            plan.needs_consent.len()
        );
    }
    if let Err(blocked) = refuse_blocked(&plan) {
        for line in &blocked {
            eprintln!("blocked: {line}");
        }
        anyhow::bail!(
            "spawn refused: {} blocked field(s) would not be honoured — fix the ask (a blocked ask must not launch)",
            blocked.len()
        );
    }
    let base_label = opts
        .label_override
        .as_deref()
        .map(String::from)
        .unwrap_or_else(|| swarm_label(project));
    let request_key = opts
        .request_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let plan = request_receipts::plan_digest(
        &items
            .iter()
            .map(|i| (i.short.clone(), i.count, i.kind.clone()))
            .collect::<Vec<_>>(),
    );
    let raw_label = match request_key {
        Some(key) => request_receipts::label_for_request_key(&base_label, key),
        None => base_label,
    };
    let (label, changed) = slugify_label(&raw_label);
    if changed && opts.label_override.is_some() {
        eprintln!(
            "flywheel: label {:?} normalized to {label:?} (slugs: lowercase + hyphens)",
            opts.label_override.as_deref()
        );
    }
    if label.is_empty() {
        bail!("workspace label must contain at least one alphanumeric character");
    }
    if let Some(key) = request_key {
        match request_receipts::spawn_gate(
            request_receipts::load_spawn_row(project, &label).as_ref(),
            &plan,
        ) {
            request_receipts::Gate::Proceed => {}
            request_receipts::Gate::Replay => {
                print_json(&json!({
                    "request_key": key,
                    "label": label,
                    "already_exists": true,
                    "replay": true,
                }));
                eprintln!(
                    "flywheel: request key {key:?} already settled this slot ({label}) — replaying, \
                     no second dispatch. Its panes: flywheel list --json."
                );
                return Ok(());
            }
            request_receipts::Gate::InFlight => bail!(
                "spawn request key {key:?} is IN FLIGHT (workspace label {label}) — a second \
                 dispatch could fork a crew into one workspace. Inspect it with `flywheel status \
                 {project}`, or retry with a fresh --request-key once the first attempt settles."
            ),
            request_receipts::Gate::Conflict => bail!(
                "spawn request key {key:?} was already used for a DIFFERENT plan (the recorded plan \
                 digest differs) — refusing to hand back another crew's slot under this key. Use a \
                 fresh --request-key for the new plan."
            ),
        }
    }
    if !opts.yes && std::io::stdin().is_terminal() {
        let rows = spawn_model_rows(&items, &catalog, &model_reg, cfg, &opts.bare_model)?;
        let defaults_on = model_reg.defaults.as_ref().is_some_and(|d| d.active());
        eprintln!("flywheel: spawn {project} — resolved models:");
        for (lane, h, m) in &rows {
            eprintln!("  {lane:24} {h:10} {m}");
        }
        if defaults_on {
            eprintln!(
                "  (operator default active: all slot aliases resolve through it — flywheel model clear-default to lift)"
            );
        }
        let go: bool = dialoguer::Confirm::new()
            .with_prompt("dispatch this crew?")
            .default(true)
            .interact()?;
        if !go {
            bail!("spawn: aborted by the operator at the model reconfirm gate");
        }
    }
    if opts.label_override.is_some()
        && let Err(e) = state::save_spawn_meta(project, &label)
    {
        eprintln!("flywheel: cannot save spawn label metadata: {e:#}");
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let split = project_workspaces(&mut c, project)?;
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    let all_agents = c.agent_list()?;
    let mut probe = Client::connect(Some(&cfg.socket_path())).ok();
    let mut dead_owned: Vec<hc::WorkspaceInfo> = Vec::new();
    let mut live_owned: Vec<hc::WorkspaceInfo> = Vec::new();
    for ws in &split.owned {
        let ws_agents: Vec<_> = all_agents
            .iter()
            .filter(|a| a.workspace_id == ws.workspace_id)
            .collect();
        if ws_agents.is_empty() {
            live_owned.push(ws.clone());
            continue;
        }
        if ws_agents
            .iter()
            .any(|a| matches!(a.agent_status.as_str(), "working" | "blocked"))
        {
            live_owned.push(ws.clone());
            continue;
        }
        let mut all_dead = true;
        for a in &ws_agents {
            let kind = a.agent.as_deref().unwrap_or("opencode");
            let name = a.name.as_deref().unwrap_or("<null>");
            let alive = probe_agent_liveness(&mut probe, &a.pane_id, kind, name);
            if alive != Some(false) {
                all_dead = false;
                break;
            }
        }
        if all_dead {
            dead_owned.push(ws.clone());
        } else {
            live_owned.push(ws.clone());
        }
    }
    if !dead_owned.is_empty() {
        let detail: Vec<String> = dead_owned
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: closing {} stale workspace(s) for {project} — all panes process-dead (bd-eign.3 B): {}",
            dead_owned.len(),
            detail.join(", ")
        );
        for ws in &dead_owned {
            if let Err(e) = c.workspace_close(&ws.workspace_id, true) {
                eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
            }
        }
        pane::reap_dead_registry_rows(project, &mut c);
    }
    let orphans = live_owned;
    if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        if opts.retire_orphans {
            eprintln!(
                "flywheel: closing {} pre-existing workspace(s) for {project}: {}",
                orphans.len(),
                detail.join(", ")
            );
            for ws in &orphans {
                if let Err(e) = c.workspace_close(&ws.workspace_id, true) {
                    eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
                }
            }
            pane::reap_dead_registry_rows(project, &mut c);
        } else if opts.join {
            eprintln!("{}", spawn_join_notice(project, &detail.join(", ")));
            let pin = crate::authority::select_agent_identity(None, &|k| std::env::var(k).ok());
            return cmd_arrive(cfg, project, pin.as_deref(), None, None, None);
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes from a killed swarm would race the new one: {}. Close them first (flywheel kill {project} or terminal workspace close <id>), pass --join to attach, or --retire-orphans to close them automatically.",
                orphans.len(),
                detail.join(", ")
            );
        }
    }
    let first_pin = items.first().and_then(|item| {
        let kind = spawn_item_kind(cfg, item, &catalog, &model_reg).ok()?;
        pinned_mail_identity(cfg, project, &agent_name_n(project, &item.short, 1), &kind)
    });
    let mut first_bare: Option<(Vec<String>, Option<String>)> = None;
    let mut first_plan: Option<ProfileLaunchPlan> = match items.first() {
        Some(item) => {
            let kind = spawn_item_kind(cfg, item, &catalog, &model_reg)?;
            match item.profile.as_deref().map(|pn| catalog.get(pn)) {
                Some(Some(p)) => Some(profile_launch_plan(&kind, p, None)?),
                _ => {
                    first_bare = Some(bare_model_launch(
                        &kind,
                        &opts.bare_model,
                        &opts.bare_provider,
                    )?);
                    None
                }
            }
        }
        None => None,
    };
    let mut plane_env = hc::plane_launch_env(cfg, first_pin.as_deref());
    if let Some(m) = first_plan
        .as_ref()
        .and_then(|p| p.env_model_pin.clone())
        .or_else(|| first_bare.as_ref().and_then(|(_, pin)| pin.clone()))
    {
        hc::apply_opencode_model_pin(&mut plane_env, &m)?;
    }
    let spawn_effect = match request_key {
        Some(_) => agent_ref::AgentEffectGuard::begin_with(
            agent_ref::AgentEffect::prepare(project, &label, agent_ref::AgentPhase::Spawn)
                .with_plan_digest(plan.clone()),
        )?,
        None => agent_ref::AgentEffectGuard::begin(project, &label, agent_ref::AgentPhase::Spawn)?,
    };
    let (ws, root) = c.workspace_create(
        Some(&utf8_path(&dir, "workspace dir")?),
        Some(&label),
        !opts.no_focus,
        Some(&plane_env),
    )?;
    let root = root.ok_or_else(|| anyhow!("workspace_create returned no root pane"))?;
    println!(
        "{}",
        if opts.no_focus {
            format!("workspace {} created (background)", ws.workspace_id)
        } else {
            format!("workspace {} created", ws.workspace_id)
        }
    );

    let mut spawned: Vec<Value> = Vec::new();
    let mut booted: Vec<(String, String, String)> = Vec::new();
    let mut registry: Vec<state::SpawnEntry> = Vec::new();
    let mut prev = root.pane_id.clone();
    let mut first = true;
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    let peer_aware = opts.peer_aware && !opts.no_peer_aware;
    let peer_template: Option<String> = if peer_aware {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/peer-aware.md.tpl");
        match std::fs::read_to_string(&p) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "flywheel: cannot read peer-aware template {}: {e}",
                    p.display()
                );
                None
            }
        }
    } else {
        None
    };
    let mut taken_names: std::collections::HashSet<String> = c
        .agent_list()
        .map(|agents| agents.into_iter().filter_map(|a| a.name).collect())
        .unwrap_or_default();
    let integration_statuses = read_integration_statuses();
    let mut preflighted_kinds: HashSet<String> = HashSet::new();
    for item in &items {
        let kind = spawn_item_kind(cfg, item, &catalog, &model_reg)?;
        if preflighted_kinds.insert(kind.clone()) {
            preflight_kind_integration(&mut c, &kind, &integration_statuses);
        }
        for _ in 0..item.count {
            let ordinal = ordinals.entry(item.short.clone()).or_insert(0);
            *ordinal += 1;
            *ordinal = free_spawn_ordinal(project, &item.short, *ordinal, &taken_names);
            let name = agent_name_n(project, &item.short, *ordinal);
            taken_names.insert(name.clone());
            let mail_identity = pinned_mail_identity(cfg, project, &name, &kind);
            if mail_identity.is_none() {
                refuse_unpinned_spawn(
                    &name,
                    item.profile.as_deref(),
                    backend::mail_backend(cfg).is_ok(),
                )?;
            }
            let identity = mail_identity.as_deref().unwrap_or(&name);
            let (mut agent_args, model_pin, env_model_pin): (
                Vec<String>,
                Option<String>,
                Option<String>,
            ) = match &item.profile {
                Some(pname) => {
                    let p = catalog
                        .get(pname)
                        .expect("profile resolved at plan build (bd-eign.13)");
                    let plan = if first {
                        first_plan.take().ok_or_else(|| {
                            anyhow!(
                                "internal: the first agent's plan was resolved before \
                                 workspace.create but is no longer available"
                            )
                        })?
                    } else {
                        profile_launch_plan(&kind, p, None)?
                    };
                    (plan.args, Some(plan.model), plan.env_model_pin)
                }
                None => {
                    let (args, env_pin) = if first {
                        first_bare.take().ok_or_else(|| {
                            anyhow!(
                                "internal: the first agent's bare model plan was resolved before \
                                 workspace.create but is no longer available"
                            )
                        })?
                    } else {
                        bare_model_launch(&kind, &opts.bare_model, &opts.bare_provider)?
                    };
                    (args, None, env_pin)
                }
            };
            let mut pane_env = hc::plane_launch_env(cfg, mail_identity.as_deref());
            if let Some(m) = &env_model_pin {
                hc::apply_opencode_model_pin(&mut pane_env, m)?;
            }
            let pane_id = if first {
                first = false;
                prev.clone()
            } else {
                let p = hc::pane_split(
                    &c,
                    &prev,
                    "down",
                    Some(&utf8_path(&dir, "workspace dir")?),
                    Some(&pane_env),
                )?;
                prev = p.pane_id.clone();
                prev.clone()
            };
            let peer_args: Vec<String> = peer_template
                .as_ref()
                .map(|t| {
                    vec![peer_prompt_template(
                        t,
                        &name,
                        identity,
                        opts.peer_poll_interval_minutes,
                        opts.peer_poll_budget_minutes,
                        opts.peer_max_prompts,
                    )]
                })
                .unwrap_or_else(|| vec![doorbell::identity_prime(&name, identity)]);
            let skills_digest = item
                .profile
                .as_deref()
                .and_then(|pname| catalog.get(pname))
                .and_then(|p| profile::skills_digest(&p.skills));

            let has_peer_prime = !peer_args.is_empty();
            if needs_in_arg_prime(&kind) && !has_peer_prime {
                agent_args.push(boot_prime_line().to_string());
            }
            let shell_ready = hc::wait_shell_prompt(&c, &pane_id, Duration::from_secs(15));
            if !shell_ready {
                eprintln!(
                    "flywheel: warn: shell for {name} ({pane_id}) did not render a prompt \
                     within 15s — the launch command may be mangled by the booting shell"
                );
            }
            let agent = start_agent_with_retry_profile(
                &mut c,
                &name,
                &kind,
                &pane_id,
                &agent_args,
                item.profile.as_deref(),
            )?;
            let ready =
                hc::wait_agent_foreground(&c, &pane_id, &kind, &name, Duration::from_secs(15));
            if !ready {
                eprintln!(
                    "flywheel: warn: {name} not foreground within 15s of spawn — the boot window race (bd-eign.6) still applies; prompts dispatched before the TUI is up may be lost"
                );
            }
            if opts.settle_secs > 0 {
                std::thread::sleep(Duration::from_secs(opts.settle_secs));
            }
            if has_peer_prime {
                prime_line_via_send_text(&mut c, &pane_id, &peer_args[0]);
                if comms_inbox_on() {
                    prime_bootstrap(&mut c, &pane_id, &kind);
                }
            } else {
                prime_line_via_send_text(
                    &mut c,
                    &pane_id,
                    &doorbell::identity_prime(&name, identity),
                );
                prime_bootstrap(&mut c, &pane_id, &kind);
            }
            if !opts.no_verify {
                let live = verify_or_respawn_pane(
                    &mut c,
                    &pane_id,
                    &name,
                    &kind,
                    &agent_args,
                    !has_peer_prime,
                    Duration::from_secs(120),
                )?;
                if !live {
                    bail!(
                        "{name} ({pane_id}) has no live agent process after spawn + recovery — the pane is a dead shell. Inspect it (hc terminal read {pane_id}), then recover manually (flywheel recover {project} {name}) or kill + re-spawn."
                    );
                }
            }
            booted.push((pane_id.clone(), name.clone(), kind.clone()));
            // governed-by: ADR-0001 (mail-register-on-spawn) — the pin was
            bind_pane_mail_identity(
                cfg,
                project,
                &name,
                &agent.pane_id,
                mail_identity.as_deref(),
            );
            note_role_bind_result(
                bind_spawn_role(
                    cfg,
                    project,
                    item.profile.as_deref(),
                    &item.short,
                    mail_identity.as_deref(),
                ),
                project,
            );
            let mut entry = match (&item.profile, &mail_identity) {
                (Some(p), Some(mid)) => state::SpawnEntry::profiled(
                    agent.pane_id.clone(),
                    name.clone(),
                    p.clone(),
                    kind.clone(),
                    Some(mid.clone()),
                ),
                (Some(p), None) => {
                    eprintln!(
                        "flywheel: warn: spawned {name} ({}) profile {p} with no mail identity (mail not configured); doorbell sweep skips unpinned panes",
                        agent.pane_id
                    );
                    state::SpawnEntry::profiled(
                        agent.pane_id.clone(),
                        name.clone(),
                        p.clone(),
                        kind.clone(),
                        None,
                    )
                }
                (None, Some(mid)) => state::SpawnEntry::with_mail_identity(
                    agent.pane_id.clone(),
                    name.clone(),
                    mid.clone(),
                ),
                (None, None) => {
                    eprintln!(
                        "flywheel: warn: spawned {name} ({}) with no mail identity (unpinned legacy entry); doorbell sweep skips unpinned panes — re-register with a mail identity to restore inbox checks",
                        agent.pane_id
                    );
                    state::SpawnEntry::legacy(agent.pane_id.clone(), name.clone())
                }
            };
            entry.model_pin = model_pin;
            entry.skills_digest = skills_digest;
            entry.harness_session = captain_directory::capture_harness_session(&kind);
            registry.push(entry);
            println!(
                "  {} {}",
                kind_paint(&kind),
                dim(&format!("({})", agent.pane_id))
            );
            spawned.push(json!({
                "name": agent.name, "kind": agent.agent, "pane_id": agent.pane_id, "status": agent.agent_status.as_str()
            }));
        }
    }

    log(
        project,
        EventKind::Spawn,
        json!({
            "workspace_id": ws.workspace_id, "label": label, "agents": spawned
        }),
    );

    if let Err(e) = state::save_spawn_registry(project, &registry) {
        eprintln!("flywheel: cannot save spawn registry: {e:#}");
    }
    if let Err(e) = state::save_agent_names(&dir, &registry) {
        eprintln!("flywheel: cannot save .flywheel/agent-names.json: {e:#}");
    }
    if let Err(e) = state::publish_agent_bindings(project, &registry) {
        eprintln!("flywheel: cannot publish shared agent bindings: {e:#}");
    }
    doorbell::register_workspace_subscriptions(
        &config::state_dir(),
        project,
        &label,
        &registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect::<Vec<_>>(),
    );
    {
        use doorbell_rt::subscribe::SubscriptionStore;
        let prefix = format!("spawn-{label}-");
        let landed = SubscriptionStore::open(&config::state_dir())
            .list()
            .iter()
            .filter(|s| s.id.starts_with(&prefix))
            .count();
        let wake = agent_ref::AgentEffect {
            state: if landed == 0 {
                agent_ref::EffectState::Unknown
            } else {
                agent_ref::EffectState::Settled
            },
            evidence: Some(format!("subscribers={landed}")),
            ..agent_ref::AgentEffect::prepare(project, &label, agent_ref::AgentPhase::Wake)
        };
        if let Err(e) = agent_ref::journal_effect(project, &wake) {
            eprintln!("flywheel: warn: cannot journal wake observation for {label}: {e:#}");
        }
        if landed == 0 {
            eprintln!(
                "flywheel: WARNING — no doorbell wake subscription landed for workspace {label} \
                 (subscribers=0): rings / inbox-digest wakes / ack-SLA nudges will NOT be delivered \
                 to this swarm. Check the plane with `flywheel doorbell status`; re-arm with \
                 `doorbell start --host {project}/{label}` (bd-enov/F17)."
            );
        }
    }

    if !opts.no_verify {
        let listed = c.workspace_list()?;
        let created_id = ws.workspace_id.clone();
        if !listed.iter().any(|lws| lws.workspace_id == created_id) {
            bail!(
                "spawn created workspace {created_id} but `flywheel list` cannot see it — hc's session registry (session.json) failed to persist the workspace.
recovery: restart hc (launchctl kickstart -k gui/$(id -u)/com.hc... or `hc restart`), then re-run: flywheel spawn {project}
the panes may still be live in hc; `--no-verify` skips this check in scripts"
            );
        }
    }

    if !opts.no_verify && !registry.is_empty() {
        let routable = |e: &state::SpawnEntry| -> String {
            if pane_takes_hc_stamp(&e.pane_id) {
                e.mail_identity
                    .clone()
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| e.name.clone())
            } else {
                e.name.clone()
            }
        };
        let agents_now = c.agent_list()?;
        for e in &registry {
            if e.pane_id.is_empty() {
                continue;
            }
            let want = routable(e);
            let unbound = agents_now.iter().any(|a| {
                a.pane_id == e.pane_id
                    && (a.name.as_deref().is_none_or(|n| n.is_empty())
                        || a.name.as_deref() != Some(want.as_str()))
            });
            if unbound && let Err(err) = c.agent_rename(&e.pane_id, &want) {
                eprintln!(
                    "flywheel: GS-039 P0-4 name-bind rename {} → {} failed: {err:#}",
                    e.pane_id, want
                );
            }
        }
        let agents_after = c.agent_list()?;
        let still_unbound: Vec<String> = registry
            .iter()
            .filter(|e| !e.pane_id.is_empty())
            .filter(
                |e| match agents_after.iter().find(|a| a.pane_id == e.pane_id) {
                    None => true,
                    Some(a) => {
                        let want = routable(e);
                        a.name.as_deref().is_none_or(|n| n.is_empty())
                            || a.name.as_deref() != Some(want.as_str())
                    }
                },
            )
            .map(|e| {
                let seen = agents_after
                    .iter()
                    .find(|a| a.pane_id == e.pane_id)
                    .and_then(|a| a.name.clone())
                    .unwrap_or_else(|| "<missing>".into());
                format!("{} ({}) saw={seen}", e.name, e.pane_id)
            })
            .collect();
        if !still_unbound.is_empty() {
            bail!(
                "spawn name-binding failed after settle (GS-039 P0-4 / GS-043 AgentRef): panes still unbound/wrong-name: {}. Refusing swarm ready — rebind manually (flywheel rebind {project}) or re-spawn.",
                still_unbound.join(", ")
            );
        }
    }

    if !opts.no_verify {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let agents = c.agent_list()?;
            let pending: Vec<AgentInfo> = agents
                .iter()
                .filter(|a| a.workspace_id == ws.workspace_id && is_still_booting(a))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{}",
                    launch_pending_error(&pending).expect("pending non-empty here")
                );
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        let dead = dead_panes(&booted, |pane_id, kind| {
            hc::pane_process_info(&c, pane_id)
                .ok()
                .and_then(|pi| hc::pane_agent_alive(&pi, kind))
        });
        if !dead.is_empty() {
            let list = dead
                .iter()
                .map(|(_, n, p)| format!("{n} ({p})"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "swarm not ready: {} — no live agent process (dead shell). Re-run spawn, or recover manually (flywheel recover {project} <name> / hc terminal read <id>)",
                list
            );
        }
    }

    if opts.no_verify {
        spawn_effect.mark_unknown(Some("--no-verify: workspace + readiness gates skipped"));
    } else {
        spawn_effect.settle(Some(&format!(
            "workspace={} panes={}",
            ws.workspace_id,
            booted.len()
        )));
    }

    let presence_project = project_dir(cfg, project)
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| project.to_string());
    let (presence_mail, presence_mail_err) = match crate::mail_or_die(cfg) {
        Ok(m) => (Some(m), String::new()),
        Err(e) => (None, format!("{e:#}")),
    };
    publish_presence_for_registry(
        presence_mail.as_ref(),
        &presence_mail_err,
        &presence_project,
        project,
        &registry,
    );

    if !opts.join_channels.is_empty() {
        let pins: Vec<String> = registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        ensure_and_join_channels(project, &opts.join_channels, &pins);
    }

    if peer_aware {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
        doorbell::ensure_running(&exe, &config::state_dir(), &label, project)
            .context("doorbell dispatcher")?;
    }

    println!(
        "swarm ready: flywheel {} {} {}",
        kind_paint("send"),
        dim(project),
        dim("--to all --stage ce-execute")
    );
    Ok(())
}

/// Rex arm of [`cmd_spawn`]: same plan/receipt/registry/presence contract over
/// `&dyn MultiplexerBackend`, never `Client::connect`. Kept as a separate
/// function so the hc path above stays byte-identical and this arm lifts
/// off cleanly. Selection is `REX_SESSION` (set = rex, empty = hc);
/// transport fallback inside `auto` stays Connect/Io/Timeout only.
///
/// Known deltas vs the hc path (all fail-closed or loud, never silent):
/// shell gate bails instead of warning (no verify-or-respawn net on rex),
/// per-kind hc preflight and the process-liveness probe are skipped
/// (hc-client-coupled), and the harness launch-hook bootstrap is not
/// installed (prime goes through `pane_run`).
/// bd-rex-reliability-ioyz D: drain the pre-subscribed event stream for
/// `terminal.child_exited` for `pane`. Raw lines carry the code (the event
/// map keeps child exits unmapped so the code survives); any other event
/// is not an exit. Bounded (~1 s): no event inside the bound is unknown,
/// never success.
#[cfg(feature = "backend-superlogical")]
fn poll_child_exit(
    stream: Option<&mut Box<dyn toron_plane::backend::EventStream>>,
    pane: &str,
) -> Option<i32> {
    let stream = stream?;
    for _ in 0..10_000 {
        match stream.read_event() {
            Ok(Some(toron_plane::backend::BackendEvent::Raw(line))) => {
                if let Some((block, code)) =
                    toron_plane::backend::superlogical::child_exit_code(&line)
                    && block == pane
                {
                    return Some(code);
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    None
}

/// D: the recorded outcome for a child that exited with `code`. Nonzero is a
/// typed failure carrying the code; zero is the one-shot success.
#[cfg(feature = "backend-superlogical")]
fn exited_status(name: &str, pane: &str, code: i32) -> String {
    if code != 0 {
        eprintln!(
            "flywheel: {name} ({pane}) exited with code {code} — headless one-shot typed failure"
        );
    }
    format!("exited: {code}")
}

#[cfg(feature = "backend-superlogical")]
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_spawn_rex(cfg: &Config, project: &str, opts: SpawnOpts) -> Result<()> {
    // A mutating door must not pave over a corrupt fleet: refuse first.
    state::load_spawn_registry_strict(project).map_err(|e| anyhow!("{e}"))?;
    let dir = project_dir(cfg, project);
    let kind_count_escape =
        opts.counts.oc.unwrap_or(0) + opts.counts.pi.unwrap_or(0) + opts.counts.gk.unwrap_or(0) > 0
            || !opts.kinds.is_empty()
            || !opts.bare_model.is_empty();
    if kind_count_escape && !opts.escape {
        eprintln!(
            "flywheel: spawn with --oc/--model/--provider is the escape hatch; default multi-lane is `workflow compose --parts wave-swarm --run`. Pass --escape to silence."
        );
    }
    let plan = match recipe_counts(opts.recipe.as_deref(), &opts.counts, &opts.kinds) {
        Ok(p) => p,
        Err(_e) if opts.profile_name.is_some() => Vec::new(),
        Err(e) => return Err(e),
    };
    let catalog = profile::ProfileCatalog::load()?;
    let model_reg = models::ModelRegistry::load_routing()?;
    let profile_plan = match opts.profile_name.as_deref() {
        Some(p) => {
            let profile = catalog
                .get(p)
                .ok_or_else(|| anyhow!("unknown profile '@{p}' (flywheel profile list)"))?;
            Some((p, profile, opts.profile_count))
        }
        None => None,
    };
    let items = spawn_items_from_plan(plan, profile_plan);
    if items.is_empty() {
        bail!("nothing to spawn: pass --oc/--pi/--gk counts, --recipe NAME, or --profile NAME");
    }
    let plan = build_dry_run_plan(
        cfg,
        &items,
        &catalog,
        &model_reg,
        &opts.bare_model,
        &opts.bare_provider,
    )?;
    if opts.dry_run {
        if opts.json {
            print_json(&serde_json::to_value(&plan)?);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).unwrap_or_else(|_| "{}".into())
            );
        }
        return Ok(());
    }
    if let Err(lines) = consent_gate(&plan, opts.yes) {
        for line in &lines {
            eprintln!("consent: {line}");
        }
        anyhow::bail!(
            "spawn refused: {} consent item(s) need operator sign-off — re-run with --yes",
            plan.needs_consent.len()
        );
    }
    if let Err(blocked) = refuse_blocked(&plan) {
        for line in &blocked {
            eprintln!("blocked: {line}");
        }
        anyhow::bail!(
            "spawn refused: {} blocked field(s) would not be honoured — fix the ask (a blocked ask must not launch)",
            blocked.len()
        );
    }
    let base_label = opts
        .label_override
        .as_deref()
        .map(String::from)
        .unwrap_or_else(|| swarm_label(project));
    let request_key = opts
        .request_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty());
    let plan = request_receipts::plan_digest(
        &items
            .iter()
            .map(|i| (i.short.clone(), i.count, i.kind.clone()))
            .collect::<Vec<_>>(),
    );
    let raw_label = match request_key {
        Some(key) => request_receipts::label_for_request_key(&base_label, key),
        None => base_label,
    };
    let (label, changed) = slugify_label(&raw_label);
    if changed && opts.label_override.is_some() {
        eprintln!(
            "flywheel: label {:?} normalized to {label:?} (slugs: lowercase + hyphens)",
            opts.label_override.as_deref()
        );
    }
    if label.is_empty() {
        bail!("workspace label must contain at least one alphanumeric character");
    }
    if let Some(key) = request_key {
        match request_receipts::spawn_gate(
            request_receipts::load_spawn_row(project, &label).as_ref(),
            &plan,
        ) {
            request_receipts::Gate::Proceed => {}
            request_receipts::Gate::Replay => {
                print_json(&json!({
                    "request_key": key,
                    "label": label,
                    "already_exists": true,
                    "replay": true,
                }));
                eprintln!(
                    "flywheel: request key {key:?} already settled this slot ({label}) — replaying, \
                     no second dispatch. Its panes: flywheel list --json."
                );
                return Ok(());
            }
            request_receipts::Gate::InFlight => bail!(
                "spawn request key {key:?} is IN FLIGHT (workspace label {label}) — a second \
                 dispatch could fork a crew into one workspace. Inspect it with `flywheel status \
                 {project}`, or retry with a fresh --request-key once the first attempt settles."
            ),
            request_receipts::Gate::Conflict => bail!(
                "spawn request key {key:?} was already used for a DIFFERENT plan (the recorded plan \
                 digest differs) — refusing to hand back another crew's slot under this key. Use a \
                 fresh --request-key for the new plan."
            ),
        }
    }
    if !opts.yes && std::io::stdin().is_terminal() {
        let rows = spawn_model_rows(&items, &catalog, &model_reg, cfg, &opts.bare_model)?;
        let defaults_on = model_reg.defaults.as_ref().is_some_and(|d| d.active());
        eprintln!("flywheel: spawn {project} — resolved models:");
        for (lane, h, m) in &rows {
            eprintln!("  {lane:24} {h:10} {m}");
        }
        if defaults_on {
            eprintln!(
                "  (operator default active: all slot aliases resolve through it — flywheel model clear-default to lift)"
            );
        }
        let go: bool = dialoguer::Confirm::new()
            .with_prompt("dispatch this crew?")
            .default(true)
            .interact()?;
        if !go {
            bail!("spawn: aborted by the operator at the model reconfirm gate");
        }
    }
    if opts.label_override.is_some()
        && let Err(e) = state::save_spawn_meta(project, &label)
    {
        eprintln!("flywheel: cannot save spawn label metadata: {e:#}");
    }

    let backend = backend::multiplexer_backend(cfg)?;
    let split = {
        let ws_list = backend.workspace_list()?;
        let agents = backend.agent_list().unwrap_or_default();
        partition_workspaces(
            project,
            ws_list,
            &agents,
            state::load_spawn_meta(project).as_deref(),
        )
    };
    if !split.foreign.is_empty() {
        let detail: Vec<String> = split
            .foreign
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        eprintln!(
            "flywheel: warn: {} foreign workspace(s) share the {project} label but are not flywheel-owned — leaving them alone: {}",
            split.foreign.len(),
            detail.join(", ")
        );
    }
    let orphans = split.owned;
    if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        if opts.retire_orphans {
            eprintln!(
                "flywheel: closing {} pre-existing workspace(s) for {project}: {}",
                orphans.len(),
                detail.join(", ")
            );
            for ws in &orphans {
                if let Err(e) = backend.workspace_close(&ws.workspace_id, true) {
                    eprintln!("flywheel: close {} failed: {e:#}", ws.workspace_id);
                }
            }
            let live: std::collections::HashSet<String> = backend
                .pane_list(None)
                .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
                .unwrap_or_default();
            let (kept, pruned) = crate::pane::reap_closed_rows(
                state::load_spawn_registry(project),
                &live,
                backend.name(),
            );
            if pruned > 0 {
                eprintln!("flywheel: reaped {pruned} dead-pane registry row(s) for {project}");
                if let Err(e) = state::save_spawn_registry(project, &kept) {
                    eprintln!("flywheel: cannot save reaped registry: {e:#}");
                }
            }
        } else if opts.join {
            eprintln!("{}", spawn_join_notice(project, &detail.join(", ")));
            let pin = crate::authority::select_agent_identity(None, &|k| std::env::var(k).ok());
            return cmd_arrive(cfg, project, pin.as_deref(), None, None, None);
        } else {
            bail!(
                "found {} pre-existing workspace(s) for {project} — stale panes from a killed swarm would race the new one: {}. Close them first (flywheel kill {project}), pass --join to attach, or --retire-orphans to close them automatically.",
                orphans.len(),
                detail.join(", ")
            );
        }
    }
    let spawn_effect = match request_key {
        Some(_) => agent_ref::AgentEffectGuard::begin_with(
            agent_ref::AgentEffect::prepare(project, &label, agent_ref::AgentPhase::Spawn)
                .with_plan_digest(plan.clone()),
        )?,
        None => agent_ref::AgentEffectGuard::begin(project, &label, agent_ref::AgentPhase::Spawn)?,
    };
    let ws = backend.workspace_create(Some(&label))?;
    println!(
        "{}",
        if opts.no_focus {
            format!("workspace {} created (background)", ws.workspace_id)
        } else {
            format!("workspace {} created", ws.workspace_id)
        }
    );

    let mut spawned: Vec<Value> = Vec::new();
    let mut booted: Vec<(String, String, String)> = Vec::new();
    let mut registry: Vec<state::SpawnEntry> = Vec::new();
    let panes = backend.pane_list(Some(&ws.workspace_id))?;
    let mut prev = panes
        .first()
        .map(|p| p.pane_id.clone())
        .ok_or_else(|| anyhow!("rex: workspace {} has no panes", ws.workspace_id))?;
    let mut exit_stream =
        toron_plane::backend::superlogical::subscribe_session(&ws.workspace_id).ok();
    if let Some(stream) = exit_stream.as_mut() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(2_000)));
        let live = matches!(stream.read_event(), Ok(Some(_)));
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        if !live {
            eprintln!(
                "flywheel: warn: rex events stream was not live before spawn — a one-shot exit \
                 code will read unknown (no replay on rex events)"
            );
            exit_stream = None;
        }
    }
    if let Some(stream) = exit_stream.as_mut() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    }
    let mut first = true;
    let mut ordinals: BTreeMap<String, u32> = BTreeMap::new();
    let peer_aware = opts.peer_aware && !opts.no_peer_aware;
    let peer_template: Option<String> = if peer_aware {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("integrations/peer-aware.md.tpl");
        match std::fs::read_to_string(&p) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "flywheel: cannot read peer-aware template {}: {e:#}",
                    p.display()
                );
                None
            }
        }
    } else {
        None
    };
    let mut taken_names: std::collections::HashSet<String> = backend
        .agent_list()
        .map(|agents| agents.into_iter().filter_map(|a| a.name).collect())
        .unwrap_or_default();
    for item in &items {
        let kind = spawn_item_kind(cfg, item, &catalog, &model_reg)?;
        for _ in 0..item.count {
            let ordinal = ordinals.entry(item.short.clone()).or_insert(0);
            *ordinal += 1;
            *ordinal = free_spawn_ordinal(project, &item.short, *ordinal, &taken_names);
            let name = agent_name_n(project, &item.short, *ordinal);
            taken_names.insert(name.clone());
            let mail_identity = pinned_mail_identity(cfg, project, &name, &kind);
            if mail_identity.is_none() {
                refuse_unpinned_spawn(
                    &name,
                    item.profile.as_deref(),
                    backend::mail_backend(cfg).is_ok(),
                )?;
            }
            let identity = mail_identity.as_deref().unwrap_or(&name);
            let (mut agent_args, model_pin, env_model_pin): (
                Vec<String>,
                Option<String>,
                Option<String>,
            ) = match &item.profile {
                Some(pname) => {
                    let p = catalog
                        .get(pname)
                        .expect("profile resolved at plan build (bd-eign.13)");
                    let plan = profile_launch_plan(&kind, p, None)?;
                    (plan.args, Some(plan.model), plan.env_model_pin)
                }
                None => {
                    let (args, env_pin) =
                        bare_model_launch(&kind, &opts.bare_model, &opts.bare_provider)?;
                    (args, None, env_pin)
                }
            };
            let pane_env = crate::pane::pane_env_for_launch(
                cfg,
                mail_identity.as_deref(),
                env_model_pin.as_deref(),
            )?;
            let cwd = utf8_path(&dir, "workspace dir")?;
            let peer_args: Vec<String> = peer_template
                .as_ref()
                .map(|t| {
                    vec![peer_prompt_template(
                        t,
                        &name,
                        identity,
                        opts.peer_poll_interval_minutes,
                        opts.peer_poll_budget_minutes,
                        opts.peer_max_prompts,
                    )]
                })
                .unwrap_or_else(|| vec![doorbell::identity_prime(&name, identity)]);
            let has_peer_prime = !peer_args.is_empty();
            if needs_in_arg_prime(&kind) && !has_peer_prime {
                agent_args.push(boot_prime_line().to_string());
            }
            let command = crate::pane::rex_env_command(&pane_env, &kind, &agent_args);
            let pane_id = if first {
                toron_plane::backend::superlogical::create_agent_block(
                    &ws.workspace_id,
                    identity,
                    Some(&cwd),
                    &command,
                )
                .map_err(|e| anyhow!("spawn: rex run for {name} (pin {identity}): {e:#}"))?
            } else {
                toron_plane::backend::superlogical::create_split_block(
                    &ws.workspace_id,
                    &prev,
                    "below",
                    identity,
                    Some(&cwd),
                    &command,
                )
                .map_err(|e| anyhow!("spawn: rex split for {name} (pin {identity}): {e:#}"))?
            };
            first = false;
            prev = pane_id.clone();
            let launched_status: String = match poll_child_exit(exit_stream.as_mut(), &pane_id) {
                Some(code) => exited_status(&name, &pane_id, code),
                None => match backend.agent_wait(
                    &pane_id,
                    &[hc::AgentStatus::Idle, hc::AgentStatus::Working],
                    Some(90_000),
                ) {
                    Ok(a) => a.agent_status.as_str().to_string(),
                    Err(e) => match poll_child_exit(exit_stream.as_mut(), &pane_id) {
                        Some(code) => exited_status(&name, &pane_id, code),
                        None => {
                            eprintln!(
                                "flywheel: warn: {name} not ready within 90s of spawn (dispatching anyway): {e}"
                            );
                            "unknown".to_string()
                        }
                    },
                },
            };
            if opts.settle_secs > 0 {
                std::thread::sleep(Duration::from_secs(opts.settle_secs));
            }
            if let Err(e) = backend.pane_run(&pane_id, &peer_args[0]) {
                eprintln!("flywheel: warn: rex prime for {name} ({pane_id}) failed: {e:#}");
            }
            bind_pane_mail_identity(cfg, project, &name, &pane_id, mail_identity.as_deref());
            note_role_bind_result(
                bind_spawn_role(
                    cfg,
                    project,
                    item.profile.as_deref(),
                    &item.short,
                    mail_identity.as_deref(),
                ),
                project,
            );
            let mut entry = match (&item.profile, &mail_identity) {
                (Some(p), Some(mid)) => state::SpawnEntry::profiled(
                    pane_id.clone(),
                    name.clone(),
                    p.clone(),
                    kind.clone(),
                    Some(mid.clone()),
                ),
                (Some(p), None) => {
                    eprintln!(
                        "flywheel: warn: spawned {name} ({}) profile {p} with no mail identity (mail not configured); doorbell sweep skips unpinned panes",
                        pane_id
                    );
                    state::SpawnEntry::profiled(
                        pane_id.clone(),
                        name.clone(),
                        p.clone(),
                        kind.clone(),
                        None,
                    )
                }
                (None, Some(mid)) => state::SpawnEntry::with_mail_identity(
                    pane_id.clone(),
                    name.clone(),
                    mid.clone(),
                ),
                (None, None) => {
                    eprintln!(
                        "flywheel: warn: spawned {name} ({}) with no mail identity (unpinned legacy entry); doorbell sweep skips unpinned panes — re-register with a mail identity to restore inbox checks",
                        pane_id
                    );
                    state::SpawnEntry::legacy(pane_id.clone(), name.clone())
                }
            };
            entry.model_pin = model_pin;
            entry.skills_digest = item
                .profile
                .as_deref()
                .and_then(|pname| catalog.get(pname))
                .and_then(|p| profile::skills_digest(&p.skills));
            entry.harness_session = captain_directory::capture_harness_session(&kind);
            registry.push(entry);
            println!("  {} {}", kind_paint(&kind), dim(&format!("({})", pane_id)));
            let exit_code: Option<i32> = launched_status
                .strip_prefix("exited:")
                .and_then(|s| s.trim().parse().ok());
            spawned.push(json!({
                "name": name, "kind": kind, "pane_id": pane_id, "status": launched_status, "exit_code": exit_code
            }));
            booted.push((pane_id.clone(), name.clone(), kind.clone()));
        }
    }

    log(
        project,
        EventKind::Spawn,
        json!({
            "workspace_id": ws.workspace_id, "label": label, "agents": spawned
        }),
    );

    if let Err(e) = state::save_spawn_registry(project, &registry) {
        eprintln!("flywheel: cannot save spawn registry: {e:#}");
    }
    if let Err(e) = state::save_agent_names(&dir, &registry) {
        eprintln!("flywheel: cannot save .flywheel/agent-names.json: {e:#}");
    }
    if let Err(e) = state::publish_agent_bindings(project, &registry) {
        eprintln!("flywheel: cannot publish shared agent bindings: {e:#}");
    }
    doorbell::register_workspace_subscriptions(
        &config::state_dir(),
        project,
        &label,
        &registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect::<Vec<_>>(),
    );
    {
        use doorbell_rt::subscribe::SubscriptionStore;
        let prefix = format!("spawn-{label}-");
        let landed = SubscriptionStore::open(&config::state_dir())
            .list()
            .iter()
            .filter(|s| s.id.starts_with(&prefix))
            .count();
        let wake = agent_ref::AgentEffect {
            state: if landed == 0 {
                agent_ref::EffectState::Unknown
            } else {
                agent_ref::EffectState::Settled
            },
            evidence: Some(format!("subscribers={landed}")),
            ..agent_ref::AgentEffect::prepare(project, &label, agent_ref::AgentPhase::Wake)
        };
        if let Err(e) = agent_ref::journal_effect(project, &wake) {
            eprintln!("flywheel: warn: cannot journal wake observation for {label}: {e:#}");
        }
        if landed == 0 {
            eprintln!(
                "flywheel: WARNING — no doorbell wake subscription landed for workspace {label} \
                 (subscribers=0): rings / inbox-digest wakes / ack-SLA nudges will NOT be delivered \
                 to this swarm. Check the plane with `flywheel doorbell status`; re-arm with \
                 `doorbell start --host {project}/{label}` (bd-enov/F17)."
            );
        }
    }

    if !opts.no_verify {
        let listed = backend.workspace_list()?;
        if !listed.iter().any(|lws| lws.workspace_id == ws.workspace_id) {
            bail!(
                "spawn created workspace {} but the backend cannot list it — the session registry failed to persist. Re-run: flywheel spawn {project} \
                 (the panes may still be live; --no-verify skips this check in scripts)",
                ws.workspace_id
            );
        }
    }

    if !opts.no_verify && !registry.is_empty() {
        let routable_label = |e: &state::SpawnEntry| -> String {
            e.mail_identity
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or(e.name.as_str())
                .to_string()
        };
        let agents_now = backend.agent_list()?;
        for e in &registry {
            if e.pane_id.is_empty() {
                continue;
            }
            let routable = routable_label(e);
            let unbound = agents_now
                .iter()
                .any(|a| a.pane_id == e.pane_id && a.name.as_deref() != Some(routable.as_str()));
            if unbound && let Err(err) = backend.agent_rename(&e.pane_id, Some(routable.as_str())) {
                eprintln!(
                    "flywheel: GS-039 P0-4 name-bind rename {} → {routable} failed: {err:#}",
                    e.pane_id
                );
            }
        }
        let agents_after = backend.agent_list()?;
        let still_unbound: Vec<String> = registry
            .iter()
            .filter(|e| !e.pane_id.is_empty())
            .filter(|e| {
                let routable = routable_label(e);
                match agents_after.iter().find(|a| a.pane_id == e.pane_id) {
                    None => true,
                    Some(a) => a.name.as_deref() != Some(routable.as_str()),
                }
            })
            .map(|e| {
                let seen = agents_after
                    .iter()
                    .find(|a| a.pane_id == e.pane_id)
                    .and_then(|a| a.name.clone())
                    .unwrap_or_else(|| "<missing>".into());
                format!("{} ({}) saw={seen}", e.name, e.pane_id)
            })
            .collect();
        if !still_unbound.is_empty() {
            bail!(
                "spawn name-binding failed after settle (GS-039 P0-4 / GS-043 AgentRef): panes still unbound/wrong-name: {}. Refusing swarm ready — rebind manually (flywheel rebind {project}) or re-spawn.",
                still_unbound.join(", ")
            );
        }
    }

    if !opts.no_verify {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let agents = backend.agent_list()?;
            let pending: Vec<AgentInfo> = agents
                .iter()
                .filter(|a| a.workspace_id == ws.workspace_id && is_still_booting(a))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{}",
                    launch_pending_error(&pending).expect("pending non-empty here")
                );
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    if opts.no_verify {
        spawn_effect.mark_unknown(Some("--no-verify: workspace + readiness gates skipped"));
    } else {
        spawn_effect.settle(Some(&format!(
            "workspace={} panes={}",
            ws.workspace_id,
            booted.len()
        )));
    }

    let presence_project = project_dir(cfg, project)
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| project.to_string());
    let (presence_mail, presence_mail_err) = match crate::mail_or_die(cfg) {
        Ok(m) => (Some(m), String::new()),
        Err(e) => (None, format!("{e:#}")),
    };
    publish_presence_for_registry(
        presence_mail.as_ref(),
        &presence_mail_err,
        &presence_project,
        project,
        &registry,
    );

    if !opts.join_channels.is_empty() {
        let pins: Vec<String> = registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        ensure_and_join_channels(project, &opts.join_channels, &pins);
    }

    if peer_aware {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flywheel"));
        doorbell::ensure_running(&exe, &config::state_dir(), &label, project)
            .context("doorbell dispatcher")?;
    }

    println!(
        "swarm ready: flywheel {} {} {}",
        kind_paint("send"),
        dim(project),
        dim("--to all --stage ce-execute")
    );
    Ok(())
}

/// bd-durable-doorbell-lngf: the registered workspaces of `project` that
/// ever ran a dispatcher (the lazy-heal scope). The dispatcher key is the
/// workspace LABEL (`swarm_label`, e.g. `flywheel-flywheel`) — the same key
/// `cmd_spawn` writes pid/gen files under — never the hc pane-id prefix.
pub(crate) fn doorbell_project_workspaces(project: &str) -> Vec<String> {
    let registry = state::load_spawn_registry(project);
    if registry.is_empty() {
        return Vec::new();
    }
    let state_dir = config::state_dir();
    let mut candidates = Vec::new();
    if let Some(meta) = state::load_spawn_meta(project) {
        candidates.push(meta);
    }
    candidates.push(swarm_label(project));
    candidates
        .into_iter()
        .filter(|l| doorbell::state_file(&state_dir, l).exists())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// bd-durable-doorbell-lngf: register the local supervisor —
/// `heal_tick` + an every-minute schedule on the cron host (idempotent by
/// name). This process becomes a supervisor WORKER until it exits; the
/// schedule itself lives server-side, so ticks that land while nothing is
/// connected stay queued and execute at-least-once later. Server
/// unreachable → skip silently; the lazy fallback still covers us.
pub(crate) fn ensure_doorbell_supervisor(cfg: &crate::config::Config, project: &str) {
    if let Err(e) = crate::daemon::install_doorbell_supervisor(cfg, project) {
        eprintln!("flywheel: doorbell supervisor install failed: {e:#}");
    }
}

/// Thin proxy to the daemon CLI when it hosts wake (bd-asr2.5, D1).
/// Lifecycle verbs (status/start/stop) exec `doorbell` — byte-identical
/// output by construction. Read verbs (why/afk/re_inject) stay local: they
/// read the same journal files either way, so output is identical without
/// a proxy hop. Returns true when proxied.
pub(crate) fn proxy_doorbell_verb(cfg: &Config, verb: &str, extra: &[String]) -> Result<bool> {
    let url = match doorbell::resolve_host(&config::state_dir(), cfg.doorbell_url(), None) {
        doorbell::DoorbellHost::Proxy { url } => url,
        doorbell::DoorbellHost::Embedded => return Ok(false),
    };
    let host = url
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    if host != "127.0.0.1" && host != "localhost" {
        anyhow::bail!("flywheel doorbell {verb}: remote daemon {url} is operated out of band");
    }
    let port = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .and_then(|h| h.split(':').nth(1))
        .unwrap_or("18980");
    eprintln!("flywheel doorbell: daemon host {url} — proxying `{verb}`");
    let colocated = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("doorbell")))
        .filter(|p| p.is_file());
    let mut cmd = match colocated {
        Some(p) => std::process::Command::new(p),
        None => std::process::Command::new("doorbell"),
    };
    let st = cmd
        .arg(verb)
        .args(extra)
        .arg("--state-dir")
        .arg(config::state_dir())
        .arg("--port")
        .arg(port)
        .status();
    let st = match st {
        Ok(st) => st,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "flywheel doorbell: `doorbell` binary not on PATH nor beside {} — degraded local handling (install with `cargo install --path crates/doorbell-cli`): {e:#}",
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.display().to_string()))
                    .unwrap_or_else(|| "?".into())
            );
            return Ok(false);
        }
        Err(e) => {
            anyhow::bail!("exec `doorbell` (is doorbell-cli installed on PATH?): {e:#}")
        }
    };
    if !st.success() {
        anyhow::bail!("doorbell {verb} failed: {st}");
    }
    Ok(true)
}

#[cfg(test)]
mod spawn_profile_plan_tests {
    use super::*;
    use crate::config::lock_env;
    use crate::{
        SpawnItem, bare_model_launch, interactive_pane_args, profile_launch_args,
        profile_launch_plan, spawn_items_from_plan, spawn_model_rows,
    };

    fn profile(harness: &str, model: &str) -> profile::Profile {
        profile::Profile {
            harness: harness.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_plan_passes_through() {
        let plan = vec![("oc".to_string(), 2), ("pi".to_string(), 1)];
        let items = spawn_items_from_plan(plan, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].short, "oc");
        assert_eq!(items[0].count, 2);
        assert!(items[0].kind.is_none());
        assert!(items[0].profile.is_none());
        assert!(items[1].profile.is_none());
    }

    #[test]
    fn profile_item_carries_harness_and_name_base() {
        let items = spawn_items_from_plan(
            vec![],
            Some(("coder", &profile("opencode", "deepseek-v4-flash"), 2)),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].short, "coder");
        assert_eq!(items[0].count, 2);
        assert_eq!(items[0].kind.as_deref(), Some("opencode"));
        assert_eq!(items[0].profile.as_deref(), Some("coder"));
    }

    #[test]
    fn profile_count_floor_is_one() {
        let items = spawn_items_from_plan(vec![], Some(("coder", &profile("opencode", "m"), 0)));
        assert_eq!(items[0].count, 1);
    }

    #[test]
    fn profile_merges_with_plain_for_mixed_crews() {
        let items = spawn_items_from_plan(
            vec![("oc".to_string(), 1)],
            Some(("reviewer", &profile("pi", "smol"), 1)),
        );
        assert_eq!(items.len(), 2);
        assert!(items[0].profile.is_none());
        assert_eq!(items[1].profile.as_deref(), Some("reviewer"));
        assert_eq!(items[1].kind.as_deref(), Some("pi"));
    }

    /// Registry provider must prefix the spawn flag (coder → opencode-go,
    /// not a bare id that omp then binds to provider `deepseek`).
    #[test]
    fn launch_args_resolve_registry_provider() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        std::fs::write(
            cfg_home.join("flywheel/models.toml"),
            "[models.\"opencode-go/deepseek-v4.1-flash\"]\n",
        )
        .expect("models.toml");
        let p = profile("pi", "deepseek-v4.1-flash");
        let args = profile_launch_args("pi", &p, None).expect("launch args");
        assert_eq!(
            args,
            vec!["--model", "opencode-go/deepseek-v4.1-flash"],
            "spawn must inject registry provider, not bare id: {args:?}"
        );
    }

    #[test]
    fn launch_args_follow_harness_forms() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let p = profile("opencode", "deepseek-v4.1-flash");
        assert_eq!(
            profile_launch_plan("opencode", &p, None)
                .expect("opencode")
                .args,
            Vec::<String>::new()
        );
        assert_eq!(
            profile_launch_plan("opencode", &p, None)
                .expect("opencode")
                .env_model_pin
                .as_deref(),
            Some("opencode-go/deepseek-v4.1-flash"),
            "the pin must be delivered by pane env, not dropped"
        );
        assert!(
            profile_launch_args("opencode", &p, None).is_err(),
            "the argv-only view must refuse rather than silently drop the pin"
        );
        let pi = profile("pi", "glm-5.3");
        assert_eq!(
            profile_launch_args("pi", &pi, None).expect("pi"),
            vec!["--model", "zai/glm-5.3"]
        );
        let mut pi = profile("pi", "zai/glm-5.3");
        pi.effort = "max".into();
        pi.smol = "zai/glm-4.5-air".into();
        assert_eq!(
            profile_launch_args("pi", &pi, None).expect("pi"),
            vec!["--model", "zai/glm-5.3", "--thinking", "max"]
        );
        let mut unknown = profile("hermes", "glm-5.3");
        unknown.effort = "max".into();
        assert_eq!(
            profile_launch_args("hermes", &unknown, None).expect("unknown harness, routable model"),
            [] as [String; 0]
        );
    }

    /// ADR-0013 P1-7: the plan's pin is the CONCRETE registry route the argv
    /// embeds — a declared `@slot` alias is expanded before it is recorded, so
    /// the spawn ledger can never pin a pane to an unresolved alias.
    #[test]
    fn launch_plan_pin_is_the_resolved_route_not_the_slot_alias() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let slot = profile("pi", "@standard");
        let plan = profile_launch_plan("pi", &slot, None).expect("standard slot plan");
        let (args, pin) = (&plan.args, &plan.model);
        assert!(
            !pin.starts_with('@'),
            "the pin is the resolved route, never the slot alias: {pin}"
        );
        assert_eq!(
            pin, "opencode-go/muse-spark-1.3-contributor",
            "the compiled-in `standard` slot resolves to its Muse pin"
        );
        assert!(
            args.iter().any(|a| a.contains(pin.as_str())),
            "the argv embeds the recorded pin: {args:?} vs {pin}"
        );
        let plan = profile_launch_plan("pi", &profile("pi", "zai/glm-5.3"), None)
            .expect("concrete model plan");
        let (args, pin) = (&plan.args, &plan.model);
        assert_eq!(pin, "zai/glm-5.3");
        assert_eq!(*args, vec!["--model", "zai/glm-5.3"]);
    }

    /// STRICT routing: a profile model that is NOT a registry id fails the
    /// launch naming the fix — nothing bypasses the registry (the old
    /// passthrough spawned broken bare ids).
    /// bd-62ke: an explicit Profile.provider pins the provider half of the
    /// composite registry key (bd-1t3j) — it overrides the registry's
    /// default-provider fallback when one model id exists under multiple
    /// providers, and an unknown provider fails loudly naming the pair.
    #[test]
    fn launch_args_explicit_provider_wins_over_default() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        std::fs::write(
            cfg_home.join("flywheel/models.toml"),
            "[models.\"aaa/glm-5.2\"]\n[models.\"zzz/glm-5.2\"]\n",
        )
        .expect("models.toml");
        let bare = profile("pi", "glm-5.2");
        assert_eq!(
            profile_launch_args("pi", &bare, None).expect("bare"),
            vec!["--model", "aaa/glm-5.2"]
        );
        let mut pinned = profile("pi", "glm-5.2");
        pinned.provider = "zzz".into();
        assert_eq!(
            profile_launch_args("pi", &pinned, None).expect("pinned"),
            vec!["--model", "zzz/glm-5.2"]
        );
        let mut ghost = profile("pi", "glm-5.2");
        ghost.provider = "qqq".into();
        assert_eq!(
            profile_launch_args("pi", &ghost, None).expect("unknown provider passes through"),
            vec!["--model", "qqq/glm-5.2"],
            "explicit unregistered pair launches verbatim"
        );
    }

    /// bd-j3so: a BARE harness + provider/model pair spawns without a named
    /// profile. Lenient routing by design — an id REGISTERED in the composite
    /// registry routes through it (default-provider fallback, explicit
    /// provider pin), an unregistered pair passes through verbatim (the
    /// operator named it explicitly; no profile indirection to protect).
    #[test]
    fn bare_harness_model_pair_routes_without_profile() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        std::fs::write(
            cfg_home.join("flywheel/models.toml"),
            "[models.\"aaa/glm-5.2\"]\n[models.\"zzz/glm-5.2\"]\n",
        )
        .expect("models.toml");
        let (args, pin) = bare_model_launch("pi", "glm-5.2", "").expect("bare id");
        assert_eq!(
            args,
            vec!["--model", "aaa/glm-5.2"],
            "registered bare id routes"
        );
        assert_eq!(pin, None, "pi carries no env pin");
        let (args, _) = bare_model_launch("pi", "glm-5.2", "zzz").expect("pinned");
        assert_eq!(
            args,
            vec!["--model", "zzz/glm-5.2"],
            "explicit provider wins"
        );
        let (args, _) = bare_model_launch("pi", "ghost-model", "").expect("lenient passthrough");
        assert_eq!(args, vec!["--model", "ghost-model"]);
        let (args, _) = bare_model_launch("pi", "ghost-model", "qqq").expect("pair passthrough");
        assert_eq!(args, vec!["--model", "qqq/ghost-model"]);
        assert_eq!(
            bare_model_launch("pi", "", "").expect("empty model").0,
            vec!["--model", "opencode-go/muse-spark-1.3-contributor"],
            "bare pi without --model uses the standard slot"
        );
    }

    /// bd-oc-bare-spawn-model-argv-zbzt: the bare kind/count lane launches
    /// through `hc agent start --kind <harness>` exactly like the profile
    /// lane, so it must deliver the model the SAME way — argv for harnesses
    /// that take a flag, pane env for opencode, whose ROOT command rejects
    /// `--model` outright (`ERROR Unrecognized flag: --model in command
    /// opencode`, measured live 2026-10-07 on v2.0.24; the corpse pane died
    /// before it could draw its TUI).
    #[test]
    fn bare_opencode_spawn_never_emits_model_argv() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let (args, env_pin) =
            bare_model_launch("opencode", "", "").expect("opencode cheap implied");
        assert!(
            !args.iter().any(|a| a == "--model"),
            "opencode's root command rejects --model; argv must carry none: {args:?}"
        );
        assert_eq!(
            args,
            [] as [String; 0],
            "no argv at all reaches the opencode root command: {args:?}"
        );
        assert_eq!(
            env_pin.as_deref(),
            Some("opencode/big-pickle"),
            "the implied cheap slot must still reach the pane, by env"
        );
        let (args, env_pin) =
            bare_model_launch("opencode", "muse-spark-1.3-contributor", "opencode-go")
                .expect("explicit bare pair");
        assert_eq!(args, [] as [String; 0], "still no argv: {args:?}");
        assert_eq!(
            env_pin.as_deref(),
            Some("opencode-go/muse-spark-1.3-contributor")
        );
        let (pi_args, pi_env) = bare_model_launch("pi", "", "").expect("pi standard implied");
        assert_eq!(
            pi_args,
            vec!["--model", "opencode-go/muse-spark-1.3-contributor"],
            "pi takes its model as argv"
        );
        assert_eq!(pi_env, None, "pi needs no env pin");
    }

    /// bd-oc-bare-spawn-model-argv-zbzt: the rate-limit fallback hops re-launch
    /// INTO an existing pane, so they use the argv-only view — which must be
    /// the INTERACTIVE form (opencode takes no model flag on the command hc
    /// starts) and must REFUSE an env-delivered pin instead of silently
    /// launching a seat whose env already says something else.
    #[test]
    fn interactive_pane_args_is_argv_only_and_refuses_env_pins() {
        assert_eq!(
            interactive_pane_args("pi", "opencode-go/muse-spark-1.3-contributor", None)
                .expect("pi argv"),
            vec!["--model", "opencode-go/muse-spark-1.3-contributor"]
        );
        assert_eq!(
            interactive_pane_args("grok", "grok-4.5", None).expect("grok argv"),
            vec!["-m", "grok-4.5"]
        );
        let err = interactive_pane_args("opencode", "opencode/big-pickle", None)
            .expect_err("an env pin cannot ride argv into an existing pane");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no model flag") && msg.contains("re-spawn the pane"),
            "the refusal must name the reason and the remedy: {msg}"
        );
    }

    /// ADR-0007 D1 (implicit): a bare kind/count spawn with NO --model rides
    /// the harness's implied slot (opencode→cheap, pi→standard, grok→premium).
    #[test]
    fn bare_spawn_without_model_uses_implied_slot() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let (args, env_pin) = bare_model_launch("opencode", "", "").expect("cheap implied");
        assert!(
            args.is_empty() && env_pin.as_deref() == Some("opencode/big-pickle"),
            "opencode spawn without --model uses the cheap slot, by env: {args:?} {env_pin:?}"
        );
        let (args, env_pin) = bare_model_launch("pi", "", "").expect("standard implied");
        assert_eq!(
            args,
            vec!["--model", "opencode-go/muse-spark-1.3-contributor"],
            "pi spawn without --model uses the standard slot: {args:?}"
        );
        assert_eq!(env_pin, None, "pi rides argv");
        let err = bare_model_launch("grok", "", "").expect_err("closed premium");
        assert!(
            err.to_string().contains("grok"),
            "the refusal names the harness that cannot be routed: {err}"
        );
        assert_eq!(
            bare_model_launch("kind-without-slot", "", "")
                .expect("unknown kind has no implied slot"),
            (vec![], None)
        );
    }

    /// bd-ovew: bare grok spawn must never imply the premium zai id — the
    /// grok harness runs only its own models. Zero-config (nothing
    /// grok-family registered) fails loud naming --model; a registered
    /// grok id routes in bare flag form.
    #[test]
    fn bare_grok_spawn_never_implies_foreign_model() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let err = bare_model_launch("grok", "", "")
            .expect_err("bare grok without a registered grok model must fail loud");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--model") && !msg.contains("glm"),
            "loud error names --model, leaks no zai id: {msg}"
        );
        std::fs::write(
            cfg_home.join("flywheel/models.toml"),
            "[models.\"grok/grok-4.5\"]\n",
        )
        .expect("models.toml");
        let (args, pin) = bare_model_launch("grok", "", "").expect("grok routed");
        assert_eq!(
            args,
            vec!["-m", "grok-4.5"],
            "registered grok id routes, no premium leak: {args:?}"
        );
        assert_eq!(pin, None, "grok rides argv");
    }

    /// bd-ll3k: the reconfirm table resolves profile and kind lanes
    /// through the registry, collapsing onto `[defaults]` when set.
    #[test]
    fn spawn_model_rows_show_resolved_pairs_and_defaults() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let cfg = Config::default();
        let catalog = profile::ProfileCatalog::load_from(None).expect("catalog");
        let mut reg = models::ModelRegistry::load_routing().expect("routing");
        let items = vec![
            SpawnItem {
                short: "coder".into(),
                count: 2,
                kind: None,
                profile: Some("coder".into()),
            },
            SpawnItem {
                short: "oc".into(),
                count: 1,
                kind: None,
                profile: None,
            },
        ];
        let rows = spawn_model_rows(&items, &catalog, &reg, &cfg, "").expect("rows");
        assert_eq!(
            rows[0],
            (
                "@coder x2".to_string(),
                "pi".to_string(),
                "opencode-go/muse-spark-1.3-contributor".to_string()
            ),
            "coder resolves the standard slot: {rows:?}"
        );
        assert_eq!(
            rows[1],
            (
                "oc x1".to_string(),
                "opencode".to_string(),
                "opencode/big-pickle".to_string()
            ),
            "kind lane rides its implied slot: {rows:?}"
        );
        reg.defaults = Some(models::DefaultsSpec {
            harness: "pi".to_string(),
            model: "opencode-go/muse-spark-1.3-contributor".to_string(),
            fallback: None,
        });
        let rows = spawn_model_rows(&items, &catalog, &reg, &cfg, "").expect("rows");
        assert!(
            rows.iter()
                .all(|(_, _, m)| m == "opencode-go/muse-spark-1.3-contributor"),
            "defaults collapse every lane: {rows:?}"
        );
    }

    /// ADR-0022 D4 phase 3a (bd-uivb4): the strictness split ends. An
    /// unregistered explicit id passes through (the plan marks it
    /// `registry: unregistered`); caps exclusion still refuses.
    #[test]
    fn launch_args_unregistered_model_passes_through_marked() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let p = profile("omp", "totally-unknown-model");
        let args = profile_launch_args("pi", &p, None).expect("unknown id passes through");
        assert_eq!(
            args,
            vec!["--model", "totally-unknown-model"],
            "unregistered id launches verbatim: {args:?}"
        );
        let bp = profile("pi", "big-pickle");
        let err = profile_launch_args("pi", &bp, None).expect_err("caps must refuse");
        assert!(
            err.to_string().contains("not routable"),
            "caps exclusion is an error: {err}"
        );
        assert_eq!(
            profile_launch_plan("opencode", &profile("opencode", "big-pickle"), None)
                .expect("capped harness ok")
                .env_model_pin
                .as_deref(),
            Some("opencode/big-pickle")
        );
        let mut role = profile("pi", "zai/glm-4.5-air");
        role.smol = "ghost-role".into();
        assert_eq!(
            profile_launch_args("pi", &role, None).expect("unknown id passes through"),
            vec!["--model", "zai/glm-4.5-air"],
            "unregistered model launches verbatim (smol is advisory here)"
        );
    }

    #[test]
    fn launch_args_precede_peer_text() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let p = profile("opencode", "deepseek-v4.1-flash");
        let plan = profile_launch_plan("opencode", &p, None).expect("launch args");
        let mut args = plan.args.clone();
        args.extend(vec!["peer template".to_string()]);
        assert_eq!(args, vec!["peer template"]);
        assert_eq!(
            plan.env_model_pin.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash"),
            "the model still reaches the seat, by env"
        );
    }

    #[test]
    fn skill_prefix_built_once_for_fresh_profiled_agent() {
        let entry = state::SpawnEntry::profiled(
            "w1:p1",
            "flywheel-flywheel-coder",
            "coder",
            "opencode",
            None,
        );
        let skills = vec!["code-quality".to_string(), "tdd".to_string()];
        let p =
            skill_injection_prefix(&entry, &skills).expect("fresh profiled agent gets the prefix");
        assert_eq!(
            p,
            "Read skill://code-quality skill://tdd before starting.\n\n"
        );
        let mut injected = entry;
        injected.skills_injected = true;
        assert!(skill_injection_prefix(&injected, &skills).is_none());
    }

    #[test]
    fn skill_prefix_skips_legacy_and_skillless() {
        let legacy = state::SpawnEntry::legacy("w1:p1", "flywheel-flywheel-oc");
        assert!(skill_injection_prefix(&legacy, &["x".to_string()]).is_none());
        let profiled = state::SpawnEntry::profiled(
            "w1:p1",
            "flywheel-flywheel-coder",
            "coder",
            "opencode",
            None,
        );
        assert!(skill_injection_prefix(&profiled, &[]).is_none());
    }
}

#[cfg(test)]
mod profile_run_spawn_tests {
    use super::*;
    use crate::config::lock_env;
    use crate::{
        profile_launch_args, project_dir_str, record_spawn_entry, resolved_spec_workspace,
    };

    fn profile(harness: &str, model: &str) -> profile::Profile {
        profile::Profile {
            harness: harness.into(),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Hermetic cfg: temp workspace root; the state dir must be isolated via
    /// XDG_STATE_HOME (state::state_dir honors it — config.rs 193.2), and
    /// the env lock serializes against other env-mutating tests.
    fn hermetic() -> (Config, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_home = dir.path().join("state");
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        let cfg = Config {
            workspace_root: Some(dir.path().join("ws").to_string_lossy().into_owned()),
            ..Default::default()
        };
        (cfg, dir)
    }

    #[test]
    fn profile_run_launch_args_non_empty_for_known_profile() {
        let _guard = crate::config::lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let mut coder = profile("pi", "zai/glm-5.3");
        coder.effort = "max".into();
        let args = profile_launch_args("pi", &coder, None).expect("launch args");
        assert!(
            !args.is_empty(),
            "known profile must inject model/effort args"
        );
        assert!(
            args.iter().any(|a| a.contains("glm-5.3")),
            "model flag present: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.contains("max")),
            "effort flag present: {args:?}"
        );
    }

    /// bd-ude8: `resolved_spec_workspace` must return a PANE id — `profile
    /// run`/`squad spawn` hand it to `agent_start` as the pane, and hc
    /// rejects a workspace id with `agent_pane_not_found`.
    #[test]
    fn resolved_spec_workspace_returns_pane_id() {
        use crate::backend::MultiplexerBackend;
        use crate::hc::{PaneInfo, WorkspaceInfo};
        use toron_plane::backend::types::AgentStatus;

        struct MockMux {
            ws: Vec<WorkspaceInfo>,
            panes: Vec<PaneInfo>,
        }
        impl MultiplexerBackend for MockMux {
            fn name(&self) -> &'static str {
                "mock-mux"
            }
            fn workspace_list(
                &self,
            ) -> Result<Vec<WorkspaceInfo>, toron_plane::backend::BackendError> {
                Ok(self.ws.clone())
            }
            fn workspace_create(
                &self,
                _label: Option<&str>,
            ) -> Result<WorkspaceInfo, toron_plane::backend::BackendError> {
                let id = "ws-1".to_string();
                Ok(WorkspaceInfo {
                    workspace_id: id,
                    number: 1,
                    label: "mock".into(),
                    focused: false,
                    pane_count: 1,
                    tab_count: 1,
                    active_tab_id: "t1".into(),
                    agent_status: AgentStatus::Idle,
                    cwd: None,
                })
            }
            fn pane_list(
                &self,
                workspace_id: Option<&str>,
            ) -> Result<Vec<PaneInfo>, toron_plane::backend::BackendError> {
                let ws = workspace_id.unwrap_or_default();
                Ok(self
                    .panes
                    .iter()
                    .filter(|p| p.workspace_id == ws)
                    .cloned()
                    .collect())
            }
        }
        fn pane(ws: &str, n: u32) -> PaneInfo {
            PaneInfo {
                pane_id: format!("{ws}:p{n}"),
                terminal_id: format!("term-{n}"),
                workspace_id: ws.into(),
                tab_id: format!("{ws}:t1"),
                focused: false,
                agent_status: AgentStatus::Idle,
                revision: 0,
                title: None,
                cwd: None,
                agent: None,
                agent_session: None,
                display_agent: None,
                foreground_cwd: None,
                label: None,
                scroll: None,
                state_labels: serde_json::Map::new(),
                terminal_title: None,
                terminal_title_stripped: None,
                tokens: serde_json::Map::new(),
            }
        }
        let _g = lock_env();
        let (cfg, _dir) = hermetic();

        let mux = MockMux {
            ws: vec![WorkspaceInfo {
                workspace_id: "wA".into(),
                number: 1,
                label: "proj".into(),
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "wA:t1".into(),
                agent_status: AgentStatus::Idle,
                cwd: None,
            }],
            panes: vec![pane("wA", 1)],
        };
        let got = resolved_spec_workspace(&cfg, &mux, "omp").expect("resolves");
        assert_eq!(got, "wA:p1", "must return a pane id, not workspace id");

        let mux2 = MockMux {
            ws: vec![],
            panes: vec![pane("ws-1", 1)],
        };
        let got2 = resolved_spec_workspace(&cfg, &mux2, "omp").expect("resolves");
        assert_eq!(got2, "ws-1:p1", "created workspace's root pane");
    }

    #[test]
    fn record_spawn_entry_records_profile_for_conditioning() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let entry = state::SpawnEntry::profiled(
            "w1:p1",
            "profile-coder",
            "coder",
            "omp",
            Some("loud-lion".into()),
        );
        record_spawn_entry(&cfg, "flywheel", entry);
        let key = project_dir_str(&cfg, "flywheel").expect("key");
        let registry = state::load_spawn_registry(&key);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry[0].name, "profile-coder");
        assert_eq!(registry[0].profile.as_deref(), Some("coder"));
        assert_eq!(registry[0].harness.as_deref(), Some("omp"));
    }

    #[test]
    fn record_spawn_entry_preserves_others_and_replaces_same_name() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p1", "profile-coder", "coder", "omp", None),
        );
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p2", "profile-planner", "planner", "omp", None),
        );
        record_spawn_entry(
            &cfg,
            "flywheel",
            state::SpawnEntry::profiled("w1:p9", "profile-coder", "coder", "omp", None),
        );
        let key = project_dir_str(&cfg, "flywheel").expect("key");
        let registry = state::load_spawn_registry(&key);
        assert_eq!(registry.len(), 2, "coder row replaced, planner preserved");
        let coder = registry
            .iter()
            .find(|e| e.name == "profile-coder")
            .expect("coder row present");
        assert_eq!(
            coder.pane_id, "w1:p9",
            "re-spawn replaces the same-name row"
        );
        assert_eq!(coder.profile.as_deref(), Some("coder"));
        assert!(
            registry.iter().any(|e| e.name == "profile-planner"),
            "unrelated rows preserved"
        );
    }
}

#[cfg(test)]
mod internal_respawn_tests {
    use super::*;
    use crate::backend::MultiplexerBackend;
    use crate::cmd::internal::{respawn_launch_args, respawn_with_backend};
    use crate::config::lock_env;
    use crate::hc::AgentStatus;
    use crate::hc::{PaneInfo, WorkspaceInfo};
    use crate::project_dir_str;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hermetic cfg: temp workspace root + temp XDG dirs. `XDG_CONFIG_HOME`
    /// isolates the profile catalog (profiles.toml), `XDG_STATE_HOME` the
    /// state dir (spawn registry).
    fn hermetic() -> (Config, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_home = dir.path().join("state");
        let config_home = dir.path().join("config");
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config_home) };
        std::fs::create_dir_all(config_home.join("flywheel")).expect("config dir");
        let cfg = Config {
            workspace_root: Some(dir.path().join("ws").to_string_lossy().into_owned()),
            ..Default::default()
        };
        (cfg, dir)
    }

    fn write_profiles_toml(dir: &tempfile::TempDir) {
        std::fs::write(
            dir.path().join("config/flywheel/profiles.toml"),
            "[profiles.pi-coder]\nharness = \"pi\"\nmodel = \"zai/glm-5.3\"\neffort = \"max\"\nsmol = \"zai/glm-4.5-air\"\n",
        )
        .expect("profiles.toml");
        std::fs::write(
            dir.path().join("config/flywheel/models.toml"),
            "[models.\"zai/glm-4.5-air\"]\n",
        )
        .expect("models.toml");
    }

    fn agent(name: &str, pane: &str, ws: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            agent: Some(name.to_string()),
            agent_status: status,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: ws.to_string(),
        }
    }

    fn pane(ws: &str, n: u32) -> PaneInfo {
        PaneInfo {
            pane_id: format!("{ws}:p{n}"),
            terminal_id: format!("term-{n}"),
            workspace_id: ws.into(),
            tab_id: format!("{ws}:t1"),
            focused: false,
            agent_status: AgentStatus::Idle,
            revision: 0,
            title: None,
            cwd: None,
            agent: None,
            agent_session: None,
            display_agent: None,
            foreground_cwd: None,
            label: None,
            scroll: None,
            state_labels: serde_json::Map::new(),
            terminal_title: None,
            terminal_title_stripped: None,
            tokens: serde_json::Map::new(),
        }
    }

    fn workspace(label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: "wA".into(),
            number: 1,
            label: label.into(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: "wA:t1".into(),
            agent_status: AgentStatus::Idle,
            cwd: None,
        }
    }

    struct MockMux {
        ws_label: String,
        agents: Mutex<Vec<AgentInfo>>,
        started: Mutex<Vec<StartedAgent>>,
        splits: AtomicUsize,
        closes: AtomicUsize,
    }

    /// (name, kind, pane_id, args) captured from each `agent_start`.
    type StartedAgent = (String, String, String, Vec<String>);

    impl MockMux {
        fn new(ws_label: &str, agents: Vec<AgentInfo>) -> Self {
            Self {
                ws_label: ws_label.into(),
                agents: Mutex::new(agents),
                started: Mutex::new(Vec::new()),
                splits: AtomicUsize::new(0),
                closes: AtomicUsize::new(0),
            }
        }
    }

    impl MultiplexerBackend for MockMux {
        fn name(&self) -> &'static str {
            "mock-mux"
        }
        fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, toron_plane::backend::BackendError> {
            Ok(vec![workspace(&self.ws_label)])
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<PaneInfo>, toron_plane::backend::BackendError> {
            Ok(self
                .agents
                .lock()
                .expect("mock agents lock")
                .iter()
                .map(|a| pane(&a.workspace_id, 1))
                .collect())
        }
        fn pane_split(
            &self,
            _parent_pane_id: &str,
            _direction: &str,
            _cwd: Option<&str>,
        ) -> Result<PaneInfo, toron_plane::backend::BackendError> {
            self.splits.fetch_add(1, Ordering::SeqCst);
            Ok(pane("wA", 2))
        }
        fn pane_close(&self, _pane_id: &str) -> Result<(), toron_plane::backend::BackendError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn agent_list(&self) -> Result<Vec<AgentInfo>, toron_plane::backend::BackendError> {
            Ok(self.agents.lock().expect("mock agents lock").clone())
        }
        fn agent_start(
            &self,
            name: &str,
            kind: &str,
            pane_id: &str,
            args: &[String],
        ) -> Result<AgentInfo, toron_plane::backend::BackendError> {
            self.started.lock().expect("mock started lock").push((
                name.into(),
                kind.into(),
                pane_id.into(),
                args.to_vec(),
            ));
            let mut agents = self.agents.lock().expect("mock agents lock");
            agents.push(agent(name, pane_id, "wA", AgentStatus::Idle));
            Ok(agents.last().expect("just pushed").clone())
        }
        fn agent_rename(
            &self,
            _target: &str,
            _new_name: Option<&str>,
        ) -> Result<(), toron_plane::backend::BackendError> {
            Ok(())
        }
    }

    /// bd-ude8: non-omp kinds keep the split+close cycle.
    #[test]
    fn non_omp_respawn_splits_and_closes_old_pane() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let mux = MockMux::new(
            "flywheel-demo",
            vec![agent("flywheel-demo-oc", "wA:p1", "wA", AgentStatus::Idle)],
        );

        respawn_with_backend(&mux, &cfg, "demo", "flywheel-demo-oc", "opencode", true)
            .expect("respawn ok");

        assert_eq!(
            mux.splits.load(Ordering::SeqCst),
            1,
            "non-omp respawn splits a fresh pane"
        );
        assert_eq!(
            mux.closes.load(Ordering::SeqCst),
            1,
            "non-omp respawn closes the old pane"
        );
        let started = mux.started.lock().expect("mock started lock");
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0].2, "wA:p2",
            "agent starts on the fresh split pane"
        );
        assert!(
            started[0].3.is_empty(),
            "unprofiled agent has no registry profile → unchanged empty argv"
        );
    }

    /// bd-ude8: respawn argv comes ONLY from the registry-pinned profile —
    /// legacy entries (no profile) and unknown names keep the empty argv.
    #[test]
    fn respawn_args_only_from_registry_pinned_profile() {
        let _g = lock_env();
        let (cfg, dir) = hermetic();
        write_profiles_toml(&dir);
        let key = project_dir_str(&cfg, "demo").expect("key");
        state::save_spawn_registry(
            &key,
            &[
                state::SpawnEntry::profiled("wA:p1", "profile-coder", "pi-coder", "pi", None),
                state::SpawnEntry::legacy("wA:p2", "flywheel-demo-oc"),
            ],
        )
        .expect("registry");

        let pi = respawn_launch_args(&cfg, "demo", "profile-coder", "pi");
        assert!(!pi.is_empty(), "profiled agent re-injects its launch args");
        assert!(
            pi.iter().any(|a| a.contains("glm-5.3")),
            "model flag: {pi:?}"
        );

        let legacy = respawn_launch_args(&cfg, "demo", "flywheel-demo-oc", "opencode");
        assert!(legacy.is_empty(), "legacy entry (no profile) → empty argv");

        let unknown = respawn_launch_args(&cfg, "demo", "no-such-agent", "pi");
        assert!(unknown.is_empty(), "unknown agent → empty argv");
    }
}

#[cfg(test)]
mod workspace_origin_tests {
    use super::*;
    use crate::hc::{AgentInfo, AgentStatus, WorkspaceInfo};
    use crate::{
        PrimeChoice, WorkspaceOrigin, add_target_from_split, classify_workspace_origin,
        partition_workspaces, prime_choice,
    };

    fn ws(id: &str, label: &str, panes: u64) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 0,
            label: label.to_string(),
            focused: false,
            pane_count: panes,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: AgentStatus::Idle,
            cwd: None,
        }
    }

    fn agent(name: &str, workspace_id: &str) -> AgentInfo {
        AgentInfo {
            agent: Some("opencode".into()),
            agent_status: AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: true,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: format!("{workspace_id}:p1"),
            revision: 1,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term_1".into(),
            terminal_title: None,
            workspace_id: workspace_id.to_string(),
        }
    }

    /// bd-627z.3 gate: every ownership signal in one table — base label,
    /// swarm label, flywheel-prefixed agents, recorded spawn meta, plain
    /// agents, unrelated workspaces, and the precedence pairs.
    /// One row of the workspace-origin table, named rather than a five-tuple:
    /// the fields read at the assertion sites, and clippy's `type_complexity`
    /// stays quiet (same shape as `pane`'s `RouteRow`).
    struct OriginRow {
        case: &'static str,
        w: WorkspaceInfo,
        agents: Vec<AgentInfo>,
        spawn_label: Option<&'static str>,
        want: Option<WorkspaceOrigin>,
    }

    #[test]
    fn classify_workspace_origin_rows() {
        let cases: [OriginRow; 9] = [
            OriginRow {
                case: "base_label_alone_is_foreign",
                w: ws("w17", "toron", 2),
                agents: vec![],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Foreign),
            },
            OriginRow {
                case: "swarm_label_is_owned",
                w: ws("w2A", "flywheel-toron", 3),
                agents: vec![],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Owned),
            },
            OriginRow {
                case: "request_key_label_is_owned",
                w: ws("w2C", "flywheel-toron-rk-toron-host-cut-1", 1),
                agents: vec![],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Owned),
            },
            OriginRow {
                case: "flywheel_prefixed_agents_mark_owned",
                w: ws("w9", "custom-swarm-label", 1),
                agents: vec![agent("flywheel-toron-oc", "w9")],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Owned),
            },
            OriginRow {
                case: "recorded_spawn_meta_label_marks_owned",
                w: ws("w9", "my-special-label", 1),
                agents: vec![],
                spawn_label: Some("my-special-label"),
                want: Some(WorkspaceOrigin::Owned),
            },
            OriginRow {
                case: "unrelated_workspace_is_skipped",
                w: ws("w1", "scratchpad", 1),
                agents: vec![],
                spawn_label: None,
                want: None,
            },
            OriginRow {
                case: "base_label_with_plain_agent_stays_foreign",
                w: ws("w17", "toron", 1),
                agents: vec![agent("oc", "w17")],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Foreign),
            },
            OriginRow {
                case: "spawn_meta_beats_base_label",
                w: ws("w2B", "toron", 1),
                agents: vec![],
                spawn_label: Some("toron"),
                want: Some(WorkspaceOrigin::Owned),
            },
            OriginRow {
                case: "flywheel_agents_beat_base_label",
                w: ws("w17", "toron", 1),
                agents: vec![agent("flywheel-toron-oc", "w17")],
                spawn_label: None,
                want: Some(WorkspaceOrigin::Owned),
            },
        ];
        for row in cases {
            assert_eq!(
                classify_workspace_origin("toron", &row.w, &row.agents, row.spawn_label),
                row.want,
                "{}",
                row.case
            );
        }
    }

    #[test]
    fn partition_splits_owned_foreign_and_drops_unrelated() {
        let ws_list = vec![
            ws("wA", "flywheel-toron", 2),
            ws("wB", "toron", 1),
            ws("wC", "scratch", 1),
        ];
        let split = partition_workspaces("toron", ws_list, &[], None);
        assert_eq!(split.owned.len(), 1);
        assert_eq!(split.owned[0].workspace_id, "wA");
        assert_eq!(split.foreign.len(), 1);
        assert_eq!(split.foreign[0].workspace_id, "wB");
    }

    #[test]
    fn add_targets_owned_workspace() {
        let ws_list = vec![
            ws("w37", "toron", 6),
            ws("w83", "flywheel-toron", 3),
            ws("wX", "scratchpad", 1),
        ];
        let split = partition_workspaces("toron", ws_list, &[], None);
        assert_eq!(split.foreign.len(), 1);
        assert_eq!(split.foreign[0].workspace_id, "w37");
        assert_eq!(split.owned.len(), 1);
        assert_eq!(split.owned[0].workspace_id, "w83");
        let target = add_target_from_split(&split, &[], "toron").unwrap();
        assert_eq!(target, "w83", "add must target the owned swarm workspace");
    }

    #[test]
    fn add_refuses_when_only_foreign_workspace_exists() {
        let ws_list = vec![ws("w37", "toron", 6)];
        let split = partition_workspaces("toron", ws_list, &[], None);
        let err = add_target_from_split(&split, &[], "toron").unwrap_err();
        assert!(
            err.to_string().contains("no flywheel-owned swarm"),
            "expected refuse, got: {err}"
        );
    }

    #[test]
    fn spawn_prime_is_launch_bootstrap_under_the_flag_and_ready_check_without() {
        use crate::pane_write::Harness;
        assert_eq!(prime_choice(false, "pi"), PrimeChoice::LegacyReady);
        assert_eq!(prime_choice(false, "claude"), PrimeChoice::LegacyReady);
        assert_eq!(
            prime_choice(true, "pi"),
            PrimeChoice::Bootstrap(Harness::Pi)
        );
        assert_eq!(
            prime_choice(true, "claude"),
            PrimeChoice::Bootstrap(Harness::Claude)
        );
        assert_eq!(
            prime_choice(true, "who-knows"),
            PrimeChoice::Bootstrap(Harness::Other)
        );
        let text = pane_write::launch_bootstrap(Harness::Pi);
        assert!(text.contains("toron next") && !text.contains("READY"));
        let src = crate::test_source::crate_source();
        let call_lines = |p: &str| {
            src.lines()
                .filter(|l| l.trim_start().starts_with(p))
                .count()
        };
        assert_eq!(
            call_lines("prime_bootstrap("),
            4,
            "spawn (peer arm + plain arm) + add + re-launch"
        );
        assert_eq!(
            call_lines("prime_via_send_text("),
            0,
            "no spawn path calls the legacy prime directly"
        );
        let a = src.find("\nfn prime_bootstrap_with(").unwrap();
        let body = &src[a..a + src[a..].find("\n}\n").unwrap()];
        assert!(
            body.contains("pane_write::launch_bootstrap_into(c, pane_id, h);")
                && body.contains("note_launch_hook(kind);"),
            "the flag arm goes through the PaneWrite sink: {body}"
        );
        let arm = concat!("=> prime_via", "_send_text(c, pane_id)");
        assert_eq!(src.matches(arm).count(), 1);
    }

    #[test]
    fn add_prime_wired_into_add_path() {
        let src = crate::test_source::crate_source();
        let add = src.find("fn cmd_add(").expect("cmd_add present in main.rs");
        let add_body = &src[add..];
        let prime_fn = src
            .find("fn prime_via_send_text(")
            .expect("prime_via_send_text present in main.rs");
        let prime_body = &src[prime_fn..];
        assert!(
            add_body.contains("if settle_secs > 0 {\n                std::thread::sleep")
                && add_body
                    .find("if settle_secs > 0")
                    .expect("settle sleep in cmd_add")
                    < add_body
                        .find("prime_bootstrap(&mut c, &prev, &kind)")
                        .expect("prime call in cmd_add"),
            "cmd_add must sleep the settle BEFORE priming (bd-umb3)"
        );
        assert!(
            add_body.contains("prime_bootstrap(&mut c, &prev, &kind);"),
            "cmd_add must invoke the retried boot prime for each new pane (bd-umb3)"
        );
        assert!(
            prime_body.contains("manual socket prime: hc terminal send-text"),
            "prime_via_send_text must warn with the manual socket one-liner (bd-umb3)"
        );
    }

    #[test]
    fn add_mints_mail_identity_and_writes_agent_names() {
        let src = crate::test_source::crate_source();
        let add = src.find("fn cmd_add(").expect("cmd_add present in main.rs");
        let add_body = &src[add..];
        assert!(
            add_body.contains("register_spawned_agent(cfg, project, &name, &kind, &agent.pane_id)"),
            "cmd_add must register a unique mail identity per added pane (bd-add-missing-mail-identity-16le)"
        );
        assert!(
            add_body.contains("state::save_agent_names(&dir, &registry)"),
            "cmd_add must write the project-local agent-names.json mirror (bd-add-missing-mail-identity-16le)"
        );
    }

    #[test]
    fn add_never_passes_launch_text_as_agent_start_args() {
        let src = crate::test_source::crate_source();
        let add = crate::tripwire::find_fn(&src, "cmd_add");
        let add_end = crate::tripwire::find_fn(&src, "preflight_gate");
        let add_body = &src[add..add_end];
        assert!(
            !add_body.contains("add_args.push(doorbell::identity_prime"),
            "cmd_add must not push the identity line into agent.start args — opencode \
             reads a positional arg as a directory and dies chdir ENOENT"
        );
        let typed = add_body
            .find("prime_line_via_send_text(")
            .expect("cmd_add must type the identity line via pane.send_text");
        assert!(
            add_body[typed..].contains("doorbell::identity_prime("),
            "the typed prime must carry the pane's identity line"
        );
        let settle = add_body
            .find("if settle_secs > 0")
            .expect("settle sleep in cmd_add");
        assert!(
            settle < typed,
            "cmd_add must settle BEFORE typing the identity prime (bd-eign.6 L2)"
        );
    }
}

#[cfg(test)]
mod adr0013_spawn_effect_tests {
    use super::{build_dry_run_plan, refuse_blocked, spawn_items_from_plan};
    use crate::profile;

    /// Slice one top-level function body. Both needles carry a real newline so
    /// they can only match real code — never a test literal, which spells the
    /// newline as an escape and so contains no newline character.
    fn body_of<'a>(src: &'a str, start_needle: &str, end_needle: &str) -> &'a str {
        let start = src
            .find(start_needle)
            .unwrap_or_else(|| panic!("expected {start_needle:?}"));
        let end = src[start..]
            .find(end_needle)
            .map(|i| start + i)
            .unwrap_or(src.len());
        assert!(start < end, "{start_needle:?} must precede {end_needle:?}");
        &src[start..end]
    }

    /// ADR-0022 phase 1: `--dry-run` returns before any effect. In both
    /// spawn entrypoints the dry-run branch precedes the receipt digest,
    /// the intent journal, and every pane/registry/mail dispatch, so a dry
    /// run cannot create a pane or write any state.
    #[test]
    fn dry_run_returns_before_any_effect() {
        let src = crate::test_source::crate_source();
        for entry in ["pub(crate) fn cmd_spawn(", "pub(crate) fn cmd_spawn_rex("] {
            let start = src
                .find(entry)
                .unwrap_or_else(|| panic!("expected {entry:?}"));
            let body = &src[start..];
            let dry = body
                .find("if opts.dry_run {")
                .unwrap_or_else(|| panic!("{entry} must have a dry-run branch"));
            for effect in [
                "request_receipts::plan_digest(",
                "AgentEffectGuard::begin",
                "workspace_create(",
                "pane_split(",
            ] {
                if let Some(pos) = body.find(effect) {
                    assert!(dry < pos, "{entry}: dry-run must precede {effect:?}");
                }
            }
            assert!(
                body[dry..].contains("return Ok(())"),
                "{entry}: dry-run branch must return"
            );
        }
    }

    /// ADR-0022 phase 2b (bd-7rg0f): a blocked field refuses the launch.
    /// Pure decision: any `blocked` field on any seat refuses, naming each
    /// blocked seat field and its remedy; an all-honored plan proceeds.
    #[test]
    fn refuse_blocked_names_every_blocked_seat_field() {
        let blocked = crate::swarm_plan::Field {
            value: "gemini-2.5-pro".into(),
            from: crate::swarm_plan::Provenance::User,
            status: crate::swarm_plan::FieldStatus::Blocked {
                remedy: "pick another harness".into(),
            },
            registry: None,
            was: None,
        };
        let honored = crate::swarm_plan::Field {
            value: "x".into(),
            from: crate::swarm_plan::Provenance::Slot,
            status: crate::swarm_plan::FieldStatus::Honored,
            registry: None,
            was: None,
        };
        let mut plan = crate::swarm_plan::Plan::new("swarm");
        plan.seats.push(crate::swarm_plan::Seat {
            role: "coder".into(),
            count: 1,
            harness: "gemini".into(),
            model: Some(blocked),
            effort: None,
            mode: None,
            phase: None,
            placement: "pane".to_string(),
        });
        plan.seats.push(crate::swarm_plan::Seat {
            role: "reviewer".into(),
            count: 1,
            harness: "pi".into(),
            model: Some(honored),
            effort: None,
            mode: None,
            phase: None,
            placement: "pane".to_string(),
        });
        let err = refuse_blocked(&plan).expect_err("one blocked field refuses");
        assert!(
            err.iter()
                .any(|e| e.contains("coder") && e.contains("model")),
            "{err:?}"
        );
        assert!(
            err.iter().any(|e| e.contains("pick another harness")),
            "{err:?}"
        );
        let mut clean = crate::swarm_plan::Plan::new("swarm");
        clean.seats.push(crate::swarm_plan::Seat {
            role: "coder".into(),
            count: 1,
            harness: "pi".into(),
            model: Some(crate::swarm_plan::Field {
                value: "m".into(),
                from: crate::swarm_plan::Provenance::User,
                status: crate::swarm_plan::FieldStatus::Honored,
                registry: None,
                was: None,
            }),
            effort: None,
            mode: None,
            phase: None,
            placement: "pane".to_string(),
        });
        assert!(refuse_blocked(&clean).is_ok());
    }

    /// ADR-0022 D4 phase 3a (bd-uivb4): the dry-run plan carries the
    /// resolution. Ambiguous bare ask from the user blocks listing the
    /// candidates; replaced ids resolve with from=replacement and was set.
    #[test]
    fn dry_run_resolution_marks_ambiguous_and_replaced() {
        use std::collections::BTreeMap;
        let mut models = BTreeMap::new();
        for key in ["a/m", "b/m", "old/r", "new/r", "zai/ok"] {
            models.insert(
                key.to_string(),
                crate::models::ModelEntry {
                    caps: vec![],
                    fallback: None,
                    ..Default::default()
                },
            );
        }
        models.insert(
            "old/r".to_string(),
            crate::models::ModelEntry {
                caps: vec![],
                fallback: None,
                replaced_by: Some("new/r".to_string()),
                retired: false,
                ..Default::default()
            },
        );
        let reg = crate::models::ModelRegistry {
            models,
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        };
        let cfg = crate::config::Config::default();
        let catalog = profile::ProfileCatalog::default();
        let items = spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let plan = build_dry_run_plan(&cfg, &items, &catalog, &reg, "m", "").expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        match &f.status {
            crate::swarm_plan::FieldStatus::Blocked { remedy } => {
                assert!(
                    remedy.contains("a/m") && remedy.contains("b/m"),
                    "remedy lists candidates: {remedy}"
                );
            }
            other => panic!("user ambiguity must block, got {other:?}"),
        }
        let plan =
            build_dry_run_plan(&cfg, &items, &catalog, &reg, "old/r", "").expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Replacement);
        assert_eq!(f.was.as_deref(), Some("old/r"));
        assert_eq!(f.status, crate::swarm_plan::FieldStatus::Honored);
    }

    /// ADR-0022 D4 phase 3a (bd-uivb4): the same ambiguous ask from a
    /// profile degrades instead of blocking.
    #[test]
    fn dry_run_ambiguous_from_profile_is_degraded() {
        use std::collections::BTreeMap;
        let mut models = BTreeMap::new();
        for key in ["a/m", "b/m"] {
            models.insert(
                key.to_string(),
                crate::models::ModelEntry {
                    caps: vec![],
                    fallback: None,
                    ..Default::default()
                },
            );
        }
        let reg = crate::models::ModelRegistry {
            models,
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        };
        let cfg = crate::config::Config::default();
        let mut catalog = profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".into(),
                model: "m".into(),
                ..Default::default()
            },
        );
        let items = spawn_items_from_plan(
            Vec::new(),
            Some(("coder", catalog.profiles.get("coder").unwrap(), 1)),
        );
        let plan = build_dry_run_plan(&cfg, &items, &catalog, &reg, "", "").expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert!(
            matches!(f.status, crate::swarm_plan::FieldStatus::Degraded { .. }),
            "profile ambiguity degrades: {:?}",
            f.status
        );
    }

    /// I4: the spawn intent is committed before the workspace dispatch.
    #[test]
    fn spawn_journals_intent_before_dispatch() {
        let src = crate::test_source::crate_source();
        let spawn = body_of(&src, "pub(crate) fn cmd_spawn(", "\nfn poll_child_exit(");
        let intent = spawn
            .find("AgentEffectGuard::begin(project, &label, agent_ref::AgentPhase::Spawn)")
            .expect("cmd_spawn must journal the spawn intent");
        let dispatch = spawn
            .find("c.workspace_create(")
            .expect("cmd_spawn must create the workspace");
        assert!(
            intent < dispatch,
            "I4: the spawn intent must precede the workspace dispatch"
        );
    }

    /// bd-command-receipts-zile: the receipt check is a precondition, not a
    /// postscript. `spawn_gate` reads the journal for the key's slot BEFORE
    /// the effect guard journals anything and before the workspace dispatch,
    /// so a replay returns without a second crew and an unresolved slot
    /// refuses instead of racing one.
    #[test]
    fn the_spawn_receipt_gate_precedes_dispatch() {
        let src = crate::test_source::crate_source();
        let spawn = body_of(&src, "pub(crate) fn cmd_spawn(", "\nfn poll_child_exit(");
        let gate = spawn
            .find("request_receipts::spawn_gate(")
            .expect("cmd_spawn must consult the request-key receipt");
        let guard = spawn
            .find("AgentEffectGuard::begin")
            .expect("cmd_spawn must journal the spawn intent");
        let dispatch = spawn
            .find("c.workspace_create(")
            .expect("cmd_spawn must create the workspace");
        assert!(
            gate < guard,
            "the receipt is read before the intent is journaled"
        );
        assert!(guard < dispatch, "and the intent before the dispatch");
        let replay_arm = spawn
            .split_once("request_receipts::Gate::Replay =>")
            .map(|(_, rest)| rest)
            .expect("the replay arm exists");
        let replay_arm = replay_arm
            .split("request_receipts::Gate::InFlight")
            .next()
            .expect("the in-flight arm follows the replay arm");
        assert!(
            replay_arm.contains("return Ok(())"),
            "a settled slot must return before dispatch: {replay_arm}"
        );
        assert!(
            spawn.contains("with_plan_digest"),
            "the keyed attempt records the plan it was dispatched for"
        );
    }

    /// bd-command-receipts-zile: `add` creates panes, so its receipt gate and
    /// intent row stand before the first split for the same reason spawn's do
    /// — a retried call must add nothing, not add again.
    #[test]
    fn the_add_receipt_gate_precedes_dispatch() {
        let src = include_str!("spawn.rs");
        let add = body_of(
            src,
            "pub(crate) fn cmd_add(",
            "\n// ---- list / status ----",
        );
        let gate = add
            .find("request_receipts::spawn_gate(")
            .expect("cmd_add must consult the request-key receipt");
        let guard = add
            .find("AgentEffectGuard::begin")
            .expect("cmd_add must journal the add intent");
        let dispatch = add
            .find("hc::pane_split(")
            .expect("cmd_add must split panes");
        assert!(
            gate < guard,
            "the receipt is read before the intent is journaled"
        );
        assert!(
            guard < dispatch,
            "and the intent before the first pane split"
        );
        let replay_arm = add
            .split_once("request_receipts::Gate::Replay =>")
            .map(|(_, rest)| rest)
            .expect("the replay arm exists");
        let replay_arm = replay_arm
            .split("request_receipts::Gate::InFlight")
            .next()
            .expect("the in-flight arm follows the replay arm");
        assert!(
            replay_arm.contains("return Ok(())"),
            "a settled slot must return before dispatch: {replay_arm}"
        );
        assert!(
            add.contains("let add_base = format!(\"{}-add\", swarm_label(project));"),
            "the add slot must not be the swarm's own spawn subject"
        );
        assert!(
            add.contains("add_effect.settle("),
            "the add row settles with its own evidence"
        );
    }

    /// ADR-0019 D10: the approved payload runs the same goal delegate as a
    /// local steer, with empty crew/reuse meaning absent. Pure command
    /// construction — execution stays downstream.
    #[test]
    fn approved_payload_builds_the_goal_delegate() {
        let payload = crate::request_receipts::SteerPayload {
            objective: "o".to_string(),
            crew: "-".to_string(),
            direct: true,
            reuse: "-".to_string(),
            retain: false,
            retire_orphans: false,
            project: "flywheel".to_string(),
            bead_id: None,
        };
        let (name, inputs, crew, execute, mode, project) = match crate::approved_run_cmd(&payload) {
            crate::cmd::workflow::WorkflowCmd::Run {
                name,
                inputs,
                crew,
                execute,
                mode,
                project,
                ..
            } => (name, inputs, crew, execute, mode, project),
            _ => panic!("approved steer must run the goal delegate"),
        };
        assert_eq!(name, "goal-direct");
        assert!(execute);
        assert_eq!(mode, "pane");
        assert_eq!(project.as_deref(), Some("flywheel"));
        assert_eq!(crew, None, "absent crew stays absent, never \"-\"");
        assert!(inputs.unwrap_or_default().contains("\"objective\":\"o\""));
    }

    /// bd-4fgc: the steer door is effectful (a MaterialContext fact plus a
    /// workflow run), so its receipt gate and intent row stand before the first
    /// effect. A retried steer replays; it never queues a second fact or
    /// dispatches a second run.
    #[test]
    fn the_steer_receipt_gate_precedes_dispatch() {
        let src = include_str!("spawn.rs");
        let steer = body_of(
            src,
            "pub(crate) fn cmd_steer(",
            "\n/// ADR-0022 W4 testable seam",
        );
        let gate = steer
            .find("request_receipts::receipt_gate(")
            .expect("cmd_steer must consult the request-key receipt");
        let guard = steer
            .find("DoorReceiptGuard::begin(")
            .expect("cmd_steer must journal the steer intent");
        let fact = steer
            .find("append_pending_fact(")
            .expect("cmd_steer must queue the MaterialContext fact");
        let dispatch = steer
            .find("cmd_workflow(cfg, &run)")
            .expect("cmd_steer must dispatch the run");
        assert!(
            gate < guard,
            "the receipt is read before the intent is journaled"
        );
        assert!(guard < fact, "and the intent before the first effect");
        assert!(fact < dispatch, "and the fact before the run dispatch");
        let replay_arm = steer
            .split_once("request_receipts::Gate::Replay =>")
            .map(|(_, rest)| rest)
            .expect("the replay arm exists");
        let replay_arm = replay_arm
            .split("request_receipts::Gate::InFlight")
            .next()
            .expect("the in-flight arm follows the replay arm");
        assert!(
            replay_arm.contains("return Ok(())"),
            "a settled slot must return before dispatch: {replay_arm}"
        );
        assert!(
            steer.contains("g.settle(") && steer.contains("mark_unknown("),
            "the steer row settles on the observed outcome, never by assumption"
        );
    }

    /// bd-4fgc: the halt door is effectful too (it appends a `Halt` the host
    /// folds), so the same ordering holds — and one `queue_halt` serves both
    /// `flywheel halt` and `supervisor halt`.
    #[test]
    fn the_halt_receipt_gate_precedes_dispatch() {
        let src = include_str!("../supervisor_host.rs");
        let halt = body_of(src, "\npub fn queue_halt(", "\nfn print_view(");
        let gate = halt
            .find("crate::request_receipts::receipt_gate(")
            .expect("queue_halt must consult the request-key receipt");
        let guard = halt
            .find("DoorReceiptGuard::begin(")
            .expect("queue_halt must journal the halt intent");
        let effect = halt
            .find("append_input(")
            .expect("queue_halt must append the Halt");
        assert!(
            gate < guard,
            "the receipt is read before the intent is journaled"
        );
        assert!(
            guard < effect,
            "and the intent before the Halt is queued: {halt}"
        );
        let replay_arm = halt
            .split_once("crate::request_receipts::Gate::Replay =>")
            .map(|(_, rest)| rest)
            .expect("the replay arm exists");
        let replay_arm = replay_arm
            .split("crate::request_receipts::Gate::InFlight")
            .next()
            .expect("the in-flight arm follows the replay arm");
        assert!(
            replay_arm.contains("return Ok(())"),
            "a settled slot must return before the append: {replay_arm}"
        );
        assert!(
            halt.contains("door_slot(project, \"halt\", key)"),
            "the halt slot names its own door"
        );
    }

    /// I5: Settled needs apply evidence; `--no-verify` journals Unknown.
    #[test]
    fn spawn_never_settles_without_apply_evidence() {
        let src = crate::test_source::crate_source();
        let spawn = body_of(&src, "pub(crate) fn cmd_spawn(", "\nfn poll_child_exit(");
        let unknown = spawn
            .find("spawn_effect.mark_unknown(Some(\"--no-verify")
            .expect("--no-verify must journal Unknown, not Settled");
        let settle = spawn
            .find("spawn_effect.settle(Some(&format!(")
            .expect("verified spawns must journal Settled with evidence");
        assert!(
            unknown < settle,
            "Unknown and Settled are exclusive branches"
        );
        assert!(
            spawn.contains("\"workspace={} panes={}\""),
            "Settled must cite the verified workspace + live panes"
        );
    }

    /// P1-7/P1-8 (Wave C): the spawn ledger records the Muse/slot pin the
    /// launch plan resolved AND a bounded digest of the attached skills. One
    /// `profile_launch_plan` call feeds argv and ledger alike — the spawn path
    /// never re-derives a second, possibly-divergent route.
    #[test]
    fn spawn_ledger_records_model_pin_and_skills_digest() {
        let src = crate::test_source::crate_source();
        let spawn = body_of(&src, "pub(crate) fn cmd_spawn(", "\nfn poll_child_exit(");
        assert!(
            spawn.contains("profile_launch_plan(&kind, p, None)"),
            "cmd_spawn must resolve argv + pin in ONE call (P1-7)"
        );
        assert!(
            !spawn.contains("profile_launch_args("),
            "cmd_spawn must not re-derive a second launch plan"
        );
        assert!(
            spawn.contains("entry.model_pin = model_pin;"),
            "the spawn row must carry the resolved Muse/slot pin (P1-7)"
        );
        assert!(
            spawn.contains("entry.skills_digest = skills_digest;"),
            "the spawn row must carry the bounded skills digest (P1-8)"
        );
        assert!(
            spawn.contains("profile::skills_digest(&p.skills)"),
            "the digest must be taken from the profile's attached skills (P1-8)"
        );
    }

    /// bd-rex-reliability-ioyz (A+B fence): the post-spawn repair pass
    /// compares and renames to the mail PIN (`mail_identity`), never the
    /// lane name. A revert to `e.name` reopens the measured defect (a live
    /// pin-labelled block renamed back to the lane, unroutable again), so
    /// this asserts the call site positively names `mail_identity` — a
    /// symbol-absence grep is not evidence.
    #[test]
    fn rex_repair_pass_binds_the_mail_pin_not_the_lane_name() {
        let src = include_str!("spawn.rs");
        let spawn = body_of(
            src,
            "pub(crate) fn cmd_spawn_rex(",
            "\nfn doorbell_project_workspaces(",
        );
        let repair = spawn
            .find("let routable_label = |e: &state::SpawnEntry|")
            .expect("cmd_spawn_rex must carry the A+B repair pass");
        let repair = &spawn[repair..];
        let rename = repair
            .find("backend.agent_rename(&e.pane_id, Some(routable.as_str()))")
            .expect("the repair rename must pass the routable PIN label");
        let routable = 0usize;
        assert!(
            routable < rename,
            "the closure must be defined before the rename call"
        );
        let closure = &repair[routable..rename];
        assert!(
            closure.contains("e.mail_identity"),
            "the routable label must come from mail_identity: {closure}"
        );
        assert!(
            repair.contains("a.name.as_deref() != Some(routable.as_str())"),
            "the comparison must be against the PIN label, never e.name"
        );
    }

    /// I6: the wake read is its own observation row, separate from spawn.
    #[test]
    fn wake_observation_is_separate_journal_evidence() {
        let src = crate::test_source::crate_source();
        let spawn = body_of(&src, "pub(crate) fn cmd_spawn(", "\nfn poll_child_exit(");
        let obs = &spawn[spawn
            .find("agent_ref::AgentEffect {")
            .expect("cmd_spawn must journal the wake observation")..];
        let row = obs
            .find("agent_ref::AgentEffect {")
            .map(|i| &obs[i..])
            .expect("the wake observation is its own effect row");
        let row = &row[..row.find("};\n").expect("the wake effect literal ends")];
        assert!(
            row.contains("agent_ref::AgentPhase::Wake"),
            "the wake read is its own effect row: {row}"
        );
        assert!(
            row.contains("subscribers={landed}"),
            "the wake row must carry the observed subscriber count: {row}"
        );
        assert!(
            row.contains("agent_ref::EffectState::Unknown"),
            "a wake read that found nothing is Unknown, not a silent absence: {row}"
        );
        assert!(
            !row.contains("spawn_effect"),
            "wake observation is separate evidence — it must never settle the spawn effect: {row}"
        );
    }

    /// D9: register is an effect with intent before the upsert, and both
    /// failure paths land Unknown (an unobservable upsert is not absence).
    #[test]
    fn register_journals_intent_and_unknown_on_failure() {
        let src = include_str!("../main.rs");
        let register = body_of(
            src,
            "\npub(crate) fn pinned_mail_identity(",
            "\npub(crate) fn bind_pane_mail_identity(",
        );
        let intent = register
            .find("agent_ref::AgentPhase::Register")
            .expect("pinned_mail_identity must journal the register intent");
        let upsert = register
            .find("mail.ensure_project(&key)")
            .expect("pinned_mail_identity must ensure the project");
        assert!(
            intent < upsert,
            "I4: register intent must precede the first upsert call"
        );
        assert_eq!(
            register.matches("mark_unknown(Some(&format!").count(),
            2,
            "ensure_project and register_agent failures both journal Unknown"
        );
        assert!(
            register.contains("g.settle(Some(&format!(\"pin={mail_name}\")"),
            "a drawn + registered pin settles with the pin as evidence"
        );
    }

    /// D9: the per-pin presence publish journals intent before the invoke and
    /// a terminal row from the observed outcome. Scoped to the owning
    /// function (`publish_presence_for_registry`): the presence code never
    /// lived inline in `cmd_spawn` — the old slice only caught it through an
    /// EOF-fallback accident.
    #[test]
    fn presence_journals_per_pin_effect() {
        let src = crate::test_source::crate_source();
        let start = crate::test_source::find_close(&src, "fn publish_presence_for_registry(");
        let spawn = body_of(&src[start..], "fn publish_presence_for_registry(", "\nfn ");
        let intent = spawn
            .find("agent_ref::AgentPhase::Presence")
            .expect("presence publish must journal its intent");
        let invoke = spawn
            .find("m.presence_publish(")
            .expect("presence publish invokes the mail surface");
        assert!(
            intent < invoke,
            "I4: presence intent must precede the publish invoke"
        );
        assert!(
            spawn.contains("g.mark_unknown(Some(&ev))"),
            "an unobservable presence publish journals Unknown, never Settled"
        );
    }
}

#[cfg(test)]
mod taken_name_recovery_tests {
    use super::*;
    use crate::backend::MultiplexerBackend;
    use crate::hc::AgentStatus;
    use crate::{
        StartAttempt, TakenNameAction, TakenNameOps, is_taken_name_error,
        start_agent_with_retry_dyn, start_agent_with_taken_name_clear, taken_name_action,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toron_plane::backend::BackendError;
    use toron_plane::backend::error::BackendErrorCode;

    /// The error an hc name collision really surfaces as: the wire code
    /// (no typed variant) plus the name in the message.
    fn taken(name: &str) -> BackendError {
        BackendError {
            code: BackendErrorCode::Other("agent_name_taken".into()),
            message: format!("agent name '{name}' is already taken"),
        }
    }

    fn row(name: &str, pane: &str) -> AgentInfo {
        AgentInfo {
            agent: Some(name.to_string()),
            agent_status: AgentStatus::Idle,
            cwd: None,
            display_agent: None,
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: Some(name.to_string()),
            pane_id: pane.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: "t1".into(),
            terminal_id: "term".into(),
            terminal_title: None,
            workspace_id: "wA".into(),
        }
    }

    /// The `(name, pane)` binding table a live hc would show, with call
    /// counters. `agent_start` refuses while ANY row holds the name (hc's
    /// own semantics); `agent_rename` rewrites the row whose pane matches —
    /// exactly what "renames whatever agent that pane holds" means.
    struct NameMux {
        bindings: Mutex<Vec<(String, String)>>,
        starts: AtomicUsize,
        renames: Mutex<Vec<(String, String)>>,
    }

    impl NameMux {
        fn new(rows: &[(&str, &str)]) -> Self {
            Self {
                bindings: Mutex::new(
                    rows.iter()
                        .map(|(n, p)| (n.to_string(), p.to_string()))
                        .collect(),
                ),
                starts: AtomicUsize::new(0),
                renames: Mutex::new(Vec::new()),
            }
        }
        fn starts(&self) -> usize {
            self.starts.load(Ordering::SeqCst)
        }
        fn renames(&self) -> Vec<(String, String)> {
            self.renames.lock().expect("renames lock").clone()
        }
    }

    impl MultiplexerBackend for NameMux {
        fn name(&self) -> &'static str {
            "name-taken-mux"
        }
        fn agent_list(&self) -> std::result::Result<Vec<AgentInfo>, BackendError> {
            Ok(self
                .bindings
                .lock()
                .expect("bindings lock")
                .iter()
                .map(|(n, p)| row(n, p))
                .collect())
        }
        fn agent_start(
            &self,
            name: &str,
            _kind: &str,
            pane_id: &str,
            _args: &[String],
        ) -> std::result::Result<AgentInfo, BackendError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let mut bindings = self.bindings.lock().expect("bindings lock");
            if bindings.iter().any(|(n, _)| n == name) {
                return Err(taken(name));
            }
            bindings.push((name.to_string(), pane_id.to_string()));
            Ok(row(name, pane_id))
        }
        fn agent_rename(
            &self,
            target: &str,
            new_name: Option<&str>,
        ) -> std::result::Result<(), BackendError> {
            let Some(new_name) = new_name else {
                return Ok(());
            };
            let mut bindings = self.bindings.lock().expect("bindings lock");
            for (n, p) in bindings.iter_mut() {
                if p == target {
                    *n = new_name.to_string();
                }
            }
            self.renames
                .lock()
                .expect("renames lock")
                .push((target.to_string(), new_name.to_string()));
            Ok(())
        }
    }

    /// bd-s91y, the bead's fence: recover's respawn call shape — the exact
    /// `start_agent_with_retry_dyn` invocation `cmd_recover`'s Respawn arm
    /// makes — must FREE the pane's own dead binding and really restart the
    /// agent. Pre-fix the loop reported success WITHOUT restarting, because
    /// the registry short-circuit matched the stale row the refused
    /// `agent.start` had just failed to replace: asserted as one refused start
    /// + one rename + one successful start.
    #[test]
    fn recover_frees_a_taken_name() {
        let mux = NameMux::new(&[("flywheel-demo-oc", "wA:p1")]);
        let agent = start_agent_with_retry_dyn(&mux, "flywheel-demo-oc", "opencode", "wA:p1", &[])
            .expect("recover's respawn must free its own stale binding and restart");
        assert_eq!(agent.pane_id, "wA:p1");
        assert_eq!(
            mux.starts(),
            2,
            "one refused start + one retry after the clear — never a 30-attempt burn"
        );
        assert_eq!(
            mux.renames(),
            vec![("wA:p1".to_string(), "flywheel-demo-oc-stale-1".to_string())],
            "the dead binding is freed with the bounded stale label"
        );
    }

    /// bd-s91y: a name held by ANOTHER pane's binding refuses in one read —
    /// never rename a stranger, never burn ~90s — and names the real holder.
    #[test]
    fn recover_refuses_a_foreign_holder_without_burning_the_budget() {
        let mux = NameMux::new(&[("flywheel-demo-oc", "wA:p9")]);
        let started = std::time::Instant::now();
        let err = start_agent_with_retry_dyn(&mux, "flywheel-demo-oc", "opencode", "wA:p1", &[])
            .expect_err("a foreign holder is a refusal, not a retry");
        let msg = err.to_string();
        assert!(
            msg.contains("wA:p9"),
            "the refusal must name the real holder: {msg}"
        );
        assert!(
            msg.contains("will not rename a stranger"),
            "the refusal must say WHY it did not clear the name: {msg}"
        );
        assert_eq!(
            mux.starts(),
            1,
            "one start attempt — the 3s-per-attempt burn is gone"
        );
        assert!(
            mux.renames().is_empty(),
            "a foreign holder's binding must never be renamed"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the refusal is immediate (no retry sleep ran): {:?}",
            started.elapsed()
        );
    }

    /// The pure split: only the pane's OWN binding may be freed; every other
    /// shape refuses (an unreadable/absent binding included).
    #[test]
    fn taken_name_action_frees_only_the_panes_own_binding() {
        assert_eq!(
            taken_name_action(Some("flywheel-demo-oc"), "flywheel-demo-oc", 3),
            TakenNameAction::FreeOwnBinding {
                label: "flywheel-demo-oc-stale-3".into()
            }
        );
        for binding in [None, Some("other-agent"), Some("flywheel-demo-o")] {
            assert_eq!(
                taken_name_action(binding, "flywheel-demo-oc", 1),
                TakenNameAction::ForeignHolder,
                "binding {binding:?} is not this pane's own"
            );
        }
    }

    /// A scripted `TakenNameOps`: the sequence of `agent.start` outcomes, the
    /// pane's binding, the visible holder, and an ordered call log. Lets a
    /// fence drive [`start_agent_with_taken_name_clear`] branch by branch —
    /// including the restore path, which needs a retry that fails for a reason
    /// OTHER than the name.
    struct ScriptedOps {
        /// Popped per `agent_start`: `None` = accepted, `Some(code)` = refusal.
        starts: Mutex<Vec<Option<&'static str>>>,
        binding: Option<String>,
        holder: Option<String>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedOps {
        fn new(
            starts: Vec<Option<&'static str>>,
            binding: Option<&str>,
            holder: Option<&str>,
        ) -> Self {
            Self {
                starts: Mutex::new(starts.into_iter().rev().collect()),
                binding: binding.map(str::to_string),
                holder: holder.map(str::to_string),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl TakenNameOps for ScriptedOps {
        fn start_agent(
            &mut self,
            _name: &str,
            _kind: &str,
            _pane_id: &str,
            _args: &[String],
        ) -> std::result::Result<AgentInfo, BackendError> {
            self.calls.lock().expect("calls lock").push("start".into());
            let next = self
                .starts
                .lock()
                .expect("starts lock")
                .pop()
                .expect("start_agent called more times than scripted");
            match next {
                None => Ok(row("flywheel-demo-oc", "wA:p1")),
                Some(code) => Err(BackendError {
                    code: BackendErrorCode::Other(code.into()),
                    message: format!("scripted {code}"),
                }),
            }
        }
        fn pane_binding(&mut self, _pane_id: &str) -> Option<String> {
            self.calls
                .lock()
                .expect("calls lock")
                .push("pane_binding".into());
            self.binding.clone()
        }
        fn holder_pane(&mut self, _name: &str) -> Option<String> {
            self.calls
                .lock()
                .expect("calls lock")
                .push("holder_pane".into());
            self.holder.clone()
        }
        fn rename_pane(
            &mut self,
            _pane_id: &str,
            label: &str,
        ) -> std::result::Result<(), BackendError> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(format!("rename:{label}"));
            Ok(())
        }
    }

    /// The clear path is one read of the pane's OWN binding: no holder scan.
    /// (Fresh-eyes fix — the holder shaped nothing but the refusal message.)
    #[test]
    fn successful_clear_reads_only_the_panes_own_binding() {
        let mut ops = ScriptedOps::new(
            vec![Some("agent_name_taken"), None],
            Some("flywheel-demo-oc"),
            Some("wA:p9"),
        );
        let outcome = start_agent_with_taken_name_clear(
            &mut ops,
            "flywheel-demo-oc",
            "opencode",
            "wA:p1",
            &[],
            1,
        );
        assert!(
            matches!(outcome, StartAttempt::Started),
            "the retry lands: {outcome:?}"
        );
        assert_eq!(
            ops.calls(),
            vec![
                "start".to_string(),
                "pane_binding".to_string(),
                "rename:flywheel-demo-oc-stale-1".to_string(),
                "start".to_string(),
            ],
            "one binding read, one free, one retry — and no holder scan"
        );
    }

    /// A free that does not buy a start is UNDONE: the pane gets its own name
    /// back instead of being left renamed for the rest of the caller's loop
    /// (measured live, where the retry answered `agent_pane_busy`). The caller
    /// still sees the retry's real error.
    #[test]
    fn failed_retry_restores_the_renamed_binding() {
        let mut ops = ScriptedOps::new(
            vec![Some("agent_name_taken"), Some("agent_pane_busy")],
            Some("flywheel-demo-oc"),
            None,
        );
        let outcome = start_agent_with_taken_name_clear(
            &mut ops,
            "flywheel-demo-oc",
            "opencode",
            "wA:p1",
            &[],
            1,
        );
        match outcome {
            StartAttempt::NotTaken(e) => assert_eq!(
                e.code().as_str(),
                "agent_pane_busy",
                "the caller must see the retry's own error"
            ),
            other => panic!("expected the retry's error to reach the caller, got {other:?}"),
        }
        assert_eq!(
            ops.calls(),
            vec![
                "start".to_string(),
                "pane_binding".to_string(),
                "rename:flywheel-demo-oc-stale-1".to_string(),
                "start".to_string(),
                "rename:flywheel-demo-oc".to_string(),
            ],
            "the free is transactional: the binding comes back when the retry fails"
        );
    }

    /// The refusal branch reads the holder FOR THE MESSAGE and renames nothing.
    #[test]
    fn foreign_refusal_names_the_holder_and_renames_nothing() {
        let mut ops = ScriptedOps::new(vec![Some("agent_name_taken")], None, Some("wA:p9"));
        match start_agent_with_taken_name_clear(
            &mut ops,
            "flywheel-demo-oc",
            "opencode",
            "wA:p1",
            &[],
            1,
        ) {
            StartAttempt::Refused(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("wA:p9"),
                    "the refusal must name the holder: {msg}"
                );
                assert!(
                    msg.contains("will not rename a stranger"),
                    "the refusal must say why it did not clear the name: {msg}"
                );
            }
            other => panic!("a foreign holder must refuse, got {other:?}"),
        }
        assert_eq!(
            ops.calls(),
            vec![
                "start".to_string(),
                "pane_binding".to_string(),
                "holder_pane".to_string()
            ],
            "the holder is read for the message only — never a rename"
        );
    }

    /// ADR-0003: the taken-name check reads the CODE. A different code whose
    /// message merely mentions `agent_name_taken` must not classify — that
    /// string-matching is what the refactor removed.
    #[test]
    fn taken_name_error_classifies_on_the_code_never_the_message() {
        assert!(is_taken_name_error(&taken("flywheel-demo-oc")));
        let lookalike = BackendError {
            code: BackendErrorCode::Other("invalid_agent_name".into()),
            message: "agent_name_taken is a different code".into(),
        };
        assert!(
            !is_taken_name_error(&lookalike),
            "message text must never classify a backend error"
        );
    }

    /// Wiring (bd-s91y): the recovery exists ONCE and every start door uses it
    /// — spawn's dead-shell verify plus both retry loops, the dyn one being
    /// `cmd_recover`'s respawn path. Each door's body is bounded at the next
    /// door so a later door's call cannot satisfy an earlier door's check (the
    /// older tripwire slice ran to EOF).
    #[test]
    fn every_start_door_routes_through_the_shared_recovery() {
        let src = include_str!("../main.rs");
        let doors = [
            "verify_or_respawn_pane",
            "start_agent_with_retry_profile",
            "start_agent_with_retry_dyn_profile",
        ];
        let mut bounds: Vec<usize> = doors
            .iter()
            .map(|d| crate::tripwire::find_fn(src, d))
            .collect();
        bounds.push(src.len());
        for (door, w) in doors.iter().zip(bounds.windows(2)) {
            let body = &src[w[0]..w[1]];
            assert!(
                body.contains("start_agent_with_taken_name_clear"),
                "{door} must route agent.start through the shared taken-name recovery"
            );
            assert!(
                !body.contains("format!(\"{name}-stale-{attempts}\")"),
                "{door} must not mint a raw stale label (hc's 32-char cap)"
            );
        }
    }
}

#[cfg(test)]
mod agent_mail_name_tests {
    use super::*;
    use crate::{
        WantToken, resolve_agent_default, resolve_mail_target, resolve_mail_target_registry,
        terminal_mail_name, to_want_token, utf8_path, valid_pair,
    };
    use std::path::PathBuf;

    /// 193.1: a non-UTF-8 path converts to an actionable ERROR (never a
    /// panic, never silent lossy corruption) — the hc/mail wire protocols
    /// are UTF-8, so a non-UTF-8 path is a clear failure.
    #[test]
    fn utf8_path_rejects_non_utf8_with_error() {
        use std::ffi::OsString;
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt;
        let bad = PathBuf::from(OsString::from_vec(vec![0x66, 0xFF, 0x6F, 0x6F]));
        #[cfg(not(unix))]
        let bad = PathBuf::from("plain-utf8");
        let err = utf8_path(&bad, "test path").unwrap_err();
        assert!(
            err.to_string().contains("not valid UTF-8"),
            "actionable error: {err:#}"
        );
        let good = PathBuf::from("/tmp/flywheel-utf8-ok");
        assert_eq!(
            utf8_path(&good, "test path").unwrap(),
            "/tmp/flywheel-utf8-ok"
        );
    }

    /// The mapping is deterministic: same terminal name → same adjective+noun,
    /// every call (and across machines — no randomness, no time).
    #[test]
    fn mapping_is_deterministic() {
        let a = terminal_mail_name("flywheel-flywheel-oc");
        let b = terminal_mail_name("flywheel-flywheel-oc");
        assert_eq!(a, b);
        assert_eq!(a, terminal_mail_name("flywheel-flywheel-oc"));
    }

    /// The output is a whitelist pair: a VALID_ADJECTIVES member + a
    /// VALID_NOUNS member, total length >= 5. This is the REAL gate — it is
    /// what makes `terminal_mail_name` idempotent (a mapped pin passes back
    /// through `valid_pair`), and bd-odc proved off-shape pairs like
    /// FrostDolphin are rejected.
    #[test]
    fn mapping_produces_valid_agent_mail_name() {
        for name in [
            "flywheel-flywheel-oc",
            "flywheel-hachiko-pi2",
            "flywheel-toron-gk3",
            "oc",
            "pi",
            "gk",
        ] {
            let m = terminal_mail_name(name);
            let lower = m.to_ascii_lowercase();
            assert!(
                lower.len() >= 5,
                "{name} -> '{m}' too short for normalize_agent_name (>=5)"
            );
            let adjectives = [
                "red",
                "blue",
                "green",
                "golden",
                "sapphire",
                "turquoise",
                "emerald",
                "crimson",
                "frosty",
                "misty",
                "stormy",
                "swift",
                "quiet",
                "bold",
                "calm",
                "bright",
                "dark",
                "wild",
                "silent",
                "gentle",
                "rustic",
                "noble",
                "proud",
                "amber",
                "coral",
                "azure",
                "teal",
                "topaz",
                "violet",
                "pearl",
                "ruby",
                "jade",
                "ivory",
                "indigo",
                "lilac",
                "fuchsia",
                "maroon",
                "navy",
                "olive",
                "sage",
                "scarlet",
                "silver",
                "copper",
                "bronze",
                "beige",
                "cream",
                "peach",
                "plum",
                "sunny",
                "foggy",
                "hazy",
                "cloudy",
                "rainy",
                "snowy",
                "icy",
                "mossy",
                "sandy",
                "gray",
                "black",
                "white",
                "brown",
                "pink",
                "yellow",
                "orange",
                "chartreuse",
                "lavender",
                "magenta",
                "rose",
                "cobalt",
                "tan",
            ];
            let nouns = [
                "stone",
                "lake",
                "creek",
                "pond",
                "mountain",
                "hill",
                "snow",
                "castle",
                "river",
                "forest",
                "valley",
                "canyon",
                "meadow",
                "prairie",
                "desert",
                "island",
                "cliff",
                "cave",
                "glacier",
                "waterfall",
                "spring",
                "stream",
                "reef",
                "dune",
                "ridge",
                "peak",
                "gorge",
                "marsh",
                "brook",
                "glen",
                "grove",
                "fern",
                "hollow",
                "basin",
                "cove",
                "bay",
                "harbor",
                "coast",
                "shore",
                "bluff",
                "knoll",
                "summit",
                "plateau",
                "dog",
                "cat",
                "bear",
                "fox",
                "wolf",
                "deer",
                "elk",
                "moose",
                "otter",
                "beaver",
                "badger",
                "lynx",
                "puma",
                "squirrel",
                "rabbit",
                "hare",
                "mouse",
                "mink",
                "seal",
                "horse",
                "lion",
                "tiger",
                "panther",
                "leopard",
                "jaguar",
                "coyote",
                "bison",
                "ox",
                "hawk",
                "eagle",
                "owl",
                "falcon",
                "raven",
                "heron",
                "crane",
                "finch",
                "robin",
                "sparrow",
                "duck",
                "goose",
                "swan",
                "dove",
                "wren",
                "jay",
                "lark",
                "kite",
                "condor",
                "osprey",
                "pelican",
                "gull",
                "tern",
                "stork",
                "ibis",
                "cardinal",
                "oriole",
                "thrush",
                "trout",
                "salmon",
                "bass",
                "pike",
                "carp",
                "turtle",
                "frog",
                "pine",
                "oak",
                "maple",
                "birch",
                "cedar",
                "willow",
                "aspen",
                "elm",
                "orchid",
                "lotus",
                "ivy",
                "tower",
                "bridge",
                "forge",
                "mill",
                "barn",
                "gate",
                "anchor",
                "lantern",
                "beacon",
                "compass",
                "horizon",
                "spire",
                "chapel",
                "citadel",
                "fortress",
            ];
            let adj_ok = adjectives.iter().any(|a| {
                lower
                    .strip_prefix(a)
                    .is_some_and(|rest| nouns.contains(&rest) && !rest.is_empty())
            });
            assert!(
                adj_ok,
                "{name} -> '{m}' must be a VALID_ADJECTIVES+VALID_NOUNS pair"
            );
        }
    }

    /// The mapping spreads across the tables (not all names collapse to one
    /// adjective+noun) — collisions are harmless but rare is better.
    #[test]
    fn mapping_has_spread() {
        let names = [
            "flywheel-flywheel-oc",
            "flywheel-flywheel-oc2",
            "flywheel-hachiko-pi",
            "flywheel-hachiko-pi2",
            "flywheel-toron-gk",
            "flywheel-toron-gk2",
            "flywheel-chiebukuro-oc",
            "flywheel-chiebukuro-pi",
            "flywheel-chiebukuro-gk",
        ];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for n in names {
            seen.insert(terminal_mail_name(n));
        }
        assert!(
            seen.len() >= 6,
            "expected spread across adjective+noun pairs, got {} unique: {seen:?}",
            seen.len()
        );
    }

    /// Idempotency: a name already shaped as a whitelist pair maps to ITSELF
    /// — re-mapping a mapped name must be a no-op, not a second hash into a
    /// DIFFERENT pair (the reverse-mapping gap: send paths that receive a
    /// mail identity must never re-map it).
    #[test]
    fn mapping_is_idempotent_on_valid_pairs() {
        for name in [
            "TopazOx",
            "SapphireOwl",
            "CoralFinch",
            "MistyCardinal",
            "AzurePine",
        ] {
            assert_eq!(
                terminal_mail_name(name),
                name,
                "{name} must pass through unchanged"
            );
        }
    }

    /// The shared splitter returns the canonical PascalCase form and rejects
    /// anything that is not a whitelist pair (regardless of input case).
    #[test]
    fn valid_pair_splits_and_canonicalizes() {
        let adjs = ["topaz", "sapphire"];
        let nouns = ["ox", "owl"];
        assert_eq!(
            valid_pair("TopazOx", &adjs, &nouns),
            Some("TopazOx".to_string())
        );
        assert_eq!(
            valid_pair("sapphireowl", &adjs, &nouns),
            Some("SapphireOwl".to_string())
        );
        assert_eq!(
            valid_pair("topazowl", &adjs, &nouns),
            Some("TopazOwl".to_string())
        );
        assert_eq!(
            valid_pair("topaz", &adjs, &nouns),
            None,
            "adjective alone is invalid"
        );
        assert_eq!(
            valid_pair("ox", &adjs, &nouns),
            None,
            "noun alone is invalid"
        );
        assert_eq!(valid_pair("flywheel-flywheel-oc", &adjs, &nouns), None);
        assert_eq!(valid_pair("", &adjs, &nouns), None);
    }

    /// An hc agent name maps to its identity; anything already a valid
    /// identity (or a bare short like "oc") passes through untouched.
    #[test]
    fn resolve_mail_target_maps_plane_names_and_passes_through_others() {
        assert_eq!(
            resolve_mail_target("flywheel-flywheel-oc"),
            terminal_mail_name("flywheel-flywheel-oc")
        );
        assert_eq!(resolve_mail_target("TopazOx"), "TopazOx");
        assert_eq!(resolve_mail_target("oc"), "oc");
        assert_eq!(resolve_mail_target(""), "");
    }

    /// bd-mail-hyphen-loud-3r20 (GS-028 NEW-2): ONLY hc-shaped names hash.
    /// Channel-shaped input (`steer-<ch>`) and hyphenated typos pass through
    /// so the server rejects them honestly instead of misrouting to a random
    /// adjective+noun pin. Crew pane names still map.
    #[test]
    fn resolve_mail_target_hyphen_only_plane_shapes_hash() {
        assert_eq!(resolve_mail_target("steer-abc123"), "steer-abc123");
        assert_eq!(resolve_mail_target("my-channel"), "my-channel");
        assert_eq!(
            resolve_mail_target("not-a-hc-name-at-all"),
            "not-a-hc-name-at-all"
        );
        assert_eq!(
            resolve_mail_target("flywheel-toron-oc"),
            terminal_mail_name("flywheel-toron-oc")
        );
        assert_eq!(
            resolve_mail_target("crew-plan-coder"),
            terminal_mail_name("crew-plan-coder")
        );
        assert_eq!(resolve_mail_target("TopazOx"), "TopazOx");
    }

    /// bd-627z.4 GATE: the spawn-registry pin wins over the pure hash —
    /// two resolves of the SAME agent return the SAME identity, the pin
    /// survives a reload (daemon-restart equivalent), and an unpinned agent
    /// falls back to the deterministic hash mapping (still stable).
    #[test]
    fn registry_pin_is_deterministic_and_survives_reload() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-reg-pin-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let project = "/tmp/fw-regpin-proj";
        let entries = vec![
            state::SpawnEntry::with_mail_identity(
                "w3:p1",
                "flywheel-fw-regpin-oc2",
                "DustySparrow",
            ),
            state::SpawnEntry::with_mail_identity("w3:p2", "flywheel-fw-regpin-oc3", "RoseDune"),
        ];
        state::save_spawn_registry(project, &entries).expect("save");

        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow"
        );
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc3"),
            "RoseDune"
        );

        let entries2 = state::load_spawn_registry(project);
        assert_eq!(entries2.len(), 2);
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-oc2"),
            "DustySparrow",
            "binding persists across reloads"
        );

        let hashed = resolve_mail_target_registry(project, "flywheel-fw-regpin-gk");
        assert_eq!(
            resolve_mail_target_registry(project, "flywheel-fw-regpin-gk"),
            hashed,
            "fallback is stable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bd-aseq regression: a registered slug whose PIN diverges from the
    /// deterministic hash mapping resolves to the PIN — the send-shaped
    /// chokepoint addresses the inbox the agent actually reads, never the
    /// parallel hash-mapped one.
    #[test]
    fn registered_slug_resolves_to_pin_not_hash_mapping() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-aseq-{}", uuid::Uuid::new_v4()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let project = "/tmp/fw-aseq-proj";
        let entries = vec![state::SpawnEntry::with_mail_identity(
            "w:p",
            "flywheel-fw-aseq-oc2",
            "TanFern",
        )];
        state::save_spawn_registry(project, &entries).expect("save");

        let pin = resolve_mail_target_registry(project, "flywheel-fw-aseq-oc2");
        assert_eq!(pin, "TanFern");
        assert_ne!(
            resolve_mail_target("flywheel-fw-aseq-oc2"),
            pin,
            "precondition: hash mapping diverges from the pinned identity"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// review-fix F7: bare shorts resolve to the spawned terminal name; concrete
    /// names pass through unchanged.
    #[test]
    fn resolve_agent_default_expands_shorts_and_passes_concrete() {
        assert_eq!(
            resolve_agent_default("hachiko", "oc"),
            "flywheel-hachiko-oc"
        );
        assert_eq!(
            resolve_agent_default("hachiko", "pi"),
            "flywheel-hachiko-pi"
        );
        assert_eq!(
            resolve_agent_default("hachiko", "gk"),
            "flywheel-hachiko-gk"
        );
        assert_eq!(
            resolve_agent_default("hachiko", "flywheel-hachiko-oc2"),
            "flywheel-hachiko-oc2"
        );
        let mapped = terminal_mail_name("flywheel-hachiko-oc");
        assert_eq!(resolve_agent_default("hachiko", &mapped), mapped);
        assert_eq!(resolve_agent_default("hachiko", "w1:p1"), "w1:p1");
        assert_eq!(resolve_agent_default("hachiko", ""), "");
    }

    /// bd-bwf: full terminal names shown by `flywheel list` must NOT be
    /// double-wrapped. Bare shorts expand; concrete names stay exact
    /// (WantToken.broaden=false) so a full name never drags in oc2.
    #[test]
    fn to_want_token_passes_through_full_names() {
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "flywheel-toron-oc2"),
            WantToken {
                name: "flywheel-toron-oc2".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "oc2"),
            WantToken {
                name: "flywheel-toron-oc2".into(),
                broaden: false,
                short: Some("oc".into()),
                ordinal: 2,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "oc1"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: false,
                short: Some("oc".into()),
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "pi2"),
            WantToken {
                name: "flywheel-toron-pi2".into(),
                broaden: false,
                short: Some("pi".into()),
                ordinal: 2,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "gk1"),
            WantToken {
                name: "flywheel-toron-gk".into(),
                broaden: false,
                short: Some("gk".into()),
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "2"),
            WantToken {
                name: "flywheel-toron-2".into(),
                broaden: false,
                short: Some("2".into()),
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "oc"),
            WantToken {
                name: "flywheel-toron-oc".into(),
                broaden: true,
                short: Some("oc".into()),
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "pi"),
            WantToken {
                name: "flywheel-toron-pi".into(),
                broaden: true,
                short: Some("pi".into()),
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "w1:p1"),
            WantToken {
                name: "w1:p1".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", ""),
            WantToken {
                name: String::new(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
    }

    /// bd-send-crew-addressable-7sjo (GS-028 NEW-1): composed crew pane names
    /// (`crew-<workflow>-<role>[N]`) are concrete — they pass through exact
    /// with no broadening, never wrapped to `flywheel-<base>-crew-...`.
    #[test]
    fn to_want_token_send_crew_names_pass_through() {
        assert_eq!(
            to_want_token("toron", "crew-plan-coder"),
            WantToken {
                name: "crew-plan-coder".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "crew-wave-swarm-coder2"),
            WantToken {
                name: "crew-wave-swarm-coder2".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
        assert_eq!(
            to_want_token("toron", "  crew-plan-coder  "),
            WantToken {
                name: "crew-plan-coder".into(),
                broaden: false,
                short: None,
                ordinal: 1,
                pane: None,
            }
        );
    }

    /// bd-enov (F3): the briefing dirt preflight names uncommitted ADRs from
    /// `git status --porcelain` and ignores unrelated dirt.
    #[test]
    fn uncommitted_decision_docs_names_adrs_only() {
        let porcelain = " M docs/decisions/0032-swarm-dispatch.md\n\
                         M src/main.rs\n\
                         ?? docs/decisions/0033-new.md\n\
                         R  docs/old.md -> docs/decisions/0034-renamed.md\n\
                         M docs/guides/reservations.mdx\n";
        assert_eq!(
            uncommitted_decision_docs(porcelain),
            vec![
                "docs/decisions/0032-swarm-dispatch.md".to_string(),
                "docs/decisions/0033-new.md".to_string(),
                "docs/decisions/0034-renamed.md".to_string(),
            ]
        );
        assert_eq!(
            uncommitted_decision_docs(" M src/main.rs\n"),
            [] as [String; 0]
        );
        assert_eq!(uncommitted_decision_docs(""), [] as [String; 0]);
    }
}

#[cfg(test)]
mod plane_gate_tests {
    use super::cmd_add;

    /// A typo'd backend must refuse at the plane gate BEFORE the add effect
    /// row is journaled. The hc arm dials the socket directly (bypassing the
    /// factory), so without the gate `FLYWHEEL_BACKEND=nonsense` silently
    /// added panes through hc.
    #[test]
    fn add_refuses_an_unknown_backend_before_journaling() {
        let _g = crate::config::lock_env();
        let prev = std::env::var_os("FLYWHEEL_BACKEND");
        unsafe { std::env::set_var("FLYWHEEL_BACKEND", "nonsense") };
        let cfg = crate::config::Config::default();
        let out = cmd_add(
            &cfg,
            "typo-probe",
            &crate::AgentCounts::default(),
            &[],
            0,
            None,
        );
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("FLYWHEEL_BACKEND", v),
                None => std::env::remove_var("FLYWHEEL_BACKEND"),
            }
        }
        let err = out.expect_err("a typo'd backend must refuse");
        assert!(
            err.to_string().contains("unknown multiplexer backend"),
            "names the typo: {err}"
        );
        assert!(
            !crate::state::project_log_path("typo-probe").exists(),
            "the gate must run before the effect journal writes a row"
        );
    }
}

#[cfg(test)]
mod consent_gate_tests {
    use crate::swarm_plan::{ConsentItem, ConsentReason, Plan};

    fn blocked_jev_plan() -> Plan {
        let mut plan = Plan::new("swarm");
        plan.needs_consent = vec![ConsentItem {
            seat: "coder".to_string(),
            field: "model".to_string(),
            reason: ConsentReason::Jev,
        }];
        plan
    }

    #[test]
    fn consent_gate_refuses_without_yes_and_names_items() {
        let plan = blocked_jev_plan();
        let err = super::consent_gate(&plan, false).expect_err("needs --yes");
        assert!(
            err.iter()
                .any(|l| l.contains("coder") && l.contains("model")),
            "names the seat and field: {err:?}"
        );
        assert!(
            err.iter().any(|l| l.contains("--yes")),
            "hint names the flag: {err:?}"
        );
        assert!(
            err.iter().any(|l| l.contains("plan apply")),
            "hint says there is no plan apply yet: {err:?}"
        );
    }

    #[test]
    fn consent_gate_proceeds_with_yes() {
        let plan = blocked_jev_plan();
        assert!(super::consent_gate(&plan, true).is_ok());
        let clean = Plan::new("swarm");
        assert!(super::consent_gate(&clean, false).is_ok());
    }

    #[test]
    fn dry_run_carries_needs_consent_for_unregistered() {
        use std::collections::BTreeMap;
        let reg = crate::models::ModelRegistry {
            models: BTreeMap::new(),
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        };
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let items = super::spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let plan =
            super::build_dry_run_plan(&cfg, &items, &catalog, &reg, "totally-unknown-9z", "")
                .expect("plan builds");
        let item = plan
            .needs_consent
            .iter()
            .find(|item| item.reason == crate::swarm_plan::ConsentReason::Unregistered)
            .expect("unregistered ask needs sign-off, so consent is required");
        assert_eq!(item.field, "model");
    }

    #[test]
    fn dry_run_ordinary_model_leaves_no_consent() {
        use std::collections::BTreeMap;
        let reg = crate::models::ModelRegistry {
            models: BTreeMap::new(),
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        };
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let items = super::spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let plan =
            super::build_dry_run_plan(&cfg, &items, &catalog, &reg, "", "").expect("plan builds");
        assert!(
            plan.needs_consent.is_empty(),
            "no ask, no consent: {:?}",
            plan.needs_consent
        );
    }
}
