//! `flywheel add` / `flywheel steer` verbs + preflight gate (split from
//! `main.rs`, bd-wave5). Crew creation lives here; the send verbs live in
//! [`super::send`].

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::hc::AgentInfo;
use crate::output::{kind_paint, print_json};
use crate::{agent_ref, backend, state};

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
    let plan = crate::recipe_counts(None, counts, kinds)?;
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    let add_base = format!("{}-add", crate::swarm_label(project));
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
    if crate::pane::rex_door_selected(cfg) {
        return cmd_add_rex(cfg, project, counts, kinds, settle_secs, request_key);
    }
    let plan = crate::recipe_counts(None, counts, kinds)?;
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    let add_base = format!("{}-add", crate::swarm_label(project));
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
                crate::prime_bootstrap(&mut c, &prev, &kind);
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
    crate::log(
        project,
        state::EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    add_effect.settle(Some(&format!("workspace={ws_id} added={}", spawned.len())));
    Ok(())
}

fn preflight_gate(
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
            let snap = crate::snapshot::assemble(cfg, p, &ws_label, crate::doorbell_now_unix());
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
