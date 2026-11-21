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
    LaunchCtx, ProfileLaunchPlan, SpawnOpts, agent_name_n, bare_model_launch,
    bind_pane_mail_identity, bind_spawn_role, boot_prime_line, check_project_spelling, cmd_arrive,
    comms_inbox_on, dead_panes, ensure_and_join_channels, free_spawn_ordinal, is_still_booting,
    log, needs_in_arg_prime, note_role_bind_result, pane_takes_hc_stamp, peer_prompt_template,
    pinned_mail_identity, preflight_kind_integration, prime_bootstrap, prime_line_via_send_text,
    probe_agent_liveness, profile_launch_plan, project_dir, project_workspaces,
    publish_presence_for_registry, read_integration_statuses, recipe_counts, refuse_unpinned_spawn,
    slugify_label, spawn_items_from_plan, spawn_join_notice, spawn_model_rows,
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
/// The dispatcher's key for a workspace: the live session id, or the label
/// only when the plane answered none.
fn dispatcher_workspace<'a>(ws: &'a hc::WorkspaceInfo, label: &'a str) -> &'a str {
    if ws.workspace_id.is_empty() {
        label
    } else {
        ws.workspace_id.as_str()
    }
}

/// The rex add extras, bundled to keep cmd_add_rex within the arg budget.
pub(crate) struct RexAddExtras<'a> {
    pub request_key: Option<&'a str>,
    pub join_channels: &'a [String],
    pub model: Option<&'a str>,
}

pub(crate) fn cmd_add_rex(
    cfg: &Config,
    project: &str,
    counts: &crate::AgentCounts,
    kinds: &[(String, u32)],
    settle_secs: u64,
    extras: RexAddExtras<'_>,
) -> Result<()> {
    let RexAddExtras {
        request_key,
        join_channels,
        model,
    } = extras;
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
    let model_reg = crate::models::ModelRegistry::load_routing().ok();
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
            let (mut add_args, env_model_pin) = add_model_launch(&kind, model)?;
            if crate::needs_in_arg_prime(&kind) {
                add_args.push(crate::boot_prime_line().to_string());
            }
            let mut pane_env = crate::hc::plane_launch_env(cfg, mail_identity.as_deref());
            if let Some(m) = &env_model_pin {
                crate::hc::apply_opencode_model_pin(&mut pane_env, m)?;
            }
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
            record_added_seat(
                cfg,
                project,
                &name,
                &prev,
                &kind,
                mail_identity.as_deref(),
                AddedSeatModel {
                    reg: model_reg.as_ref(),
                    ask: model,
                },
            );
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
    if !join_channels.is_empty() {
        let pins: Vec<String> = registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        ensure_and_join_channels(project, join_channels, &pins);
    }
    crate::log(
        project,
        state::EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    add_effect.settle(Some(&format!("workspace={ws_id} added={}", spawned.len())));
    Ok(())
}

/// Persist an `add` seat's harness and model (bd-8fiwv): the add path used to
/// leave both empty, so `swarm status` showed `-` and the reconciler saw an
/// empty harness. The model is the harness's fallback route (coupled default,
/// then the implied slot), the same value a bare spawn would launch.
/// The model inputs for an added seat: registry plus an optional explicit
/// ask. One struct keeps `record_added_seat` within clippy's arg budget.
pub(crate) struct AddedSeatModel<'a> {
    pub reg: Option<&'a crate::models::ModelRegistry>,
    pub ask: Option<&'a str>,
}

impl<'a> AddedSeatModel<'a> {
    pub fn ask_else_fallback(&self, kind: &str) -> Option<String> {
        match self.ask.filter(|m| !m.trim().is_empty()) {
            // bd-iu00a: an explicit --model wins (from: user).
            Some(ask) => Some(ask.trim().to_string()),
            None => self
                .reg
                .and_then(|r| bare_fallback_model(r, kind))
                .map(|(choice, _)| choice.as_ask().to_string()),
        }
    }
}

/// bd-syne9: an explicit `--model` on `add` must reach the launch, not just
/// the registry. Mirrors the bare spawn lane — argv where the harness takes a
/// flag, pane env where it does not (opencode's root rejects `--model`).
fn add_model_launch(kind: &str, ask: Option<&str>) -> Result<(Vec<String>, Option<String>)> {
    let Some(ask) = ask.filter(|m| !m.trim().is_empty()) else {
        return Ok((Vec::new(), None));
    };
    crate::bare_model_launch(
        kind,
        &crate::models::ModelChoice::parse(ask.trim()),
        "",
        None,
    )
}

pub(crate) fn record_added_seat(
    cfg: &Config,
    project: &str,
    name: &str,
    pane_id: &str,
    kind: &str,
    mail_identity: Option<&str>,
    model: AddedSeatModel<'_>,
) {
    let mut entry = match mail_identity {
        Some(mid) => state::SpawnEntry::with_mail_identity(pane_id, name, mid),
        None => state::SpawnEntry::legacy(pane_id, name),
    };
    entry.harness = Some(kind.to_string());
    entry.model = model.ask_else_fallback(kind);
    entry.model_pin = entry.model.clone();
    crate::record_spawn_entry(cfg, project, entry);
}

/// bd-koafr: the add door takes ONE options struct, like spawn, so the
/// argument count stays clippy-clean as options grow.
#[derive(Debug, Clone, Default)]
pub(crate) struct AddOpts {
    pub counts: crate::AgentCounts,
    pub kinds: Vec<(String, u32)>,
    pub settle_secs: u64,
    pub request_key: Option<String>,
    pub join_channels: Vec<String>,
    pub assign: Vec<String>,
    /// bd-iu00a: an optional explicit model ask; None means harness-default.
    pub model: Option<String>,
}

pub(crate) fn cmd_add(cfg: &Config, project: &str, opts: AddOpts) -> Result<()> {
    let AddOpts {
        counts,
        kinds,
        settle_secs,
        request_key,
        join_channels,
        assign,
        model,
    } = opts;
    let counts = &counts;
    let kinds = &kinds;
    let request_key = request_key.as_deref();
    let join_channels = &join_channels;
    crate::check_project_spelling(project, "add")?;
    let assign = super::spawn::parse_assign(&assign)?;
    let _ = &assign;
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
        return cmd_add_rex(
            cfg,
            project,
            counts,
            kinds,
            settle_secs,
            RexAddExtras {
                request_key,
                join_channels,
                model: model.as_deref(),
            },
        );
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
    let model_reg = crate::models::ModelRegistry::load_routing().ok();
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
            let (launch_args, env_model_pin) = add_model_launch(&kind, model.as_deref())?;
            let mut pane_env = crate::hc::plane_launch_env(cfg, mail_identity.as_deref());
            if let Some(m) = &env_model_pin {
                crate::hc::apply_opencode_model_pin(&mut pane_env, m)?;
            }
            let p = crate::hc::pane_split(
                &c,
                &prev,
                "down",
                Some(&crate::utf8_path(&dir, "workspace dir")?),
                Some(&pane_env),
            )?;
            prev = p.pane_id.clone();
            let mut add_args: Vec<String> = launch_args;
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
            record_added_seat(
                cfg,
                project,
                &name,
                &agent.pane_id,
                &kind,
                mail_identity.as_deref(),
                crate::cmd::spawn::AddedSeatModel {
                    reg: model_reg.as_ref(),
                    ask: model.as_deref(),
                },
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
    if !join_channels.is_empty() {
        let pins: Vec<String> = registry
            .iter()
            .filter_map(|e| e.mail_identity.clone())
            .collect();
        ensure_and_join_channels(project, join_channels, &pins);
    }
    crate::log(
        project,
        state::EventKind::Add,
        json!({ "workspace_id": ws_id, "agents": spawned }),
    );
    add_effect.settle(Some(&format!("workspace={ws_id} added={}", spawned.len())));
    dispatch_assignments(project, &assign, None)?;
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
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostedParkOpts<'a> {
    pub project_owned: &'a str,
    pub pin: &'a str,
    pub objective: &'a str,
    pub direct: bool,
    pub crew: Option<&'a str>,
    pub reuse: Option<&'a str>,
    pub retain: bool,
    pub retire_orphans: bool,
    pub request_key: Option<&'a str>,
}

pub(crate) fn cmd_steer_hosted_park(opts: HostedParkOpts<'_>) -> Result<bool> {
    let HostedParkOpts {
        project_owned,
        pin,
        objective,
        direct,
        crew,
        reuse,
        retain,
        retire_orphans,
        request_key,
    } = opts;
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct SteerOpts<'a> {
    pub objective: &'a str,
    pub direct: bool,
    pub crew: Option<&'a str>,
    pub project: Option<&'a str>,
    pub reuse: Option<&'a str>,
    pub retain: bool,
    pub retire_orphans: bool,
    pub json: bool,
    pub yes: bool,
    pub request_key: Option<&'a str>,
    pub as_agent: Option<&'a str>,
}

pub(crate) fn cmd_steer(cfg: &Config, opts: SteerOpts<'_>) -> Result<()> {
    let SteerOpts {
        objective,
        direct,
        crew,
        project,
        reuse,
        retain,
        retire_orphans,
        json,
        yes,
        request_key,
        as_agent,
    } = opts;
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
        && cmd_steer_hosted_park(HostedParkOpts {
            project_owned: &project_owned,
            pin,
            objective,
            direct,
            crew,
            reuse,
            retain,
            retire_orphans,
            request_key,
        })?
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
/// ADR-0022 §3.1: spawn's launch decision IS the verified PlanMachine's
/// verdict. Pure: builds the machine with `PlanContext { yes, ..default }`,
/// steps `Resolve { needs_consent, jev: Ok }` — the plan records no Jev
/// failure, so `Ok` is honest today — and reads the resulting state:
/// Confirmed launches, anything else asks with today's refusal lines.
/// The PlanCmd `Journal` and `Launch` commands are descriptive here, never
/// executed: executing `Journal` comes with the effect journal wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanGate {
    Launch,
    Ask {
        consent: Vec<String>,
        blocked: Vec<String>,
    },
}

pub(crate) fn plan_gate(plan: &crate::swarm_plan::Plan, yes: bool) -> PlanGate {
    use crate::elm::plan_statechart::{JevOutcome, PlanEvent, PlanState, build_plan_machine};
    use toron_core::kernel::MachineState;
    let machine = build_plan_machine();
    let start = MachineState::at(
        PlanState::Drafting,
        crate::elm::plan_statechart::PlanContext {
            yes,
            ..Default::default()
        },
    );
    let out = machine.step_state(
        &start,
        &PlanEvent::Resolve {
            needs_consent: plan.needs_consent.clone(),
            jev: JevOutcome::Ok,
        },
    );
    if out.configuration.primary() == Some(&PlanState::Confirmed) {
        return PlanGate::Launch;
    }
    let mut consent = Vec::new();
    if !yes {
        for item in &plan.needs_consent {
            consent.push(format!(
                "consent required: {} {} ({})",
                item.seat,
                item.field,
                item.reason.as_str()
            ));
        }
        consent.push("re-run with --yes to proceed".to_string());
    }
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
    PlanGate::Ask { consent, blocked }
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
    ask: &crate::models::ModelChoice,
    from: crate::swarm_plan::Provenance,
    effort: Option<&str>,
) -> (crate::swarm_plan::Field, String) {
    use crate::models::{ModelChoice, RegistryMembership, Resolution};
    use crate::swarm_plan::{resolved_field, unresolved_field};
    let ModelChoice::Model(ask) = ask else {
        return (
            crate::swarm_plan::harness_default_field(from),
            String::new(),
        );
    };
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

/// Load the standing-preference store for a spawn: a missing file reads as
/// empty; a corrupt file fails loud naming the path.
fn load_spawn_preferences(path: &std::path::Path) -> Result<crate::preferences::PreferenceSet> {
    crate::preferences::PreferenceSet::load(path)
}

/// Effective bare ask with provenance: user `--model` (User), else the
/// standing model preference (Preference), else none (the existing default
/// chain decides). Provider pairs the user ask; effort is
/// preference-or-nothing. The plan and the launch read this: one
/// precedence, two readers.
pub(crate) struct BareAsk {
    model: Option<(crate::models::ModelChoice, crate::swarm_plan::Provenance)>,
    provider: String,
    effort: Option<(String, crate::swarm_plan::Provenance)>,
}

fn prefer_bare_inputs(
    prefs: &crate::preferences::PreferenceSet,
    item: &crate::SpawnItem,
    project: &str,
    bare_model: &str,
    bare_provider: &str,
) -> BareAsk {
    use crate::models::ModelChoice;
    use crate::swarm_plan::Provenance;
    let effort = crate::prefer_fill(prefs, item, crate::preferences::Field::Effort, project)
        .map(|value| (value, Provenance::Preference));
    if bare_model.is_empty() {
        match crate::prefer_fill(prefs, item, crate::preferences::Field::Model, project) {
            Some(model) => BareAsk {
                model: Some((ModelChoice::parse(&model), Provenance::Preference)),
                provider: String::new(),
                effort,
            },
            None => BareAsk {
                model: None,
                provider: String::new(),
                effort,
            },
        }
    } else {
        BareAsk {
            model: Some((ModelChoice::parse(bare_model), Provenance::User)),
            provider: bare_provider.to_string(),
            effort,
        }
    }
}

/// Per-role patch overrides: the single lowering of `--patch` specs into
/// launch inputs. `build_dry_run_plan` and every launch path
/// read this map, so plan and launch are one decision. Later patches win
/// per field; a model override clears its provider (the plan resolves the
/// raw value, so the launch must look it up raw too).
#[derive(Default)]
pub(crate) struct RoleOverride {
    pub harness: Option<String>,
    pub model: Option<(crate::models::ModelChoice, String)>,
    pub effort: Option<String>,
    pub mode: Option<String>,
}

/// Lower parsed patches into per-role launch inputs. ONE place where a patch
/// becomes launch input (bd-8g3uo rework).
pub(crate) fn patch_role_overrides(
    patches: &[crate::swarm_plan::PlanPatch],
) -> std::collections::BTreeMap<String, RoleOverride> {
    let mut map = std::collections::BTreeMap::new();
    for p in patches {
        let e = map
            .entry(p.seat.clone())
            .or_insert_with(RoleOverride::default);
        match &p.field {
            crate::swarm_plan::PatchField::Harness => e.harness = Some(p.value.clone()),
            crate::swarm_plan::PatchField::Model => {
                e.model = Some((crate::models::ModelChoice::parse(&p.value), String::new()))
            }
            crate::swarm_plan::PatchField::Effort => e.effort = Some(p.value.clone()),
            crate::swarm_plan::PatchField::Mode => e.mode = Some(p.value.clone()),
            // `spawn --patch` rejects `role` before it gets here.
            crate::swarm_plan::PatchField::Role => {}
        }
    }
    map
}

/// Effective bare launch triple for one role: the base ask with the role's
/// patch overrides applied. Every bare launch site (hc first, hc pane, rex)
/// AND the launch-parity test read this — one application point.
pub(crate) fn patched_bare_launch(
    overrides: &std::collections::BTreeMap<String, RoleOverride>,
    role: &str,
    ask: BareAsk,
) -> (crate::models::ModelChoice, String, Option<String>) {
    let ov = overrides.get(role);
    let (model, provider) = match ov.and_then(|o| o.model.clone()) {
        Some(pair) => pair,
        None => (
            ask.model
                .map(|(m, _)| m)
                .unwrap_or_else(|| crate::models::ModelChoice::parse("")),
            ask.provider,
        ),
    };
    let effort = ov
        .and_then(|o| o.effort.clone())
        .or_else(|| ask.effort.map(|(e, _)| e));
    (model, provider, effort)
}

/// Apply a role's patch overrides to a profile shadow in place. Same map as
/// the bare path; the launch plan builders then see patched values.
pub(crate) fn patch_shadow_profile(
    overrides: &std::collections::BTreeMap<String, RoleOverride>,
    role: &str,
    shadow: &mut crate::profile::Profile,
) {
    if let Some(o) = overrides.get(role) {
        if let Some((m, _)) = &o.model {
            shadow.model = m.as_ask().to_string();
        }
        if let Some(e) = &o.effort {
            shadow.effort = e.clone();
        }
    }
}

/// Mode argv for one role through the same builder the plan uses
/// (`headless::mode_launch_args`): empty when unpatched or undeployable.
/// Every launch path that reads overrides appends this to the seat's argv.
pub(crate) fn patched_mode_argv(
    overrides: &std::collections::BTreeMap<String, RoleOverride>,
    role: &str,
    harness: &str,
) -> Vec<String> {
    match overrides.get(role).and_then(|o| o.mode.clone()) {
        Some(m) => crate::headless::mode_launch_args(harness, &m).0,
        None => Vec::new(),
    }
}

/// Records the launched harness when the profile set none.
pub(crate) fn with_recorded_harness(mut entry: state::SpawnEntry, kind: &str) -> state::SpawnEntry {
    if entry.harness.is_none() {
        entry.harness = Some(kind.to_string());
    }
    entry
}

/// bd-f888z: opencode v2 accepts `--session/-s <id>` ("continue, or create if
/// it does not exist"). Mint a unique session for each opencode seat so its
/// wakes can ride `session.prompt` instead of typing into the TUI. bd-edva3:
/// antigravity's conversation id is learned from the SEAT by a PreInvocation
/// hook, never from the spawner's environment. Other seats get `None`.
/// bd-4n2bn: harnesses whose seats take the pi-style per-seat agentDir
/// events bridge. omp is a pi fork honoring PI_CODING_AGENT_DIR (verified
/// in the omp 18.2.5 binary strings); opencode has its own config path.
pub(crate) fn pi_style_events_kind(kind: &str) -> bool {
    matches!(kind, "pi" | "omp")
}

/// bd-rrlo2: harnesses whose seats get a per-pin events token minted at
/// spawn. Config-file bridges (opencode, pi/omp) and the pump's
/// events-only seats (gemini/antigravity/grok/codex); every other kind
/// keeps the doorbell fallback and no token file.
pub(crate) fn has_events_door(kind: &str) -> bool {
    let canonical = crate::harness_registry::canonical(kind);
    crate::events_source::events_only_harness(&canonical)
        || matches!(canonical.as_str(), "opencode" | "pi" | "omp")
}

/// bd-rrlo2: outcome of the spawn-time token ensure, for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeatTokenOutcome {
    Minted,
    Rotated,
    Current,
}

/// bd-rrlo2: make sure the seat's token file exists before launch, minting
/// (journaled, 0600, TTL) or rotating via the existing rotation when due.
/// A missing journal beside an existing file re-mints to heal it. Never
/// writes plaintext anywhere but the 0600 file.
pub(crate) fn ensure_seat_events_token<C: crate::events_tokens::TokenCli>(
    cli: &C,
    state_dir: &std::path::Path,
    project: &str,
    pin: &str,
    now_us: i64,
) -> Result<SeatTokenOutcome, String> {
    use crate::events_tokens as et;
    let token_path = et::token_path(state_dir, project, pin);
    let journal = state_dir.join("events-token-journal.jsonl");
    if token_path.is_file()
        && let Ok((hash, expires_us)) = et::last_minted(&journal, project, pin)
    {
        let state = et::TokenState {
            hash,
            minted_us: 0,
            expires_us,
            path: token_path.display().to_string(),
        };
        if let Some(plan) =
            et::rotation_plan(&state, now_us, (et::ROTATE_BEFORE_SECS as i64) * 1_000_000)
        {
            et::rotate(cli, &plan, pin, project, &token_path, &journal, now_us)?;
            return Ok(SeatTokenOutcome::Rotated);
        }
        return Ok(SeatTokenOutcome::Current);
    }
    et::mint_and_store(
        cli,
        pin,
        project,
        et::PIN_TTL_SECS,
        &token_path,
        &journal,
        now_us,
    )?;
    Ok(SeatTokenOutcome::Minted)
}

pub(crate) fn mint_harness_session(kind: &str, args: &mut Vec<String>) -> Option<String> {
    if crate::harness_registry::canonical(kind) != "opencode" {
        return None;
    }
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("ses-{}-{}-{:x}", std::process::id(), now, seq);
    args.push("--session".to_string());
    args.push(id.clone());
    Some(id)
}

/// Verbatim `add`-accepted flags from a spawn request (bd-gjtcw): the join
/// hint hands back what the operator asked for, never a guessed harness.
pub(crate) fn requested_spawn_flags(opts: &SpawnOpts) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (flag, n) in [
        ("--oc", opts.counts.oc),
        ("--pi", opts.counts.pi),
        ("--gk", opts.counts.gk),
    ] {
        if let Some(n) = n {
            parts.push(format!("{flag} {n}"));
        }
    }
    for (kind, n) in &opts.kinds {
        parts.push(format!("--kind {kind}={n}"));
    }
    parts.join(" ")
}

/// Parse every `--patch` spec once, preserving the raw spec for errors.
fn parse_role_patches(specs: &[String]) -> Result<Vec<crate::swarm_plan::PlanPatch>> {
    specs
        .iter()
        .map(|spec| {
            let patch = crate::swarm_plan::parse_patch(spec)
                .map_err(|e| anyhow::anyhow!("bad --patch {spec:?}: {e}"))?;
            if matches!(patch.field, crate::swarm_plan::PatchField::Role) {
                bail!(
                    "spawn --patch {spec:?}: `role` is the seat's identity in a plan; patch harness|model|effort|mode (a live role change is `swarm apply --patch <seat>.role=…`)"
                );
            }
            Ok(patch)
        })
        .collect()
}

/// Effective profile for one item: standing model/effort preferences fill a
/// copy, so the launch routes exactly what the plan resolved. Returns the
/// shadow plus the model and effort asks with provenance (a profile row
/// falls back to the profile's own values). The plan and the launch read
/// this: one precedence, two readers. The harness still comes from
/// `prefer_kind` at the call site.
type Ask = Option<(String, crate::swarm_plan::Provenance)>;

fn prefer_shadow_profile(
    prefs: &crate::preferences::PreferenceSet,
    item: &crate::SpawnItem,
    profile: &profile::Profile,
    project: &str,
) -> (profile::Profile, Ask, Ask) {
    use crate::swarm_plan::Provenance;
    let mut shadow = profile.clone();
    let model = if let Some(value) =
        crate::prefer_fill(prefs, item, crate::preferences::Field::Model, project)
    {
        shadow.model = value.clone();
        Some((value, Provenance::Preference))
    } else if shadow.model.is_empty() {
        None
    } else {
        Some((shadow.model.clone(), Provenance::Profile))
    };
    let effort = if let Some(value) =
        crate::prefer_fill(prefs, item, crate::preferences::Field::Effort, project)
    {
        shadow.effort = value.clone();
        Some((value, Provenance::Preference))
    } else if shadow.effort.is_empty() {
        None
    } else {
        Some((shadow.effort.clone(), Provenance::Profile))
    };
    (shadow, model, effort)
}

/// The request half of a dry-run plan: what to spawn, with what bare ask and
/// with which per-role patches.
pub(crate) struct PlanAsk<'a> {
    pub items: &'a [crate::SpawnItem],
    pub bare_model: &'a str,
    pub bare_provider: &'a str,
    pub overrides: &'a std::collections::BTreeMap<String, RoleOverride>,
}

pub(crate) fn build_dry_run_plan(
    cfg: &Config,
    catalog: &profile::ProfileCatalog,
    registry: &models::ModelRegistry,
    prefs: &crate::preferences::PreferenceSet,
    project: &str,
    ask: PlanAsk<'_>,
) -> Result<crate::swarm_plan::Plan> {
    use crate::swarm_plan::seat_status;
    let mut plan = crate::swarm_plan::Plan::new("swarm");
    for item in ask.items {
        let (harness, model, effort) = if item.profile.is_some() {
            let p = catalog
                .get(item.profile.as_deref().unwrap_or(""))
                .ok_or_else(|| {
                    anyhow!(
                        "unknown profile '@{}' (flywheel profile list)",
                        item.profile.as_deref().unwrap_or("")
                    )
                })?;
            let (_, model_ask, effort) = prefer_shadow_profile(prefs, item, p, project);
            let harness = crate::prefer_kind(cfg, item, catalog, registry, prefs, project)?;
            let model = match model_ask {
                Some((ask, from)) => match crate::models::ModelChoice::parse(&ask) {
                    crate::models::ModelChoice::HarnessDefault => {
                        Some((crate::models::ModelChoice::HarnessDefault, from))
                    }
                    crate::models::ModelChoice::Model(ask) => {
                        let (_, resolved) = registry.resolve_profile_route(&harness, &ask)?;
                        if resolved.is_empty() {
                            None
                        } else {
                            Some((crate::models::ModelChoice::Model(resolved), from))
                        }
                    }
                },
                None => None,
            };
            (harness, model, effort)
        } else {
            let harness = crate::prefer_kind(cfg, item, catalog, registry, prefs, project)?;
            let ask = prefer_bare_inputs(prefs, item, project, ask.bare_model, ask.bare_provider);
            let model = match ask.model {
                Some((choice, from)) => Some((bare_with_provider(choice, &ask.provider), from)),
                None => bare_fallback_model(registry, &harness),
            };
            (harness, model, ask.effort)
        };
        let patch = ask.overrides.get(item.short.as_str());
        let harness = patch.and_then(|o| o.harness.clone()).unwrap_or(harness);
        let model = match patch.and_then(|o| o.model.clone()) {
            Some((m, _)) => Some((m, crate::swarm_plan::Provenance::User)),
            None => model,
        };
        let effort = match patch.and_then(|o| o.effort.clone()) {
            Some(e) => Some((e, crate::swarm_plan::Provenance::User)),
            None => effort,
        };
        let patch_mode = patch
            .and_then(|o| o.mode.clone())
            .map(|m| (m, crate::swarm_plan::Provenance::User));
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
        let mode_field = crate::swarm_plan::mode_field(&harness, patch_mode);
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
pub(crate) fn bare_fallback_model(
    registry: &models::ModelRegistry,
    harness: &str,
) -> Option<(crate::models::ModelChoice, crate::swarm_plan::Provenance)> {
    use crate::models::ModelChoice;
    if let Some(id) = registry.coupled_default_model(harness)
        && !id.is_empty()
    {
        return Some((
            ModelChoice::Model(id),
            crate::swarm_plan::Provenance::HarnessDefault,
        ));
    }
    // bd-iu00a: an OPEN harness with no model ask uses its OWN configured
    // model. Only a closed/coupled harness keeps the implied slot fallback.
    if crate::harness_registry::tier_of(harness).is_some_and(|t| t.may_be_asked_for_a_model()) {
        return Some((
            ModelChoice::HarnessDefault,
            crate::swarm_plan::Provenance::HarnessDefault,
        ));
    }
    if let Some(slot) = models::implied_slot_name_for_harness(harness)
        .and_then(|name| registry.effective_slot_model(name))
        && !slot.is_empty()
    {
        return Some((
            ModelChoice::Model(slot),
            crate::swarm_plan::Provenance::Slot,
        ));
    }
    None
}

/// `--provider` joins a concrete bare id; the harness-default ask has no id
/// to prefix, so it passes through untouched.
fn bare_with_provider(
    choice: crate::models::ModelChoice,
    provider: &str,
) -> crate::models::ModelChoice {
    use crate::models::ModelChoice;
    match choice {
        ModelChoice::HarnessDefault => ModelChoice::HarnessDefault,
        ModelChoice::Model(id) if provider.is_empty() => ModelChoice::Model(id),
        ModelChoice::Model(id) => ModelChoice::Model(format!("{provider}/{id}")),
    }
}

pub(crate) fn cmd_spawn(cfg: &Config, project: &str, opts: SpawnOpts) -> Result<()> {
    check_project_spelling(project, "spawn")?;
    let assign = super::spawn::parse_assign(&opts.assign)?;
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
    let prefs = load_spawn_preferences(&crate::preferences::preferences_path())?;
    if plane.plane == crate::backend::Plane::Rex {
        #[cfg(feature = "backend-superlogical")]
        {
            if !plane.rex_reachable {
                bail!(
                    "spawn: {} — install the rex CLI or select the fallback (FLYWHEEL_BACKEND=hc)",
                    plane.reason
                );
            }
            return cmd_spawn_rex(cfg, project, opts, &prefs);
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
    let patches = parse_role_patches(&opts.patches)?;
    let overrides = patch_role_overrides(&patches);
    let plan = build_dry_run_plan(
        cfg,
        &catalog,
        &model_reg,
        &prefs,
        project,
        PlanAsk {
            items: &items,
            bare_model: &opts.bare_model,
            bare_provider: &opts.bare_provider,
            overrides: &overrides,
        },
    )?;
    for (spec, patch) in opts.patches.iter().zip(patches.iter()) {
        crate::swarm_plan::validate_patch(&plan, patch)
            .map_err(|e| anyhow::anyhow!("bad --patch {spec:?}: {e}"))?;
    }
    if opts.dry_run {
        // bd-koafr: the assignment rides the envelope, never the seat plan.
        let doc = dry_run_doc(&plan, &assign);
        if opts.json {
            print_json(&doc);
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into())
            );
        }
        return Ok(());
    }
    if let PlanGate::Ask { consent, blocked } = plan_gate(&plan, opts.yes) {
        for line in &consent {
            eprintln!("consent: {line}");
        }
        for line in &blocked {
            eprintln!("blocked: {line}");
        }
        if !blocked.is_empty() {
            anyhow::bail!(
                "spawn refused: {} blocked field(s) would not be honoured — fix the ask (a blocked ask must not launch)",
                blocked.len()
            );
        }
        anyhow::bail!(
            "spawn refused: {} consent item(s) need operator sign-off — re-run with --yes",
            plan.needs_consent.len()
        );
    }
    // ADR-0022 D1 (bd-6setu.3): the plan is a durable object from the moment it
    // resolves to launch. The id is minted once and kept, so a replay names the
    // plan the swarm runs instead of rebuilding it from flags.
    let plan_record = crate::state::record_plan(project, &plan)?;
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
        let rows = spawn_model_rows(
            &items,
            &catalog,
            &model_reg,
            cfg,
            &opts.bare_model,
            &prefs,
            project,
        )?;
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
    let plane_backend = backend::multiplexer_backend(cfg)?;
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
            let alive = probe_agent_liveness(plane_backend.as_ref(), &a.pane_id, kind, name);
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
    let mut adopt: Option<hc::WorkspaceInfo> = None;
    if !orphans.is_empty() && opts.retire_orphans {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
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
    } else if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        let live_ids: HashSet<&str> = orphans.iter().map(|ws| ws.workspace_id.as_str()).collect();
        let live_agents: Vec<hc::AgentInfo> = all_agents
            .iter()
            .filter(|a| live_ids.contains(a.workspace_id.as_str()))
            .cloned()
            .collect();
        // The ACTIVE plane is the truth for pane liveness: a closed rex pane
        // can linger in the hc client's list and look like a live fleet.
        let live_panes: std::collections::HashSet<String> = backend::multiplexer_backend(cfg)
            .ok()
            .and_then(|b| b.pane_list(None).ok())
            .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
            .unwrap_or_default();
        match crate::classify_spawn_workspace(&orphans, &live_agents, &live_panes) {
            crate::SpawnWorkspace::Fresh => {}
            crate::SpawnWorkspace::Add { index } => {
                let existing = orphans[index].clone();
                eprintln!(
                    "flywheel: {project}: workspace {} hosts no crew pins — adding panes into it",
                    existing.workspace_id
                );
                adopt = Some(existing);
            }
            crate::SpawnWorkspace::Join => {
                if opts.join {
                    eprintln!(
                        "{}",
                        spawn_join_notice(
                            project,
                            &detail.join(", "),
                            &requested_spawn_flags(&opts)
                        )
                    );
                    let pin =
                        crate::authority::select_agent_identity(None, &|k| std::env::var(k).ok());
                    return cmd_arrive(cfg, project, pin.as_deref(), None, None, None);
                }
                bail!(
                    "found {} pre-existing workspace(s) for {project} — stale panes from a killed swarm would race the new one: {}. Close them first (flywheel kill {project} or terminal workspace close <id>), pass --join to attach, or --retire-orphans to close them automatically.",
                    orphans.len(),
                    detail.join(", ")
                );
            }
        }
    }
    let first_pin = items.first().and_then(|item| {
        let kind = crate::prefer_kind(cfg, item, &catalog, &model_reg, &prefs, project).ok()?;
        pinned_mail_identity(cfg, project, &agent_name_n(project, &item.short, 1), &kind)
    });
    let mut first_bare: Option<(Vec<String>, Option<String>)> = None;
    let mut first_plan: Option<ProfileLaunchPlan> = match items.first() {
        Some(item) => {
            let kind = crate::prefer_kind(cfg, item, &catalog, &model_reg, &prefs, project)?;
            let kind = overrides
                .get(item.short.as_str())
                .and_then(|o| o.harness.clone())
                .unwrap_or(kind);
            match item.profile.as_deref().map(|pn| catalog.get(pn)) {
                Some(Some(p)) => {
                    let (mut shadow, _, _) = prefer_shadow_profile(&prefs, item, p, project);
                    patch_shadow_profile(&overrides, item.short.as_str(), &mut shadow);
                    let mut plan = profile_launch_plan(&kind, &shadow, None)?;
                    plan.args
                        .extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
                    Some(plan)
                }
                _ => {
                    let ask = prefer_bare_inputs(
                        &prefs,
                        item,
                        project,
                        &opts.bare_model,
                        &opts.bare_provider,
                    );
                    let (eff_model, eff_provider, eff_effort) =
                        patched_bare_launch(&overrides, item.short.as_str(), ask);
                    let (mut args, env_pin) =
                        bare_model_launch(&kind, &eff_model, &eff_provider, eff_effort.as_deref())?;
                    args.extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
                    first_bare = Some((args, env_pin));
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
    let adopted = adopt.is_some();
    let (ws, root) = match adopt {
        Some(existing) => {
            let root = c
                .pane_list()?
                .into_iter()
                .find(|p| p.workspace_id == existing.workspace_id)
                .ok_or_else(|| {
                    anyhow!(
                        "workspace {} has no panes to add into",
                        existing.workspace_id
                    )
                })?;
            println!("workspace {} reused (no crew pins)", existing.workspace_id);
            (existing, root)
        }
        None => {
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
            (ws, root)
        }
    };

    let mut spawned: Vec<Value> = Vec::new();
    let mut booted: Vec<(String, String, String)> = Vec::new();
    let mut registry: Vec<state::SpawnEntry> = Vec::new();
    let mut prev = root.pane_id.clone();
    // An adopted workspace's shell env is fixed: split a fresh pane for the
    // first agent so its model pin still lands.
    let mut first = !adopted;
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
        let kind = crate::prefer_kind(cfg, item, &catalog, &model_reg, &prefs, project)?;
        let kind = overrides
            .get(item.short.as_str())
            .and_then(|o| o.harness.clone())
            .unwrap_or(kind);
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
                    let (mut shadow, _, _) = prefer_shadow_profile(&prefs, item, p, project);
                    patch_shadow_profile(&overrides, item.short.as_str(), &mut shadow);
                    let mut plan = if first {
                        first_plan.take().ok_or_else(|| {
                            anyhow!(
                                "internal: the first agent's plan was resolved before \
                                 workspace.create but is no longer available"
                            )
                        })?
                    } else {
                        profile_launch_plan(&kind, &shadow, None)?
                    };
                    plan.args
                        .extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
                    // bd-t2z0b: say which verified tool build this seat gets.
                    if let Some(v) = &plan.version {
                        eprintln!("flywheel: {name}: {kind} pinned to v{v} (mise.toml)");
                    }
                    let model_pin = match crate::models::ModelChoice::parse(&plan.model) {
                        crate::models::ModelChoice::HarnessDefault => None,
                        crate::models::ModelChoice::Model(_) => Some(plan.model),
                    };
                    (plan.args, model_pin, plan.env_model_pin)
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
                        let ask = prefer_bare_inputs(
                            &prefs,
                            item,
                            project,
                            &opts.bare_model,
                            &opts.bare_provider,
                        );
                        let (eff_model, eff_provider, eff_effort) =
                            patched_bare_launch(&overrides, item.short.as_str(), ask);
                        let (mut args, env_pin) = bare_model_launch(
                            &kind,
                            &eff_model,
                            &eff_provider,
                            eff_effort.as_deref(),
                        )?;
                        args.extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
                        (args, env_pin)
                    };
                    (args, None, env_pin)
                }
            };
            let mut pane_env = hc::plane_launch_env(cfg, mail_identity.as_deref());
            // bd-rrlo2: mint the per-pin events token before launch so the
            // config branches below find a file; without one they keep the
            // doorbell fallback. Fail-open: a mint failure warns and the
            // seat still spawns.
            if has_events_door(&kind)
                && let Some(pin) = mail_identity.as_deref()
            {
                let cli = crate::events_tokens::ToronTokenCli {
                    bin: std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string()),
                };
                let now_us = chrono::Utc::now().timestamp_micros();
                if let Err(e) = ensure_seat_events_token(
                    &cli,
                    &crate::config::state_dir(),
                    project,
                    pin,
                    now_us,
                ) {
                    eprintln!(
                        "flywheel: warn: events token mint failed for {pin}: {e}; using doorbell fallback"
                    );
                }
            }
            // bd-9os20/bd-mdpfr: Claude's flywheel plugin is delivered per seat
            // from the installed data dir (ScarletCove copies it in stack.sh).
            if crate::harness_registry::canonical(&kind) == "claude" {
                let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{home}/.local/share")
                });
                agent_args.push("--plugin-dir".to_string());
                agent_args.push(format!("{data_home}/flywheel/plugins/claude-flywheel"));
            }
            if let Some(m) = &env_model_pin {
                hc::apply_opencode_model_pin(&mut pane_env, m)?;
            }
            // bd-o1z4m: an opencode seat gets a per-pane events config pointing
            // at its 0600 token file. No plaintext enters the config or journal;
            // without a token the doorbell (bd-f888z) fallback stays in charge.
            if kind == "opencode"
                && let Some(pin) = mail_identity.as_deref()
            {
                let token_path =
                    crate::events_tokens::token_path(&crate::config::state_dir(), project, pin);
                if token_path.is_file() {
                    hc::apply_opencode_events_config(&mut pane_env, pin, &token_path)?;
                } else {
                    eprintln!(
                        "flywheel: warn: no events token for {pin}; opencode seat uses the doorbell fallback"
                    );
                }
            }
            // bd-4n2bn: pi and omp seats get a per-seat agentDir whose settings
            // load the first-party events extension; the token is a 0600 file
            // PATH in the pane env, never plaintext. No token -> doorbell
            // fallback. omp is a pi fork honoring PI_CODING_AGENT_DIR.
            if pi_style_events_kind(&kind)
                && let Some(pin) = mail_identity.as_deref()
            {
                let token_path =
                    crate::events_tokens::token_path(&crate::config::state_dir(), project, pin);
                if token_path.is_file() {
                    hc::apply_pi_events_config(&mut pane_env, project, pin, &token_path)?;
                } else {
                    eprintln!(
                        "flywheel: warn: no events token for {pin}; {kind} seat uses the doorbell fallback"
                    );
                }
            }
            // bd-mdpfr: hooks are installed by the operator-run integration
            // verb against the agent's real config dir, never at spawn.
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
            let harness_session = mint_harness_session(&kind, &mut agent_args);
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
                LaunchCtx {
                    profile_name: item.profile.as_deref(),
                    project: Some(project),
                    cfg: Some(cfg),
                },
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
            entry.effort = item
                .profile
                .as_deref()
                .and_then(|pname| catalog.get(pname))
                .map(|p| p.effort.clone())
                .filter(|e| !e.is_empty());
            entry.mode = None;
            entry.skills_digest = skills_digest;
            entry.harness_session =
                harness_session.or_else(|| captain_directory::capture_harness_session(&kind));
            registry.push(with_recorded_harness(entry, &kind));
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
            "workspace_id": ws.workspace_id, "label": label, "agents": spawned,
            "plan_id": plan_record.id
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
        doorbell::ensure_running(
            &exe,
            &config::state_dir(),
            dispatcher_workspace(&ws, &label),
            project,
        )
        .context("doorbell dispatcher")?;
    }

    println!(
        "swarm ready: flywheel {} {} {}",
        kind_paint("send"),
        dim(project),
        dim("--to all --stage ce-execute")
    );
    dispatch_assignments(project, &assign, None)?;
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
pub(crate) fn cmd_spawn_rex(
    cfg: &Config,
    project: &str,
    opts: SpawnOpts,
    prefs: &crate::preferences::PreferenceSet,
) -> Result<()> {
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
    let patches = parse_role_patches(&opts.patches)?;
    let overrides = patch_role_overrides(&patches);
    let plan = build_dry_run_plan(
        cfg,
        &catalog,
        &model_reg,
        prefs,
        project,
        PlanAsk {
            items: &items,
            bare_model: &opts.bare_model,
            bare_provider: &opts.bare_provider,
            overrides: &overrides,
        },
    )?;
    for (spec, patch) in opts.patches.iter().zip(patches.iter()) {
        crate::swarm_plan::validate_patch(&plan, patch)
            .map_err(|e| anyhow::anyhow!("bad --patch {spec:?}: {e}"))?;
    }
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
    if let PlanGate::Ask { consent, blocked } = plan_gate(&plan, opts.yes) {
        for line in &consent {
            eprintln!("consent: {line}");
        }
        for line in &blocked {
            eprintln!("blocked: {line}");
        }
        if !blocked.is_empty() {
            anyhow::bail!(
                "spawn refused: {} blocked field(s) would not be honoured — fix the ask (a blocked ask must not launch)",
                blocked.len()
            );
        }
        anyhow::bail!(
            "spawn refused: {} consent item(s) need operator sign-off — re-run with --yes",
            plan.needs_consent.len()
        );
    }
    // ADR-0022 D1 (bd-6setu.3): the plan is a durable object from the moment it
    // resolves to launch. The id is minted once and kept, so a replay names the
    // plan the swarm runs instead of rebuilding it from flags.
    let plan_record = crate::state::record_plan(project, &plan)?;
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
        let rows = spawn_model_rows(
            &items,
            &catalog,
            &model_reg,
            cfg,
            &opts.bare_model,
            prefs,
            project,
        )?;
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
    let mut adopt: Option<hc::WorkspaceInfo> = None;
    if !orphans.is_empty() && opts.retire_orphans {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
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
    } else if !orphans.is_empty() {
        let detail: Vec<String> = orphans
            .iter()
            .map(|ws| {
                format!(
                    "{} (label {:?}, {} panes)",
                    ws.workspace_id, ws.label, ws.pane_count
                )
            })
            .collect();
        let live_agents = backend.agent_list().unwrap_or_default();
        let live_panes: std::collections::HashSet<String> = backend
            .pane_list(None)
            .map(|panes| panes.into_iter().map(|p| p.pane_id).collect())
            .unwrap_or_default();
        match crate::classify_spawn_workspace(&orphans, &live_agents, &live_panes) {
            crate::SpawnWorkspace::Fresh => {}
            crate::SpawnWorkspace::Add { index } => {
                let existing = orphans[index].clone();
                eprintln!(
                    "flywheel: {project}: workspace {} hosts no crew pins — adding panes into it",
                    existing.workspace_id
                );
                adopt = Some(existing);
            }
            crate::SpawnWorkspace::Join => {
                if opts.join {
                    eprintln!(
                        "{}",
                        spawn_join_notice(
                            project,
                            &detail.join(", "),
                            &requested_spawn_flags(&opts)
                        )
                    );
                    let pin =
                        crate::authority::select_agent_identity(None, &|k| std::env::var(k).ok());
                    return cmd_arrive(cfg, project, pin.as_deref(), None, None, None);
                }
                bail!(
                    "found {} pre-existing workspace(s) for {project} — stale panes from a killed swarm would race the new one: {}. Close them first (flywheel kill {project}), pass --join to attach, or --retire-orphans to close them automatically.",
                    orphans.len(),
                    detail.join(", ")
                );
            }
        }
    }
    let spawn_effect = match request_key {
        Some(_) => agent_ref::AgentEffectGuard::begin_with(
            agent_ref::AgentEffect::prepare(project, &label, agent_ref::AgentPhase::Spawn)
                .with_plan_digest(plan.clone()),
        )?,
        None => agent_ref::AgentEffectGuard::begin(project, &label, agent_ref::AgentPhase::Spawn)?,
    };
    let adopted = adopt.is_some();
    let ws = match adopt {
        Some(existing) => {
            println!("workspace {} reused (no crew pins)", existing.workspace_id);
            existing
        }
        None => {
            let ws = backend.workspace_create(Some(&label))?;
            println!(
                "{}",
                if opts.no_focus {
                    format!("workspace {} created (background)", ws.workspace_id)
                } else {
                    format!("workspace {} created", ws.workspace_id)
                }
            );
            ws
        }
    };

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
    // An adopted workspace's blocks keep their env: split a fresh one for the
    // first agent so its env-carried model pin still lands.
    let mut first = !adopted;
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
        let kind = crate::prefer_kind(cfg, item, &catalog, &model_reg, prefs, project)?;
        let kind = overrides
            .get(item.short.as_str())
            .and_then(|o| o.harness.clone())
            .unwrap_or(kind);
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
                    let (mut shadow, _, _) = prefer_shadow_profile(prefs, item, p, project);
                    patch_shadow_profile(&overrides, item.short.as_str(), &mut shadow);
                    let mut plan = profile_launch_plan(&kind, &shadow, None)?;
                    plan.args
                        .extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
                    // bd-t2z0b: say which verified tool build this seat gets.
                    if let Some(v) = &plan.version {
                        eprintln!("flywheel: {name}: {kind} pinned to v{v} (mise.toml)");
                    }
                    let model_pin = match crate::models::ModelChoice::parse(&plan.model) {
                        crate::models::ModelChoice::HarnessDefault => None,
                        crate::models::ModelChoice::Model(_) => Some(plan.model),
                    };
                    (plan.args, model_pin, plan.env_model_pin)
                }
                None => {
                    let ask = prefer_bare_inputs(
                        prefs,
                        item,
                        project,
                        &opts.bare_model,
                        &opts.bare_provider,
                    );
                    let (eff_model, eff_provider, eff_effort) =
                        patched_bare_launch(&overrides, item.short.as_str(), ask);
                    let (mut args, env_pin) =
                        bare_model_launch(&kind, &eff_model, &eff_provider, eff_effort.as_deref())?;
                    args.extend(patched_mode_argv(&overrides, item.short.as_str(), &kind));
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
            let harness_session = mint_harness_session(&kind, &mut agent_args);
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
            entry.effort = item
                .profile
                .as_deref()
                .and_then(|pname| catalog.get(pname))
                .map(|p| p.effort.clone())
                .filter(|e| !e.is_empty());
            entry.mode = None;
            entry.skills_digest = item
                .profile
                .as_deref()
                .and_then(|pname| catalog.get(pname))
                .and_then(|p| profile::skills_digest(&p.skills));
            entry.harness_session =
                harness_session.or_else(|| captain_directory::capture_harness_session(&kind));
            registry.push(with_recorded_harness(entry, &kind));
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
            "workspace_id": ws.workspace_id, "label": label, "agents": spawned,
            "plan_id": plan_record.id
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
        doorbell::ensure_running(
            &exe,
            &config::state_dir(),
            dispatcher_workspace(&ws, &label),
            project,
        )
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

    /// bd-edva3: spawn never reads the spawner's environment for an agy
    /// conversation id — the seat's PreInvocation hook reports it later.
    #[test]
    fn agy_conversation_id_never_read_from_spawner_env() {
        let _env = lock_env();
        unsafe { std::env::set_var("ANTIGRAVITY_CONVERSATION_ID", "captains-own-conversation") };
        let mut args = vec!["--agent".to_string(), "build".to_string()];
        assert_eq!(mint_harness_session("agy", &mut args), None);
        assert_eq!(args, vec!["--agent", "build"]);
    }

    /// bd-f888z: opencode seats get a unique `ses-` session minted into argv,
    /// recorded on the entry; other harnesses are untouched.
    #[test]
    fn opencode_session_minted_into_argv_and_recorded() {
        let mut args = vec!["--agent".to_string(), "build".to_string()];
        let sid = mint_harness_session("opencode", &mut args).expect("opencode session");
        assert!(sid.starts_with("ses-"), "{sid}");
        assert_eq!(args, vec!["--agent", "build", "--session", sid.as_str()]);
        let second = mint_harness_session("oc", &mut args).expect("alias session");
        assert_ne!(sid, second, "each seat gets a unique session");

        let mut pi_args = vec!["--model".to_string(), "x".to_string()];
        assert_eq!(mint_harness_session("pi", &mut pi_args), None);
        assert_eq!(pi_args, vec!["--model", "x"], "non-opencode argv untouched");
    }

    /// bd-4n2bn: omp seats ride the pi-style agentDir bridge (same env the
    /// fork honors); every other kind keeps the doorbell fallback.
    #[test]
    fn omp_seats_take_the_pi_style_events_bridge() {
        assert!(super::pi_style_events_kind("pi"));
        assert!(super::pi_style_events_kind("omp"));
        for kind in ["opencode", "oc", "claude", "codex", "grok", ""] {
            assert!(
                !super::pi_style_events_kind(kind),
                "{kind} keeps its own path"
            );
        }
    }

    struct FakeMint {
        calls: std::cell::Cell<usize>,
    }
    impl crate::events_tokens::TokenCli for FakeMint {
        fn mint(&self, _pin: &str, _project: &str, _ttl: u64) -> Result<String, String> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            Ok(format!("secret-token-{n}"))
        }
        fn revoke(&self, _hash: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// bd-rrlo2: only harnesses with an events door get a token minted —
    /// config-file bridges plus the pump's no-door seats.
    #[test]
    fn only_events_door_harnesses_get_a_token() {
        for kind in [
            "opencode",
            "oc",
            "pi",
            "omp",
            "gemini",
            "antigravity",
            "grok",
            "codex",
        ] {
            assert!(super::has_events_door(kind), "{kind} has an events door");
        }
        for kind in ["claude", "codex-unknown", "", "Muse"] {
            assert!(
                !super::has_events_door(kind),
                "{kind} keeps the doorbell fallback"
            );
        }
    }

    /// bd-rrlo2: spawning a pi, opencode and gemini seat mints a 0600 token
    /// file plus a journal row without plaintext; a second pass reuses the
    /// file, and an expired token rotates. A skipped mint leaves nothing.
    #[test]
    fn spawn_mints_a_per_pin_token_before_launch() {
        let _home = crate::config::test_state_home();
        let state = crate::config::state_dir();
        let cli = FakeMint {
            calls: std::cell::Cell::new(0),
        };
        let now_us = 5_000_000i64;
        // Through the same gate the spawn loop uses: doors get tokens,
        // other harnesses keep the doorbell fallback and no file.
        for (kind, pin) in [
            ("pi", "PiSeat"),
            ("opencode", "OpencodeSeat"),
            ("gemini", "GemSeat"),
            ("claude", "ClaudeSeat"),
        ] {
            if !super::has_events_door(kind) {
                continue;
            }
            let outcome =
                super::ensure_seat_events_token(&cli, &state, "demo", pin, now_us).expect("mint");
            assert_eq!(outcome, super::SeatTokenOutcome::Minted, "{pin}");
            let token_path = crate::events_tokens::token_path(&state, "demo", pin);
            assert!(token_path.is_file(), "{pin} token file exists");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = std::fs::metadata(&token_path)
                    .expect("meta")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "the token file must be 0600: {mode:o}");
            }
        }
        let journal =
            std::fs::read_to_string(state.join("events-token-journal.jsonl")).expect("journal");
        assert!(
            !journal.contains("secret-token"),
            "the journal carries hashes, never plaintext"
        );
        assert!(
            journal.contains("\"op\":\"minted\""),
            "every mint is journaled: {journal}"
        );
        // A second pass reuses the file: no re-mint.
        let outcome = super::ensure_seat_events_token(&cli, &state, "demo", "PiSeat", now_us + 1)
            .expect("reuse");
        assert_eq!(outcome, super::SeatTokenOutcome::Current);
        assert_eq!(cli.calls.get(), 3, "no re-mint while current");
        // An expired token rotates: new file content, revoke covered by
        // the rotate unit tests.
        let late = now_us + (crate::events_tokens::PIN_TTL_SECS as i64 + 10) * 1_000_000;
        let outcome =
            super::ensure_seat_events_token(&cli, &state, "demo", "PiSeat", late).expect("rotate");
        assert_eq!(outcome, super::SeatTokenOutcome::Rotated);
        // The pump sees the minted seats under the default config.
        let seats = [
            ("pi", "PiSeat"),
            ("opencode", "OpencodeSeat"),
            ("gemini", "GemSeat"),
            ("claude", "ClaudeSeat"),
        ]
        .into_iter()
        .map(|(harness, pin)| {
            let token = crate::events_tokens::token_path(&state, "demo", pin).is_file();
            (harness.to_string(), Some(pin.to_string()), token)
        })
        .collect::<Vec<_>>();
        let enabled = crate::config::Config::default().doorbell_toron_events_in(&state);
        assert!(enabled, "minted tokens switch the default pump on");
        assert!(
            !crate::events_tokens::token_path(&state, "demo", "ClaudeSeat").is_file(),
            "no door, no token file"
        );
        assert_eq!(
            crate::events_source::seats_to_pump(enabled, &seats),
            vec!["GemSeat".to_string()],
            "the pump covers the no-door seat once its token exists (pi/opencode ride their own config)"
        );
    }

    /// bd-u7m1y: re-spawning one seat must never revoke another seat's
    /// token. Mint B then A (A's row is the global last), expire both,
    /// re-ensure B: only B rotates — A's file and hash are untouched.
    #[test]
    fn respawning_one_seat_never_revokes_another_seat_token() {
        let _home = crate::config::test_state_home();
        let state = crate::config::state_dir();
        let cli = FakeMint {
            calls: std::cell::Cell::new(0),
        };
        let t0 = 5_000_000i64;
        let late = t0 + (crate::events_tokens::PIN_TTL_SECS as i64 + 10) * 1_000_000;
        for pin in ["SeatB", "SeatA"] {
            let outcome =
                super::ensure_seat_events_token(&cli, &state, "demo", pin, t0).expect("mint");
            assert_eq!(outcome, super::SeatTokenOutcome::Minted, "{pin}");
        }
        let file_a0 =
            std::fs::read_to_string(crate::events_tokens::token_path(&state, "demo", "SeatA"))
                .expect("A token file");
        let hash_a = crate::events_tokens::token_hash(&file_a0);
        // Re-spawn B with both tokens expired: only B may rotate.
        let outcome =
            super::ensure_seat_events_token(&cli, &state, "demo", "SeatB", late).expect("re");
        assert_eq!(outcome, super::SeatTokenOutcome::Rotated);
        assert_eq!(
            std::fs::read_to_string(crate::events_tokens::token_path(&state, "demo", "SeatA"))
                .expect("A token file"),
            file_a0,
            "A keeps its plaintext"
        );
        let journal =
            std::fs::read_to_string(state.join("events-token-journal.jsonl")).expect("journal");
        assert!(
            !journal
                .lines()
                .any(|l| l.contains("revok") && l.contains(&hash_a)),
            "A's hash is never revoked"
        );
        let file_b =
            std::fs::read_to_string(crate::events_tokens::token_path(&state, "demo", "SeatB"))
                .expect("B token file");
        assert_ne!(file_b, file_a0, "B got a fresh token");
    }

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

    /// bd-t2z0b: an omp spawn plan with no version names the pinned pi-fork
    /// build (18.2.5), and the repo's `mise.toml` pins that same build so the
    /// shim resolves it for the live seat. A drift between the two is a bug:
    /// the seat would launch a build whose PI_CODING_AGENT_DIR support was
    /// never verified.
    #[test]
    fn omp_with_no_version_uses_the_pinned_default() {
        let _g = lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let plan =
            profile_launch_plan("omp", &profile("omp", "zai/glm-5.3"), None).expect("omp plan");
        assert_eq!(
            plan.version.as_deref(),
            Some(crate::harness_registry::OMP_DEFAULT_VERSION),
            "a bare omp spawn must record the pinned default version"
        );
        let mise = std::fs::read_to_string(format!("{}/mise.toml", env!("CARGO_MANIFEST_DIR")))
            .expect("repo mise.toml");
        assert!(
            mise.contains(&format!(
                "github:can1357/oh-my-pi\" = \"{}\"",
                crate::harness_registry::OMP_DEFAULT_VERSION
            )),
            "mise.toml must pin the same omp build OMP_DEFAULT_VERSION names"
        );
        // Other harnesses carry no tool-version pin.
        let pi = profile_launch_plan("pi", &profile("pi", "zai/glm-5.3"), None).expect("pi plan");
        assert!(
            pi.version.is_none(),
            "no guessed version for an unpinned harness"
        );
    }

    /// bd-omp-pin: a seat launched from ANY project dir names the pinned omp
    /// build explicitly, so mise never falls back to a cwd-less PATH omp.
    #[test]
    fn omp_pin_survives_a_foreign_project_cwd() {
        let binary = crate::harness_launch_binary("omp");
        assert!(
            binary.contains(&format!(
                "github:can1357/oh-my-pi@{}",
                crate::harness_registry::OMP_DEFAULT_VERSION
            )),
            "the launch names the verified build, not a bare `omp`: {binary}"
        );
        let cmd = crate::pane::rex_env_command(&Default::default(), "omp", &["--flag".into()]);
        assert!(
            cmd.contains(crate::harness_registry::OMP_DEFAULT_VERSION),
            "the rex launch carries the pin too: {cmd}"
        );
        // Other harnesses keep their plain binary name.
        assert_eq!(crate::harness_launch_binary("pi"), "pi");
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
        let (args, pin) =
            bare_model_launch("pi", &model_ask("glm-5.2"), "", None).expect("bare id");
        assert_eq!(
            args,
            vec!["--model", "aaa/glm-5.2"],
            "registered bare id routes"
        );
        assert_eq!(pin, None, "pi carries no env pin");
        let (args, _) =
            bare_model_launch("pi", &model_ask("glm-5.2"), "zzz", None).expect("pinned");
        assert_eq!(
            args,
            vec!["--model", "zzz/glm-5.2"],
            "explicit provider wins"
        );
        let (args, _) = bare_model_launch("pi", &model_ask("ghost-model"), "", None)
            .expect("lenient passthrough");
        assert_eq!(args, vec!["--model", "ghost-model"]);
        let (args, _) = bare_model_launch("pi", &model_ask("ghost-model"), "qqq", None)
            .expect("pair passthrough");
        assert_eq!(args, vec!["--model", "qqq/ghost-model"]);
        assert_eq!(
            bare_model_launch("pi", &model_ask(""), "", None)
                .expect("empty model")
                .0,
            Vec::<String>::new(),
            "bd-iu00a: bare pi without --model uses the harness own model"
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
        let (args, env_pin) = bare_model_launch("opencode", &model_ask(""), "", None)
            .expect("opencode cheap implied");
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
            None,
            "bd-iu00a: the implied cheap slot is now harness-default, so no env pin"
        );
        let (args, env_pin) = bare_model_launch(
            "opencode",
            &model_ask("muse-spark-1.3-contributor"),
            "opencode-go",
            None,
        )
        .expect("explicit bare pair");
        assert_eq!(args, [] as [String; 0], "still no argv: {args:?}");
        assert_eq!(
            env_pin.as_deref(),
            Some("opencode-go/muse-spark-1.3-contributor")
        );
        let (pi_args, pi_env) =
            bare_model_launch("pi", &model_ask(""), "", None).expect("pi harness-default");
        assert_eq!(
            pi_args,
            Vec::<String>::new(),
            "bd-iu00a: pi's IMPLIED standard slot is now harness-default — no argv"
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
        let (args, env_pin) =
            bare_model_launch("opencode", &model_ask(""), "", None).expect("cheap implied");
        assert!(
            args.is_empty() && env_pin.is_none(),
            "bd-iu00a: opencode spawn without --model injects nothing: {args:?} {env_pin:?}"
        );
        let (args, env_pin) =
            bare_model_launch("pi", &model_ask(""), "", None).expect("standard implied");
        assert_eq!(
            args,
            Vec::<String>::new(),
            "bd-iu00a: pi spawn without --model injects no model: {args:?}"
        );
        assert_eq!(env_pin, None, "pi rides argv");
        let err = bare_model_launch("grok", &model_ask(""), "", None).expect_err("closed premium");
        assert!(
            err.to_string().contains("grok"),
            "the refusal names the harness that cannot be routed: {err}"
        );
        assert_eq!(
            bare_model_launch("kind-without-slot", &model_ask(""), "", None)
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
        let err = bare_model_launch("grok", &model_ask(""), "", None)
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
        let (args, pin) = bare_model_launch("grok", &model_ask(""), "", None).expect("grok routed");
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
        let rows = spawn_model_rows(
            &items,
            &catalog,
            &reg,
            &cfg,
            "",
            &crate::preferences::PreferenceSet::new(),
            "test",
        )
        .expect("rows");
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
        let rows = spawn_model_rows(
            &items,
            &catalog,
            &reg,
            &cfg,
            "",
            &crate::preferences::PreferenceSet::new(),
            "test",
        )
        .expect("rows");
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
        PrimeChoice, SpawnWorkspace, WorkspaceOrigin, add_target_from_split,
        classify_spawn_workspace, classify_workspace_origin, partition_workspaces, prime_choice,
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

    /// bd-gjtcw: a shell-only owned workspace is not a stale swarm — spawn adds
    /// panes into it instead of refusing and then no-op joining.
    #[test]
    fn empty_workspace_classifies_as_add() {
        let owned = vec![ws("w-empty", "flywheel-demo", 1)];
        assert_eq!(
            classify_spawn_workspace(&owned, &[], &std::collections::HashSet::new()),
            SpawnWorkspace::Add { index: 0 },
            "no agent in the workspace means nothing can race the new panes"
        );
        assert_eq!(
            classify_spawn_workspace(&[], &[], &std::collections::HashSet::new()),
            SpawnWorkspace::Fresh,
            "no owned workspace means a fresh one"
        );
    }

    /// bd-89q4r: a labelled agent whose pane is gone is an orphan, not a
    /// fleet. Only an agent on a pane still in the plane's pane_list counts.
    #[test]
    fn labelled_agent_without_a_live_pane_does_not_join() {
        let owned = vec![ws("w-orphan", "flywheel-demo", 1)];
        let agents = vec![agent("flywheel-demo-oc-1", "w-orphan")];
        assert_eq!(
            classify_spawn_workspace(&owned, &agents, &std::collections::HashSet::new()),
            SpawnWorkspace::Add { index: 0 },
            "a stale terminal label must not force --retire-orphans on the next spawn"
        );
        let live = std::collections::HashSet::from(["w-orphan:p1".to_string()]);
        assert_eq!(
            classify_spawn_workspace(&owned, &agents, &live),
            SpawnWorkspace::Join,
            "the same label with a live pane is still the fleet to join"
        );
    }

    /// The multiplexer's own shell is not a crew pin: a shell-only workspace
    /// with a live shell pane still takes the Add path (bd-89q4r).
    #[test]
    fn shell_only_workspace_classifies_as_add_with_a_live_pane() {
        let owned = vec![ws("w-shell", "flywheel-demo", 1)];
        let shell = agent("shell", "w-shell");
        let live = std::collections::HashSet::from(["w-shell:p1".to_string()]);
        assert_eq!(
            classify_spawn_workspace(&owned, &[shell], &live),
            SpawnWorkspace::Add { index: 0 },
            "the shell is not the fleet"
        );
    }

    /// A pin that survived the dead-pane probe IS the fleet: join stays loud.
    #[test]
    fn live_pin_classifies_as_join() {
        let owned = vec![ws("w-live", "flywheel-demo", 2)];
        let agents = vec![agent("flywheel-demo-oc-1", "w-live")];
        let live = std::collections::HashSet::from(["w-live:p1".to_string()]);
        assert_eq!(
            classify_spawn_workspace(&owned, &agents, &live),
            SpawnWorkspace::Join
        );
        // A workspace with no agent alongside a live one still joins.
        let owned = vec![
            ws("w-empty", "flywheel-demo", 1),
            ws("w-live", "flywheel-demo", 2),
        ];
        assert_eq!(
            classify_spawn_workspace(&owned, &agents, &live),
            SpawnWorkspace::Join,
            "one live pin makes the project a fleet"
        );
    }
}

#[cfg(test)]
mod adr0013_spawn_effect_tests {
    use super::{build_dry_run_plan, spawn_items_from_plan};
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
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "m",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
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
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "old/r",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
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
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
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
            spawn.contains("profile_launch_plan(&kind, "),
            "cmd_spawn must resolve argv + pin in ONE call (P1-7)"
        );
        assert!(
            spawn.contains("prefer_shadow_profile(&prefs, item, p, project)"),
            "cmd_spawn must route the effective (preference-filled) profile through that one call"
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
        start_agent_with_retry_dyn_profile, start_agent_with_taken_name_clear, taken_name_action,
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
        let agent = start_agent_with_retry_dyn_profile(
            &mux,
            "flywheel-demo-oc",
            "opencode",
            "wA:p1",
            &[],
            LaunchCtx::default(),
        )
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
        let err = start_agent_with_retry_dyn_profile(
            &mux,
            "flywheel-demo-oc",
            "opencode",
            "wA:p1",
            &[],
            LaunchCtx::default(),
        )
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
        let out = cmd_add(&cfg, "typo-probe", super::AddOpts::default());
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
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "totally-unknown-9z",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
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
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        assert!(
            plan.needs_consent.is_empty(),
            "no ask, no consent: {:?}",
            plan.needs_consent
        );
    }
}

#[cfg(test)]
mod prefer_spawn_tests {
    use std::collections::BTreeMap;

    fn test_registry() -> crate::models::ModelRegistry {
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
        crate::models::ModelRegistry {
            models,
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        }
    }

    fn test_prefs(rows: &[(&str, &str, Option<&str>)]) -> crate::preferences::PreferenceSet {
        let mut prefs = crate::preferences::PreferenceSet::new();
        for (key, value, project) in rows {
            let scope = match project {
                Some(p) => crate::preferences::Scope::Project(p.to_string()),
                None => crate::preferences::Scope::Always,
            };
            prefs.set(
                key.parse().expect("pref key parses"),
                value.to_string(),
                scope,
            );
        }
        prefs
    }

    fn bare_pi() -> Vec<crate::SpawnItem> {
        super::spawn_items_from_plan(vec![("pi".to_string(), 1)], None)
    }

    /// A preference model with no `--model` gives plan `from == preference`
    /// AND the launch primitive carries that same model (one resolution).
    #[test]
    fn prefer_spawn_model_from_preference() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[("pi.model", "a/m", None)]);
        let items = bare_pi();
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "a/m");
        // Acceptance 4: the remembered preference IS the answer, so the next
        // session must not ask again — a Preference field is never a consent
        // item (only Jev, blocked, and over-budget/cap rows are).
        assert!(
            !plan
                .needs_consent
                .iter()
                .any(|c| c.seat == plan.seats[0].role && c.field == "model"),
            "a remembered preference suppresses the question: {:?}",
            plan.needs_consent
        );
        let ask = super::prefer_bare_inputs(&prefs, &items[0], "probe", "", "");
        let (m, from) = ask.model.clone().expect("preference resolves");
        assert_eq!(
            (m.as_ask(), from),
            ("a/m", crate::swarm_plan::Provenance::Preference)
        );
        let kind = crate::prefer_kind(&cfg, &items[0], &catalog, &reg, &prefs, "probe")
            .expect("kind resolves");
        assert_eq!(kind, plan.seats[0].harness);
        let (eff_model, eff_provider) = ask
            .model
            .map(|(m, _)| (m, ask.provider))
            .unwrap_or_else(|| (crate::models::ModelChoice::parse(""), String::new()));
        let (args, env) = crate::bare_model_launch(
            &kind,
            &eff_model,
            &eff_provider,
            ask.effort.as_ref().map(|(e, _)| e.as_str()),
        )
        .expect("launch builds");
        let carries =
            args.iter().any(|a| a == &f.value) || env.as_deref() == Some(f.value.as_str());
        let honoured = matches!(f.status, crate::swarm_plan::FieldStatus::Honored);
        assert_eq!(
            honoured, carries,
            "plan delivery must equal launch carriage: {f:?} vs ({args:?}, {env:?})"
        );
    }

    /// The profile launch resolves the same model the plan computed.
    #[test]
    fn prefer_spawn_launch_matches_plan_profile() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let mut catalog = crate::profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".into(),
                model: "b/m".into(),
                ..Default::default()
            },
        );
        let prefs = test_prefs(&[("coder.model", "a/m", None)]);
        let items = vec![crate::SpawnItem {
            short: "coder".into(),
            count: 1,
            kind: None,
            profile: Some("coder".into()),
        }];
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "a/m");
        let p = catalog.get("coder").expect("profile resolves");
        let (shadow, model_ask, _) = super::prefer_shadow_profile(&prefs, &items[0], p, "probe");
        assert_eq!(
            model_ask,
            Some(("a/m".to_string(), crate::swarm_plan::Provenance::Preference))
        );
        let kind = crate::prefer_kind(&cfg, &items[0], &catalog, &reg, &prefs, "probe")
            .expect("kind resolves");
        assert_eq!(kind, plan.seats[0].harness);
        let launched = crate::profile_launch_plan(&kind, &shadow, None).expect("launch builds");
        assert_eq!(
            launched.model, f.value,
            "launch must resolve the planned model"
        );
    }

    /// A user `--model` flows through the same resolution into plan and launch.
    #[test]
    fn prefer_spawn_launch_matches_plan_user_override() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[("pi.model", "a/m", None)]);
        let items = bare_pi();
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "b/m",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::User);
        assert_eq!(f.value, "b/m");
        let ask = super::prefer_bare_inputs(&prefs, &items[0], "probe", "b/m", "");
        let (m, from) = ask.model.clone().expect("user ask resolves");
        assert_eq!(
            (m.as_ask(), from),
            ("b/m", crate::swarm_plan::Provenance::User)
        );
        let kind = crate::prefer_kind(&cfg, &items[0], &catalog, &reg, &prefs, "probe")
            .expect("kind resolves");
        assert_eq!(kind, plan.seats[0].harness);
        let (eff_model, eff_provider) = ask
            .model
            .map(|(m, _)| (m, ask.provider))
            .unwrap_or_else(|| (crate::models::ModelChoice::parse(""), String::new()));
        let (args, env) = crate::bare_model_launch(
            &kind,
            &eff_model,
            &eff_provider,
            ask.effort.as_ref().map(|(e, _)| e.as_str()),
        )
        .expect("launch builds");
        assert!(
            args.iter().any(|a| a == &f.value) || env.as_deref() == Some(f.value.as_str()),
            "launch must carry the planned model"
        );
    }

    /// A user `--model` beats the preference (`from: user`).
    #[test]
    fn prefer_spawn_user_model_wins() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[("pi.model", "a/m", None)]);
        let items = bare_pi();
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "b/m",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::User);
        assert_eq!(f.value, "b/m");
    }

    /// The preference beats the profile model.
    #[test]
    fn prefer_spawn_beats_profile() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let mut catalog = crate::profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".into(),
                model: "b/m".into(),
                ..Default::default()
            },
        );
        let prefs = test_prefs(&[("coder.model", "a/m", None)]);
        let items = vec![crate::SpawnItem {
            short: "coder".into(),
            count: 1,
            kind: None,
            profile: Some("coder".into()),
        }];
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "a/m");
    }

    /// A project preference beats an always preference for that project only.
    #[test]
    fn prefer_spawn_project_beats_always() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[
            ("pi.model", "a/m", None),
            ("pi.model", "b/m", Some("probe")),
        ]);
        let items = bare_pi();
        let scoped = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        assert_eq!(
            scoped.seats[0].model.as_ref().expect("model field").value,
            "b/m"
        );
        let other = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "other",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = other.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "a/m");
    }

    /// `*` applies when no exact role is set.
    #[test]
    fn prefer_spawn_wildcard_applies() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[("*.model", "a/m", None)]);
        let items = bare_pi();
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "a/m");
    }

    /// An unregistered preferred model gets the consent reason `unregistered`,
    /// the same as a user ask.
    #[test]
    fn prefer_spawn_unregistered_model_consent() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = test_prefs(&[("pi.model", "ghost-9z", None)]);
        let items = bare_pi();
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let item = plan
            .needs_consent
            .iter()
            .find(|i| i.seat == "pi" && i.field == "model")
            .expect("unregistered preference needs consent");
        assert_eq!(item.reason, crate::swarm_plan::ConsentReason::Unregistered);
    }

    /// A preferred effort lands on the seat with `from: preference`.
    #[test]
    fn prefer_spawn_effort_from_preference() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let mut catalog = crate::profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".into(),
                model: "b/m".into(),
                ..Default::default()
            },
        );
        let prefs = test_prefs(&[("coder.effort", "slow", None)]);
        let items = vec![crate::SpawnItem {
            short: "coder".into(),
            count: 1,
            kind: None,
            profile: Some("coder".into()),
        }];
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].effort.as_ref().expect("effort field");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(f.value, "slow");
    }

    /// A preferred harness applies to a profile item; a bare item ignores it.
    #[test]
    fn prefer_spawn_harness_for_profile() {
        let reg = test_registry();
        let cfg = crate::config::Config::default();
        let mut catalog = crate::profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".into(),
                model: "b/m".into(),
                ..Default::default()
            },
        );
        let prefs = test_prefs(&[
            ("coder.harness", "opencode", None),
            ("pi.harness", "opencode", None),
        ]);
        let items = vec![crate::SpawnItem {
            short: "coder".into(),
            count: 1,
            kind: None,
            profile: Some("coder".into()),
        }];
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        assert_eq!(plan.seats[0].harness, "opencode");
        let bare = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &bare_pi(),
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        assert_ne!(
            bare.seats[0].harness, "opencode",
            "harness preference is profile-only"
        );
    }

    /// A corrupt preferences file fails loud, naming the path.
    #[test]
    fn prefer_spawn_corrupt_file_fails_loud() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("preferences.toml");
        std::fs::write(&path, "[unclosed").expect("write corrupt file");
        let err = super::load_spawn_preferences(&path).expect_err("corrupt file must fail");
        let msg = format!("{err:?}");
        assert!(
            msg.contains(&path.display().to_string()),
            "error names the path: {msg}"
        );
    }
}

#[cfg(test)]
mod plan_gate_tests {
    use crate::swarm_plan::{
        ConsentItem, ConsentReason, Field, FieldStatus, Plan, Provenance, Seat,
    };

    fn field(value: &str, from: Provenance, status: FieldStatus) -> Field {
        Field {
            value: value.to_string(),
            from,
            status,
            registry: None,
            was: None,
        }
    }

    fn seat_with_model(role: &str, harness: &str, model: Option<Field>) -> Seat {
        Seat {
            role: role.to_string(),
            count: 1,
            harness: harness.to_string(),
            model,
            effort: None,
            mode: None,
            phase: None,
            placement: "pane".to_string(),
        }
    }

    fn consent(seat: &str, field: &str, reason: ConsentReason) -> ConsentItem {
        ConsentItem {
            seat: seat.to_string(),
            field: field.to_string(),
            reason,
        }
    }

    fn jev_plan() -> Plan {
        let mut plan = Plan::new("swarm");
        plan.seats.push(seat_with_model(
            "coder",
            "pi",
            Some(field("a/m", Provenance::Jev, FieldStatus::Honored)),
        ));
        plan.needs_consent = vec![consent("coder", "model", ConsentReason::Jev)];
        plan
    }

    /// An empty consent set launches.
    #[test]
    fn plan_gate_empty_consent_launches() {
        let plan = Plan::new("swarm");
        assert!(matches!(
            super::plan_gate(&plan, false),
            super::PlanGate::Launch
        ));
        assert!(matches!(
            super::plan_gate(&plan, true),
            super::PlanGate::Launch
        ));
    }

    /// A Jev item without `--yes` asks.
    #[test]
    fn plan_gate_jev_without_yes_asks() {
        let plan = jev_plan();
        match super::plan_gate(&plan, false) {
            super::PlanGate::Ask { consent, .. } => {
                assert!(
                    consent
                        .iter()
                        .any(|l| l.contains("coder") && l.contains("model"))
                );
            }
            super::PlanGate::Launch => panic!("Jev ask without --yes must ask"),
        }
    }

    /// A Jev item with `--yes` launches.
    #[test]
    fn plan_gate_jev_with_yes_launches() {
        let plan = jev_plan();
        assert!(matches!(
            super::plan_gate(&plan, true),
            super::PlanGate::Launch
        ));
    }

    /// Unregistered plus `--yes` launches.
    #[test]
    fn plan_gate_unregistered_with_yes_launches() {
        let mut plan = jev_plan();
        plan.needs_consent = vec![consent("coder", "model", ConsentReason::Unregistered)];
        assert!(matches!(
            super::plan_gate(&plan, true),
            super::PlanGate::Launch
        ));
    }

    /// Blocked plus `--yes` asks, and the lines name the remedy.
    #[test]
    fn plan_gate_blocked_with_yes_asks_and_names_remedy() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(seat_with_model(
            "coder",
            "gemini",
            Some(field(
                "gemini-2.5-pro",
                Provenance::User,
                FieldStatus::Blocked {
                    remedy: "pick another harness".into(),
                },
            )),
        ));
        plan.needs_consent = vec![consent("coder", "model", ConsentReason::Blocked)];
        match super::plan_gate(&plan, true) {
            super::PlanGate::Ask { blocked, .. } => {
                assert!(
                    blocked.iter().any(|l| l.contains("pick another harness")),
                    "remedy named: {blocked:?}"
                );
            }
            super::PlanGate::Launch => panic!("blocked with --yes must still ask"),
        }
    }

    /// crew-max without `--yes` asks.
    #[test]
    fn plan_gate_crew_max_without_yes_asks() {
        let mut plan = Plan::new("swarm");
        plan.needs_consent = vec![consent("crew", "count", ConsentReason::CrewMax)];
        assert!(matches!(
            super::plan_gate(&plan, false),
            super::PlanGate::Ask { .. }
        ));
    }

    /// Ported: the ask names the seat, the field and the `--yes` hint, and
    /// never names `plan apply` (ADR-0022 D5: the verb is `flywheel swarm apply`).
    #[test]
    fn plan_gate_names_items_and_hint() {
        let plan = jev_plan();
        match super::plan_gate(&plan, false) {
            super::PlanGate::Ask { consent, .. } => {
                assert!(
                    consent
                        .iter()
                        .any(|l| l.contains("coder") && l.contains("model"))
                );
                assert!(consent.iter().any(|l| l.contains("--yes")));
                assert!(
                    !consent.iter().any(|l| l.contains("plan apply")),
                    "no live verb is called `plan apply`: {consent:?}"
                );
            }
            super::PlanGate::Launch => panic!("must ask without --yes"),
        }
    }

    /// Ported: every blocked seat field is named with its remedy.
    #[test]
    fn plan_gate_blocked_names_every_blocked_seat_field() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(seat_with_model(
            "coder",
            "gemini",
            Some(field(
                "gemini-2.5-pro",
                Provenance::User,
                FieldStatus::Blocked {
                    remedy: "pick another harness".into(),
                },
            )),
        ));
        plan.seats.push(seat_with_model(
            "reviewer",
            "pi",
            Some(field("x", Provenance::Slot, FieldStatus::Honored)),
        ));
        plan.needs_consent = vec![consent("coder", "model", ConsentReason::Blocked)];
        match super::plan_gate(&plan, true) {
            super::PlanGate::Ask { blocked, .. } => {
                assert!(
                    blocked
                        .iter()
                        .any(|l| l.contains("coder") && l.contains("model"))
                );
                assert!(blocked.iter().any(|l| l.contains("pick another harness")));
            }
            super::PlanGate::Launch => panic!("one blocked field refuses"),
        }
    }
}

#[cfg(test)]
mod plan_patch_spawn_tests {
    fn herm_registry() -> crate::models::ModelRegistry {
        let mut models = std::collections::BTreeMap::new();
        for key in ["a/m", "b/m", "gm9zprobe"] {
            models.insert(
                key.to_string(),
                crate::models::ModelEntry {
                    caps: vec![],
                    fallback: None,
                    ..Default::default()
                },
            );
        }
        crate::models::ModelRegistry {
            models,
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        }
    }

    fn jev_item() -> crate::swarm_plan::ConsentItem {
        crate::swarm_plan::ConsentItem {
            seat: "coder".to_string(),
            field: "model".to_string(),
            reason: crate::swarm_plan::ConsentReason::Jev,
        }
    }

    fn blocked_item() -> crate::swarm_plan::ConsentItem {
        crate::swarm_plan::ConsentItem {
            seat: "coder".to_string(),
            field: "model".to_string(),
            reason: crate::swarm_plan::ConsentReason::Blocked,
        }
    }

    fn drafting() -> toron_core::kernel::MachineState<
        crate::elm::plan_statechart::PlanState,
        crate::elm::plan_statechart::PlanContext,
    > {
        toron_core::kernel::MachineState::at(
            crate::elm::plan_statechart::PlanState::Drafting,
            crate::elm::plan_statechart::PlanContext {
                yes: false,
                needs_consent: vec![],
                jev_degraded: false,
            },
        )
    }

    /// The real chart walks Resolved -> Patch -> Drafting -> Resolve -> Confirmed.
    #[test]
    fn plan_patch_machine_path_reaches_confirmed() {
        let machine = crate::elm::plan_statechart::build_plan_machine();
        let out = machine.step_state(
            &drafting(),
            &crate::elm::plan_statechart::PlanEvent::Resolve {
                needs_consent: vec![jev_item()],
                jev: crate::elm::plan_statechart::JevOutcome::Ok,
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::AwaitingConsent)
        );
        let awaiting = toron_core::kernel::MachineState::at(
            crate::elm::plan_statechart::PlanState::AwaitingConsent,
            crate::elm::plan_statechart::PlanContext {
                yes: false,
                needs_consent: vec![jev_item()],
                jev_degraded: false,
            },
        );
        let out = machine.step_state(
            &awaiting,
            &crate::elm::plan_statechart::PlanEvent::Patch {
                fields: vec!["model".to_string()],
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::Drafting)
        );
        let out = machine.step_state(
            &drafting(),
            &crate::elm::plan_statechart::PlanEvent::Resolve {
                needs_consent: vec![],
                jev: crate::elm::plan_statechart::JevOutcome::Ok,
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::Confirmed)
        );
    }

    /// ...and a still-blocked second Resolve stays in AwaitingConsent.
    #[test]
    fn plan_patch_machine_path_stays_awaiting() {
        let machine = crate::elm::plan_statechart::build_plan_machine();
        let out = machine.step_state(
            &drafting(),
            &crate::elm::plan_statechart::PlanEvent::Resolve {
                needs_consent: vec![jev_item()],
                jev: crate::elm::plan_statechart::JevOutcome::Ok,
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::AwaitingConsent)
        );
        let awaiting = toron_core::kernel::MachineState::at(
            crate::elm::plan_statechart::PlanState::AwaitingConsent,
            crate::elm::plan_statechart::PlanContext {
                yes: false,
                needs_consent: vec![jev_item()],
                jev_degraded: false,
            },
        );
        let out = machine.step_state(
            &awaiting,
            &crate::elm::plan_statechart::PlanEvent::Patch {
                fields: vec!["model".to_string()],
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::Drafting)
        );
        let gated = toron_core::kernel::MachineState::at(
            crate::elm::plan_statechart::PlanState::Drafting,
            crate::elm::plan_statechart::PlanContext {
                yes: true,
                needs_consent: vec![],
                jev_degraded: false,
            },
        );
        let out = machine.step_state(
            &gated,
            &crate::elm::plan_statechart::PlanEvent::Resolve {
                needs_consent: vec![blocked_item()],
                jev: crate::elm::plan_statechart::JevOutcome::Ok,
            },
        );
        assert_eq!(
            out.configuration.primary(),
            Some(&crate::elm::plan_statechart::PlanState::AwaitingConsent),
            "blocked stays blocked even with --yes"
        );
    }

    /// A patched plan through the gate launches when nothing asks: the
    /// codex base is blocked (codex carries no model argv), the pi-patched
    /// plan built from the same overrides launches.
    #[test]
    fn plan_patch_gated_after_apply() {
        let _g = crate::config::lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(vec![("codex".to_string(), 1)], None);
        let base = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        assert!(
            matches!(super::plan_gate(&base, false), super::PlanGate::Ask { .. }),
            "codex base is blocked"
        );
        let patches = vec![crate::swarm_plan::parse_patch("codex.harness=pi").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let patched = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        assert!(matches!(
            super::plan_gate(&patched, false),
            super::PlanGate::Launch
        ));
    }

    fn plan_probe_prefs(rows: &[(&str, &str, Option<&str>)]) -> crate::preferences::PreferenceSet {
        let mut prefs = crate::preferences::PreferenceSet::new();
        for (key, value, project) in rows {
            let scope = match project {
                Some(p) => crate::preferences::Scope::Project(p.to_string()),
                None => crate::preferences::Scope::Always,
            };
            prefs.set(
                key.parse().expect("pref key parses"),
                value.to_string(),
                scope,
            );
        }
        prefs
    }

    /// Unknown seats fail at the coverage gate naming the valid roles: the
    /// plan is built from the same overrides the gate checks.
    #[test]
    fn plan_patch_unknown_seat_names_valid_roles() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(
            vec![("pi".to_string(), 1), ("opencode".to_string(), 1)],
            None,
        );
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let err = crate::swarm_plan::validate_patch(
            &plan,
            &crate::swarm_plan::parse_patch("ghost.harness=pi").expect("parses"),
        )
        .expect_err("unknown seat fails");
        assert!(
            err.contains("pi") && err.contains("opencode"),
            "names valid seats: {err}"
        );
    }

    /// A harness patch re-filters the model through the real resolvers and
    /// clears that consent entry: the codex base is blocked, patched to pi
    /// the same user ask is honored and the gate launches.
    #[test]
    fn plan_patch_harness_refilters_model_and_clears_consent() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(vec![("codex".to_string(), 1)], None);
        let base = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let bm = base.seats[0].model.as_ref().expect("model field");
        assert_eq!(bm.value, "a/m", "re-filter never swaps the value");
        assert_eq!(
            bm.from,
            crate::swarm_plan::Provenance::User,
            "user-sourced stays user"
        );
        assert!(
            matches!(bm.status, crate::swarm_plan::FieldStatus::Blocked { .. }),
            "blocked on codex: {bm:?}"
        );
        let patches = vec![crate::swarm_plan::parse_patch("codex.harness=pi").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        assert_eq!(out.seats[0].harness, "pi");
        let m = out.seats[0].model.as_ref().expect("model field");
        assert_eq!(m.value, "a/m", "re-filter never swaps the value");
        assert_eq!(
            m.from,
            crate::swarm_plan::Provenance::User,
            "user-sourced stays user"
        );
        assert!(
            matches!(m.status, crate::swarm_plan::FieldStatus::Honored),
            "re-filtered on pi: {m:?}"
        );
        assert!(
            out.needs_consent.is_empty(),
            "that consent entry is gone: {:?}",
            out.needs_consent
        );
    }

    /// A model patch beats the standing preference and is recorded as the
    /// user's own ask: the base plan resolves from preference, the patched
    /// plan is from user with recomputed unregistered consent.
    #[test]
    fn plan_patch_model_beats_preference_as_user() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = plan_probe_prefs(&[("pi.model", "a/m", None)]);
        let items = super::spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let base = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let b = base.seats[0].model.as_ref().expect("model field");
        assert_eq!(b.from, crate::swarm_plan::Provenance::Preference);
        assert_eq!(b.value, "a/m");
        let patches = vec![crate::swarm_plan::parse_patch("pi.model=z").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        let m = out.seats[0].model.as_ref().expect("model field");
        assert_eq!(m.value, "z");
        assert_eq!(m.from, crate::swarm_plan::Provenance::User);
        assert_eq!(
            out.needs_consent.len(),
            1,
            "consent recomputed: {:?}",
            out.needs_consent
        );
        assert!(matches!(
            out.needs_consent[0].reason,
            crate::swarm_plan::ConsentReason::Unregistered
        ));
    }

    /// An invalid-for-harness model stays user-sourced and turns blocked:
    /// patching pi to codex keeps the ask, names codex in the remedy, and
    /// records blocked consent.
    #[test]
    fn plan_patch_invalid_stays_user_and_blocks() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let patches = vec![crate::swarm_plan::parse_patch("pi.harness=codex").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        assert_eq!(out.seats[0].harness, "codex");
        let m = out.seats[0].model.as_ref().expect("model field");
        assert_eq!(m.value, "a/m", "never silently swapped");
        assert_eq!(m.from, crate::swarm_plan::Provenance::User);
        match &m.status {
            crate::swarm_plan::FieldStatus::Blocked { remedy } => {
                assert!(
                    remedy.contains("codex"),
                    "remedy names the harness: {remedy}"
                );
            }
            other => panic!("expected blocked, got {other:?}"),
        }
        assert_eq!(out.needs_consent.len(), 1);
        assert!(matches!(
            out.needs_consent[0].reason,
            crate::swarm_plan::ConsentReason::Blocked
        ));
    }

    /// Other seats (and their consent) are untouched by a patch on the real
    /// path: the unpatched role keeps its value, provenance, and consent.
    #[test]
    fn plan_patch_leaves_other_seats_untouched() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(
            vec![("pi".to_string(), 1), ("opencode".to_string(), 1)],
            None,
        );
        let base = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "qz-9z",
                bare_provider: "",
                overrides: &std::collections::BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let patches = vec![crate::swarm_plan::parse_patch("pi.model=z").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "qz-9z",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        let m = out.seats[0].model.as_ref().expect("model field");
        assert_eq!(m.value, "z");
        assert_eq!(out.seats[1].harness, base.seats[1].harness);
        assert_eq!(out.seats[1].role, base.seats[1].role);
        let m2 = out.seats[1].model.as_ref().expect("opencode model");
        let b2 = base.seats[1].model.as_ref().expect("opencode model before");
        assert_eq!(m2.value, b2.value);
        assert_eq!(m2.from, b2.from);
        assert!(
            out.needs_consent.iter().any(|i| i.seat == "opencode"
                && matches!(i.reason, crate::swarm_plan::ConsentReason::Unregistered)),
            "opencode consent intact: {:?}",
            out.needs_consent
        );
    }

    /// Two patches apply in order through one plan build: the harness
    /// sticks and the later model wins, both from the user.
    #[test]
    fn plan_patch_two_patches_apply_in_order() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(vec![("codex".to_string(), 1)], None);
        let patches = vec![
            crate::swarm_plan::parse_patch("codex.harness=pi").expect("parses"),
            crate::swarm_plan::parse_patch("codex.model=z").expect("parses"),
        ];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "a/m",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        let m = out.seats[0].model.as_ref().expect("model field");
        assert_eq!(out.seats[0].harness, "pi", "first patch stuck");
        assert_eq!(m.value, "z", "second patch wins");
        assert_eq!(m.from, crate::swarm_plan::Provenance::User);
    }

    /// A role patch re-resolves EVERY seat of that role, not just the
    /// first: two pi seats both carry the patched model from the user.
    #[test]
    fn plan_patch_role_patches_every_seat() {
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(
            vec![
                ("pi".to_string(), 1),
                ("pi".to_string(), 1),
                ("opencode".to_string(), 1),
            ],
            None,
        );
        let patches = vec![crate::swarm_plan::parse_patch("pi.model=b/m").expect("parses")];
        let overrides = super::patch_role_overrides(&patches);
        let out = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("patched plan builds");
        let mut patched = 0;
        for s in out.seats.iter().filter(|s| s.role == "pi") {
            let m = s.model.as_ref().expect("model field");
            assert_eq!(m.value, "b/m", "every pi seat re-resolves");
            assert_eq!(m.from, crate::swarm_plan::Provenance::User);
            patched += 1;
        }
        assert_eq!(patched, 2, "both pi seats patched, not just the first");
        let r = out
            .seats
            .iter()
            .find(|s| s.role == "opencode")
            .expect("opencode seat");
        // bd-iu00a: a bare open seat now carries harness-default by default.
        // The patch must not leak into it as from:user — that is the real
        // 'untouched' contract.
        assert_eq!(
            r.model.as_ref().expect("model field").value,
            crate::models::HARNESS_DEFAULT,
            "other roles unchanged by the patch: {:?}",
            r.model
        );
        assert_eq!(
            r.model.as_ref().expect("model field").from,
            crate::swarm_plan::Provenance::HarnessDefault
        );
    }

    /// Rework defect 1: a model patch must reach the launch argv/env through
    /// the real builders, not just the gated plan. pi rides argv, opencode
    /// rides the env pin; the plan says from:user + honored for both.
    #[test]
    fn plan_patch_launch_argv_matches_plan() {
        let _g = crate::config::lock_env();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let cfg_home = dir.path().join("config");
        std::fs::create_dir_all(cfg_home.join("flywheel")).expect("config dir");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &cfg_home) };
        let reg = herm_registry();
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = super::spawn_items_from_plan(
            vec![
                ("pi".to_string(), 1),
                ("opencode".to_string(), 1),
                ("grok".to_string(), 1),
            ],
            None,
        );
        let pi_item = items.iter().find(|i| i.short == "pi").expect("pi item");
        let oc_item = items
            .iter()
            .find(|i| i.short == "opencode")
            .expect("opencode item");
        let gk_item = items.iter().find(|i| i.short == "grok").expect("grok item");
        let patches = vec![
            crate::swarm_plan::parse_patch("pi.model=b/m").expect("parses"),
            crate::swarm_plan::parse_patch("opencode.model=a/m").expect("parses"),
            crate::swarm_plan::parse_patch("grok.model=gm9zprobe").expect("parses"),
            crate::swarm_plan::parse_patch("grok.mode=read-only").expect("parses"),
            crate::swarm_plan::parse_patch("pi.mode=turbo").expect("parses"),
        ];
        let overrides = super::patch_role_overrides(&patches);
        let plan = super::build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "probe",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("plan builds");
        // grok uses a bare id: no provider to strip on either registry, so the
        // plan and the launch agree byte-for-byte.
        for (role, value) in [("pi", "b/m"), ("opencode", "a/m"), ("grok", "gm9zprobe")] {
            let seat = plan.seats.iter().find(|s| s.role == role).expect("seat");
            let m = seat.model.as_ref().expect("model field");
            assert_eq!(m.value, value, "patched value in plan");
            assert_eq!(m.from, crate::swarm_plan::Provenance::User);
            assert!(
                matches!(m.status, crate::swarm_plan::FieldStatus::Honored),
                "patched model honored, got {:?} for {role}",
                m.status
            );
        }
        // grok mode: read-only is honored and rides argv.
        let gk = plan
            .seats
            .iter()
            .find(|s| s.role == "grok")
            .expect("grok seat");
        let gk_mode = gk.mode.as_ref().expect("grok mode field");
        assert_eq!(gk_mode.value, "read-only");
        assert_eq!(gk_mode.from, crate::swarm_plan::Provenance::User);
        assert!(
            matches!(gk_mode.status, crate::swarm_plan::FieldStatus::Honored),
            "grok read-only mode honored, got {:?}",
            gk_mode.status
        );
        // pi mode: an undeployable mode is not honored and rides nothing.
        let pi = plan.seats.iter().find(|s| s.role == "pi").expect("pi seat");
        let pi_mode = pi.mode.as_ref().expect("pi mode field");
        assert_eq!(pi_mode.value, "turbo");
        assert!(
            !matches!(pi_mode.status, crate::swarm_plan::FieldStatus::Honored),
            "pi turbo mode must not be honored, got {:?}",
            pi_mode.status
        );
        // pi: the patched value rides argv through the real builder.
        let ask_pi = super::prefer_bare_inputs(&prefs, pi_item, "probe", "", "");
        let (model_pi, provider_pi, effort_pi) =
            super::patched_bare_launch(&overrides, "pi", ask_pi);
        assert_eq!(model_pi.as_ask(), "b/m");
        let (mut args, pi_pin) =
            crate::bare_model_launch("pi", &model_pi, &provider_pi, effort_pi.as_deref())
                .expect("pi launches");
        assert!(
            args.iter().any(|a| a == "b/m"),
            "patched model reaches pi argv: {args:?}"
        );
        assert_eq!(pi_pin, None, "pi rides argv, no env pin");
        let pi_mode_argv = super::patched_mode_argv(&overrides, "pi", "pi");
        assert!(
            pi_mode_argv.is_empty(),
            "undeployable pi mode rides nothing: {pi_mode_argv:?}"
        );
        args.extend(pi_mode_argv);
        assert!(
            !args.iter().any(|a| a == "--permission-mode"),
            "no mode flag reaches pi argv: {args:?}"
        );
        // opencode: the patched value rides the env pin, not argv.
        let ask_oc = super::prefer_bare_inputs(&prefs, oc_item, "probe", "", "");
        let (model_oc, provider_oc, _) = super::patched_bare_launch(&overrides, "opencode", ask_oc);
        assert_eq!(model_oc.as_ask(), "a/m");
        let (oc_args, oc_pin) = crate::bare_model_launch("opencode", &model_oc, &provider_oc, None)
            .expect("opencode launches");
        assert!(
            oc_args.is_empty(),
            "opencode takes no model argv: {oc_args:?}"
        );
        assert_eq!(
            oc_pin.as_deref(),
            Some("a/m"),
            "patched model reaches opencode env pin"
        );
        // grok: the patched model rides argv and the read-only mode flag joins it.
        let ask_gk = super::prefer_bare_inputs(&prefs, gk_item, "probe", "", "");
        let (model_gk, provider_gk, effort_gk) =
            super::patched_bare_launch(&overrides, "grok", ask_gk);
        assert_eq!(model_gk.as_ask(), "gm9zprobe");
        let (mut gk_args, gk_pin) =
            crate::bare_model_launch("grok", &model_gk, &provider_gk, effort_gk.as_deref())
                .expect("grok launches");
        assert!(
            gk_args.iter().any(|a| a == "gm9zprobe"),
            "patched model reaches grok argv: {gk_args:?}"
        );
        assert_eq!(gk_pin, None, "grok rides argv, no env pin");
        let gk_mode_argv = super::patched_mode_argv(&overrides, "grok", "grok");
        assert_eq!(
            gk_mode_argv,
            vec!["--permission-mode".to_string(), "plan".to_string()],
            "grok read-only mode argv"
        );
        gk_args.extend(gk_mode_argv);
        assert!(
            gk_args.iter().any(|a| a == "--permission-mode"),
            "patched mode reaches grok argv: {gk_args:?}"
        );
    }
}

#[cfg(test)]
mod resolver_property_tests {
    use crate::swarm_plan::{FieldStatus, Provenance};
    use std::collections::BTreeMap;

    const HARNESSES: [&str; 8] = [
        "opencode",
        "pi",
        "grok",
        "codex",
        "claude",
        "fx",
        "antigravity",
        "gemini",
    ];

    fn herm_registry(with_slot: bool) -> crate::models::ModelRegistry {
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
        let mut slots = BTreeMap::new();
        if with_slot {
            slots.insert(
                "standard".to_string(),
                crate::models::SlotSpec {
                    model: "s/m".to_string(),
                    ..Default::default()
                },
            );
        }
        crate::models::ModelRegistry {
            models,
            slots,
            user_keys: Default::default(),
            defaults: None,
        }
    }

    fn test_prefs(pairs: &[(&str, &str)]) -> crate::preferences::PreferenceSet {
        let mut prefs = crate::preferences::PreferenceSet::new();
        for (key, value) in pairs {
            prefs.set(
                key.parse().expect("pref key parses"),
                value.to_string(),
                crate::preferences::Scope::Always,
            );
        }
        prefs
    }

    fn bare_item(kind: &str) -> crate::SpawnItem {
        crate::SpawnItem {
            short: kind.to_string(),
            count: 1,
            kind: Some(kind.to_string()),
            profile: None,
        }
    }

    /// V4: every bare harness carries the planned model iff its status is honored.
    #[test]
    fn resolver_v4_model_bare_matrix() {
        for h in HARNESSES {
            let reg = herm_registry(false);
            let cfg = crate::config::Config::default();
            let catalog = crate::profile::ProfileCatalog::default();
            let prefs = crate::preferences::PreferenceSet::new();
            let items = vec![bare_item(h)];
            let plan = super::build_dry_run_plan(
                &cfg,
                &catalog,
                &reg,
                &prefs,
                "probe",
                crate::cmd::spawn::PlanAsk {
                    items: &items,
                    bare_model: "totally-unknown-9z",
                    bare_provider: "",
                    overrides: &std::collections::BTreeMap::new(),
                },
            )
            .expect("plan builds");
            let f = plan.seats[0].model.as_ref().expect("model field");
            assert_eq!(f.from, Provenance::User, "user ask keeps User on {h}");
            let ask =
                super::prefer_bare_inputs(&prefs, &items[0], "probe", "totally-unknown-9z", "");
            let kind = crate::prefer_kind(&cfg, &items[0], &catalog, &reg, &prefs, "probe")
                .expect("kind resolves");
            assert_eq!(kind, h, "explicit kind wins on {h}");
            assert_eq!(
                kind, plan.seats[0].harness,
                "plan and launch agree on harness {h}"
            );
            let (m, _) = ask.model.clone().expect("user ask present");
            assert_eq!(
                m.as_ask(),
                f.value,
                "one resolution feeds plan and launch on {h}"
            );
            let (eff_model, eff_provider) = ask
                .model
                .map(|(m, _)| (m, ask.provider))
                .unwrap_or_else(|| (crate::models::ModelChoice::parse(""), String::new()));
            let (args, env) = crate::bare_model_launch(&kind, &eff_model, &eff_provider, None)
                .expect("launch builds");
            let honoured = matches!(f.status, FieldStatus::Honored);
            let carried =
                args.iter().any(|a| a == &f.value) || env.as_deref() == Some(f.value.as_str());
            assert_eq!(
                honoured, carried,
                "V4 no false green for model on {h}: {f:?} vs ({args:?}, {env:?})"
            );
        }
    }

    /// V4: every bare harness carries the preferred effort iff its status is honored.
    #[test]
    fn resolver_v4_effort_bare_matrix() {
        for h in HARNESSES {
            let reg = herm_registry(false);
            let cfg = crate::config::Config::default();
            let catalog = crate::profile::ProfileCatalog::default();
            let key = format!("{h}.effort");
            let prefs = test_prefs(&[(key.as_str(), "slow")]);
            let items = vec![bare_item(h)];
            let plan = super::build_dry_run_plan(
                &cfg,
                &catalog,
                &reg,
                &prefs,
                "probe",
                crate::cmd::spawn::PlanAsk {
                    items: &items,
                    bare_model: "a/m",
                    bare_provider: "",
                    overrides: &std::collections::BTreeMap::new(),
                },
            )
            .expect("plan builds");
            let f = plan.seats[0].effort.as_ref().expect("effort field");
            assert_eq!(
                f.from,
                Provenance::Preference,
                "preference fills effort on {h}"
            );
            assert_eq!(f.value, "slow", "preference value flows on {h}");
            let ask = super::prefer_bare_inputs(&prefs, &items[0], "probe", "a/m", "");
            let kind = crate::prefer_kind(&cfg, &items[0], &catalog, &reg, &prefs, "probe")
                .expect("kind resolves");
            let (eff_model, eff_provider) = ask
                .model
                .map(|(m, _)| (m, ask.provider))
                .unwrap_or_else(|| (crate::models::ModelChoice::parse(""), String::new()));
            let (args, _) = crate::bare_model_launch(
                &kind,
                &eff_model,
                &eff_provider,
                ask.effort.as_ref().map(|(e, _)| e.as_str()),
            )
            .expect("launch builds");
            let honoured = matches!(f.status, FieldStatus::Honored);
            let carried = args.iter().any(|a| a == "slow");
            assert_eq!(
                honoured, carried,
                "V4 no false green for effort on {h}: {f:?} vs {args:?}"
            );
        }
    }

    /// V4: the adapter reports honored for a mode iff its argv carries it.
    #[test]
    fn resolver_v4_mode_table() {
        for h in HARNESSES {
            for mode in ["read-only", "workspace-write"] {
                let (args, delivery) = crate::headless::mode_launch_args(h, mode);
                let f =
                    crate::swarm_plan::mode_field(h, Some((mode.to_string(), Provenance::User)))
                        .expect("mode field");
                let honoured = matches!(f.status, FieldStatus::Honored);
                let carried =
                    !args.is_empty() && matches!(delivery, crate::headless::ModeDelivery::Honored);
                assert_eq!(
                    honoured, carried,
                    "V4 mode {h}/{mode}: {f:?} vs ({args:?}, {delivery:?})"
                );
            }
        }
        assert_eq!(
            crate::headless::mode_launch_args("grok", "read-only").0,
            vec!["--permission-mode".to_string(), "plan".to_string()]
        );
        assert_eq!(
            crate::headless::mode_launch_args("pi", "read-only").0,
            vec!["--tools".to_string(), "read,grep,find,ls".to_string()]
        );
    }

    /// V4: the adapter reports honored for a phase iff its argv carries a native mechanic.
    #[test]
    fn resolver_v4_phase_table() {
        for h in HARNESSES {
            for phase in ["implement", "plan", "review"] {
                let (args, degraded) = crate::headless::phase_launch_args(h, phase);
                let f =
                    crate::swarm_plan::phase_field(h, Some((phase.to_string(), Provenance::User)))
                        .expect("phase field");
                let honoured = matches!(f.status, FieldStatus::Honored);
                assert_eq!(
                    honoured,
                    !args.is_empty() && !degraded,
                    "V4 phase {h}/{phase}: {f:?} vs ({args:?}, {degraded})"
                );
            }
        }
        assert_eq!(
            crate::headless::phase_launch_args("opencode", "implement").0,
            vec!["--agent".to_string(), "build".to_string()]
        );
        assert_eq!(
            crate::headless::phase_launch_args("antigravity", "plan").0,
            vec!["--mode".to_string(), "plan".to_string()]
        );
    }

    /// V7: user beats preference beats profile beats slot. Stage and Jev have
    /// no representation in the spawn path (no inputs carry them), so there
    /// is nothing below user/preference that could demote them — see REPORT.
    #[test]
    fn resolver_v7_precedence_table() {
        struct V7<'a> {
            cfg: &'a crate::config::Config,
            catalog: &'a crate::profile::ProfileCatalog,
            reg: &'a crate::models::ModelRegistry,
        }

        impl V7<'_> {
            fn check(
                &self,
                prefs: &crate::preferences::PreferenceSet,
                items: &[crate::SpawnItem],
                user: &str,
                expect: &str,
                expect_from: Provenance,
                case: &str,
            ) {
                let plan = super::build_dry_run_plan(
                    self.cfg,
                    self.catalog,
                    self.reg,
                    prefs,
                    "probe",
                    crate::cmd::spawn::PlanAsk {
                        items,
                        bare_model: user,
                        bare_provider: "",
                        overrides: &std::collections::BTreeMap::new(),
                    },
                )
                .expect("plan builds");
                let f = plan.seats[0].model.as_ref().expect("model field");
                assert_eq!(f.value, expect, "V7 value: {case}");
                assert_eq!(f.from, expect_from, "V7 provenance: {case}");
            }
        }
        let cfg = crate::config::Config::default();
        let mut catalog = crate::profile::ProfileCatalog::default();
        catalog.profiles.insert(
            "coder".to_string(),
            crate::profile::Profile {
                harness: "pi".to_string(),
                model: "b/m".to_string(),
                ..Default::default()
            },
        );
        let reg = herm_registry(true);
        let v7 = V7 {
            cfg: &cfg,
            catalog: &catalog,
            reg: &reg,
        };
        let coder = || crate::SpawnItem {
            short: "coder".to_string(),
            count: 1,
            kind: None,
            profile: Some("coder".to_string()),
        };
        v7.check(
            &test_prefs(&[("pi.model", "a/m")]),
            &[bare_item("pi")],
            "b/m",
            "b/m",
            Provenance::User,
            "user beats preference and slot",
        );
        v7.check(
            &test_prefs(&[("pi.model", "a/m")]),
            &[bare_item("pi")],
            "",
            "a/m",
            Provenance::Preference,
            "preference beats slot",
        );
        v7.check(
            &test_prefs(&[]),
            &[bare_item("pi")],
            "",
            crate::models::HARNESS_DEFAULT,
            Provenance::HarnessDefault,
            "bd-iu00a: the IMPLIED slot yields to harness-default; an explicit @slot still wins",
        );
        v7.check(
            &test_prefs(&[("coder.model", "a/m")]),
            &[coder()],
            "",
            "a/m",
            Provenance::Preference,
            "preference beats profile",
        );
        v7.check(
            &test_prefs(&[]),
            &[coder()],
            "",
            "b/m",
            Provenance::Profile,
            "profile wins with no user or preference",
        );
    }
}

#[cfg(test)]
fn model_ask(ask: &str) -> crate::models::ModelChoice {
    crate::models::ModelChoice::parse(ask)
}

#[cfg(test)]
mod harness_default_model_tests {
    use super::{
        build_dry_run_plan, model_ask, parse_role_patches, patch_role_overrides,
        prefer_bare_inputs, spawn_items_from_plan,
    };
    use crate::profile;
    use std::collections::BTreeMap;

    fn empty_registry() -> crate::models::ModelRegistry {
        crate::models::ModelRegistry {
            models: BTreeMap::new(),
            slots: Default::default(),
            user_keys: Default::default(),
            defaults: None,
        }
    }

    /// Slots AND a coupled default ARE configured, so a `harness-default` ask
    /// that fell through to the implied-slot chain would show up in the launch.
    fn slot_registry() -> crate::models::ModelRegistry {
        use crate::models::{ModelEntry, SlotSpec};
        let mut models = BTreeMap::new();
        for id in ["opencode/big-pickle", "pi/slot-standard"] {
            models.insert(id.to_string(), ModelEntry::default());
        }
        let mut slots = BTreeMap::new();
        slots.insert(
            "cheap".to_string(),
            SlotSpec {
                harness: "opencode".to_string(),
                model: "opencode/big-pickle".to_string(),
                ..Default::default()
            },
        );
        slots.insert(
            "standard".to_string(),
            SlotSpec {
                harness: "pi".to_string(),
                model: "pi/slot-standard".to_string(),
                ..Default::default()
            },
        );
        crate::models::ModelRegistry {
            models,
            slots,
            user_keys: Default::default(),
            defaults: None,
        }
    }

    /// bd-iu00a: a bare spawn of an open harness plans harness-default — no
    /// slot, no --model in argv, from: harness-default (not user).
    #[test]
    fn bare_open_harness_plans_harness_default() {
        let cfg = crate::config::Config::default();
        let reg = slot_registry();
        let catalog = profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        for (short, harness) in [("oc", "opencode"), ("pi", "pi")] {
            let items = spawn_items_from_plan(vec![(short.to_string(), 1)], None);
            let plan = build_dry_run_plan(
                &cfg,
                &catalog,
                &reg,
                &prefs,
                "test",
                crate::cmd::spawn::PlanAsk {
                    items: &items,
                    bare_model: "",
                    bare_provider: "",
                    overrides: &BTreeMap::new(),
                },
            )
            .expect("plan builds");
            let f = plan.seats[0].model.as_ref().expect("model field");
            assert_eq!(
                f.value,
                crate::models::HARNESS_DEFAULT,
                "{short}: bare plan"
            );
            assert_eq!(f.from, crate::swarm_plan::Provenance::HarnessDefault);
            let (args, env_pin) = crate::bare_model_launch(harness, &model_ask(&f.value), "", None)
                .expect("bare launch builds");
            assert!(args.is_empty(), "{short}: no --model: {args:?}");
            assert_eq!(env_pin, None, "{short}: no env pin");
        }
    }

    #[test]
    fn harness_default_plan_says_honored_and_the_launch_injects_nothing() {
        let cfg = crate::config::Config::default();
        let reg = slot_registry();
        let catalog = profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        for (short, harness) in [("oc", "opencode"), ("pi", "pi")] {
            let items = spawn_items_from_plan(vec![(short.to_string(), 1)], None);
            let plan = build_dry_run_plan(
                &cfg,
                &catalog,
                &reg,
                &prefs,
                "test",
                crate::cmd::spawn::PlanAsk {
                    items: &items,
                    bare_model: "harness-default",
                    bare_provider: "",
                    overrides: &BTreeMap::new(),
                },
            )
            .expect("plan builds");
            let f = plan.seats[0].model.as_ref().expect("model field");
            assert_eq!(
                f.value, "harness-default",
                "{short}: the plan names the choice"
            );
            assert_eq!(f.from, crate::swarm_plan::Provenance::User);
            assert_eq!(f.status, crate::swarm_plan::FieldStatus::Honored);
            let (args, env_pin) =
                crate::bare_model_launch(harness, &model_ask("harness-default"), "", None)
                    .expect("bare launch builds");
            assert!(
                args.is_empty(),
                "{short}: no model flag may reach the launch: {args:?}"
            );
            assert_eq!(env_pin, None, "{short}: no model env pin may be injected");
        }
    }

    #[test]
    fn harness_default_from_patch_reaches_the_same_place() {
        let cfg = crate::config::Config::default();
        let reg = empty_registry();
        let catalog = profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let items = spawn_items_from_plan(vec![("pi".to_string(), 1)], None);
        let patches =
            parse_role_patches(&["pi.model=harness-default".to_string()]).expect("patch parses");
        let overrides = patch_role_overrides(&patches);
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &overrides,
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.value, "harness-default");
        assert_eq!(f.from, crate::swarm_plan::Provenance::User);
        assert_eq!(f.status, crate::swarm_plan::FieldStatus::Honored);
        let ask = prefer_bare_inputs(&prefs, &items[0], "test", "", "");
        let (model, provider, effort) = super::patched_bare_launch(&overrides, "pi", ask);
        let (args, env_pin) = crate::bare_model_launch("pi", &model, &provider, effort.as_deref())
            .expect("patched launch builds");
        assert!(args.is_empty(), "patched default injects no argv: {args:?}");
        assert_eq!(env_pin, None, "patched default injects no env pin");
    }

    #[test]
    fn harness_default_profile_injects_nothing() {
        let opencode = crate::profile::Profile {
            harness: "opencode".into(),
            model: "harness-default".into(),
            ..Default::default()
        };
        let launch = crate::profile_launch_plan("opencode", &opencode, None)
            .expect("a harness-default profile launches");
        assert!(launch.args.is_empty(), "no argv: {:?}", launch.args);
        assert_eq!(launch.env_model_pin, None, "no env pin");
        assert_eq!(
            launch.model, "harness-default",
            "the ask is recorded as the sentinel; spawn drops it (no model pin)"
        );

        let mut catalog = profile::ProfileCatalog::default();
        catalog
            .profiles
            .insert("coder".to_string(), opencode.clone());
        let items = spawn_items_from_plan(
            Vec::new(),
            Some(("coder", catalog.profiles.get("coder").unwrap(), 1)),
        );
        let plan = build_dry_run_plan(
            &crate::config::Config::default(),
            &catalog,
            &empty_registry(),
            &crate::preferences::PreferenceSet::new(),
            "test",
            crate::cmd::spawn::PlanAsk {
                items: &items,
                bare_model: "",
                bare_provider: "",
                overrides: &BTreeMap::new(),
            },
        )
        .expect("plan builds");
        let f = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(f.value, "harness-default");
        assert_eq!(f.from, crate::swarm_plan::Provenance::Profile);
        assert_eq!(f.status, crate::swarm_plan::FieldStatus::Honored);
    }
}

#[cfg(test)]
mod add_join_channel_tests {
    /// bd-ll2f2: both add paths join every new pin to the requested channels,
    /// exactly like `spawn --join-channel`.
    #[test]
    fn both_add_paths_join_channels() {
        let src = include_str!("spawn.rs");
        let needle = concat!("if !join_channels", ".is_empty() {");
        assert_eq!(
            src.matches(needle).count(),
            2,
            "hc and rex add must each join the new pins"
        );
    }
}

#[cfg(test)]
mod requested_spawn_flags_tests {
    use super::requested_spawn_flags;
    use crate::SpawnOpts;

    /// bd-gjtcw: the join hint must echo what was asked (`--oc 3`), never a
    /// guessed harness.
    #[test]
    fn echoes_the_requested_harness_counts_and_flags() {
        let mut opts = SpawnOpts::default();
        opts.counts.oc = Some(3);
        assert_eq!(requested_spawn_flags(&opts), "--oc 3");
        opts.kinds = vec![("coder".to_string(), 2)];
        assert_eq!(requested_spawn_flags(&opts), "--oc 3 --kind coder=2");
        opts.counts.oc = None;
        assert_eq!(requested_spawn_flags(&opts), "--kind coder=2");
        assert_eq!(requested_spawn_flags(&SpawnOpts::default()), "");
    }
}

#[cfg(test)]
mod dispatcher_workspace_tests {
    use super::*;

    fn ws(workspace_id: &str) -> hc::WorkspaceInfo {
        hc::WorkspaceInfo {
            workspace_id: workspace_id.to_string(),
            number: 1,
            label: "flywheel-chiebukuro".into(),
            focused: false,
            pane_count: 0,
            tab_count: 0,
            active_tab_id: String::new(),
            agent_status: toron_core::pane_types::AgentStatus::Unknown,
            cwd: None,
        }
    }

    #[test]
    fn dispatcher_workspace_keys_by_the_session_id() {
        assert_eq!(
            dispatcher_workspace(&ws("session:01a1d1e6"), "flywheel-chiebukuro"),
            "session:01a1d1e6",
            "the live session id is the key"
        );
        assert_eq!(
            dispatcher_workspace(&ws(""), "flywheel-chiebukuro"),
            "flywheel-chiebukuro",
            "the label survives only when the plane answers no id"
        );
    }
}

/// bd-koafr: `--assign pin=bd-a,bd-b` pairs for the spawn/add flags.
///
/// One assignment per pin; the value keeps the bead list verbatim (the boot
/// brief text names them all). Duplicate pins are refused: two boot briefs to
/// one pane would race the same `READY` turn.
pub(crate) fn parse_assign(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    for pair in pairs {
        let Some((pin, beads)) = pair.split_once('=') else {
            anyhow::bail!("--assign wants pin=bead[,bead]: {pair}");
        };
        let pin = pin.trim();
        let beads: Vec<&str> = beads
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .collect();
        if pin.is_empty() || beads.is_empty() {
            anyhow::bail!("--assign wants pin=bead[,bead]: {pair}");
        }
        for bead in &beads {
            if !bead.starts_with("bd-") {
                anyhow::bail!("--assign names a bead id (bd-…): {bead}");
            }
        }
        if out.insert(pin.to_string(), beads.join(",")).is_some() {
            anyhow::bail!("--assign names {pin} twice");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod assign_tests {
    use super::parse_assign;

    #[test]
    fn assign_pairs_parse_one_per_pin_and_keep_the_bead_list() {
        let got = parse_assign(&["SlateHarbor=bd-a,bd-b".into(), "BoldBay=bd-c".into()])
            .expect("valid pairs");
        assert_eq!(
            got.get("SlateHarbor").map(String::as_str),
            Some("bd-a,bd-b")
        );
        assert_eq!(got.get("BoldBay").map(String::as_str), Some("bd-c"));
    }

    #[test]
    fn assign_refuses_malformed_duplicate_or_non_bead_input() {
        for bad in [
            vec!["no-equals".to_string()],
            vec!["=bd-a".to_string()],
            vec!["PinA=".to_string()],
            vec!["PinA=notabead".to_string()],
            vec!["PinA=bd-a".to_string(), "PinA=bd-b".to_string()],
        ] {
            assert!(parse_assign(&bad).is_err(), "must refuse {bad:?}");
        }
    }
}

/// bd-koafr: the `--dry-run --json` document. The assignment is a post-launch
/// action (it launches no seat), so it rides the envelope and the seat plan
/// stays byte-equal with `swarm plan`.
pub(crate) fn dry_run_doc(
    plan: &crate::swarm_plan::Plan,
    assign: &std::collections::BTreeMap<String, String>,
) -> serde_json::Value {
    if assign.is_empty() {
        serde_json::to_value(plan).unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::json!({ "plan": plan, "assign": assign })
    }
}

#[cfg(test)]
mod dry_run_doc_tests {
    use super::dry_run_doc;

    #[test]
    fn the_assignment_rides_the_envelope_not_the_seat_plan() {
        let plan = crate::swarm_plan::Plan::new("swarm");
        let empty = dry_run_doc(&plan, &std::collections::BTreeMap::new());
        assert!(
            empty.get("assign").is_none(),
            "no assignment, plan-only doc"
        );
        assert_eq!(empty, serde_json::to_value(&plan).unwrap());
        let mut assign = std::collections::BTreeMap::new();
        assign.insert("SlateHarbor".to_string(), "bd-a,bd-b".to_string());
        let doc = dry_run_doc(&plan, &assign);
        assert_eq!(doc["assign"]["SlateHarbor"], "bd-a,bd-b");
        assert_eq!(
            doc["plan"],
            serde_json::to_value(&plan).unwrap(),
            "the seat plan is unchanged by an assignment"
        );
    }
}

/// bd-koafr: what `--assign` will brief. One entry per pin, carrying the boot
/// line text and the first bead for the brief's corr. Pure, so the witness
/// runs without mail or a pane.
pub(crate) fn planned_briefs(
    assign: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String, Option<String>)> {
    assign
        .iter()
        .map(|(pin, beads)| {
            let first = beads.split(',').next().map(str::to_string);
            (pin.clone(), crate::boot_prime_line_for(Some(beads)), first)
        })
        .collect()
}

/// bd-koafr: deliver the planned briefs through the crew-prompt door — the
/// brief id, the operator-typing guard, and the busy deferral are that door's.
pub(crate) fn dispatch_assignments(
    project: &str,
    assign: &std::collections::BTreeMap<String, String>,
    r#as: Option<&str>,
) -> Result<()> {
    if assign.is_empty() {
        return Ok(());
    }
    let cfg = crate::config::Config::load().unwrap_or_default();
    for (pin, text, bead) in planned_briefs(assign) {
        crate::cmd::crew::cmd_crew_prompt(
            &cfg,
            project,
            &pin,
            &text,
            r#as,
            false,
            bead.as_deref(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod planned_briefs_tests {
    use super::planned_briefs;

    /// bd-koafr: exactly one brief per assigned pin, and the text is the
    /// assignment variant of the boot line (names the bead, forbids triage).
    #[test]
    fn one_brief_per_assigned_pin_naming_its_beads() {
        let mut assign = std::collections::BTreeMap::new();
        assign.insert("SlateHarbor".to_string(), "bd-a,bd-b".to_string());
        assign.insert("BoldBay".to_string(), "bd-c".to_string());
        let briefs = planned_briefs(&assign);
        assert_eq!(briefs.len(), 2, "one brief per pin: {briefs:?}");
        let slate = briefs
            .iter()
            .find(|(pin, _, _)| pin == "SlateHarbor")
            .expect("slate brief");
        assert!(slate.1.contains("bd-a,bd-b"), "{}", slate.1);
        assert!(slate.1.contains("Do not read mail"), "{}", slate.1);
        assert_eq!(slate.2.as_deref(), Some("bd-a"), "corr is the first bead");
        assert!(planned_briefs(&std::collections::BTreeMap::new()).is_empty());
    }
}

#[cfg(test)]
mod acceptance1_harness_pair {
    use std::collections::BTreeMap;

    use super::{PlanAsk, build_dry_run_plan, spawn_items_from_plan};

    /// bd-ft30t: the acceptance text names the CLI spelling that exists today,
    /// while keeping the original spelling as history rather than erasing it.
    #[test]
    fn adr_0022_acceptance_text_uses_the_current_kind_spelling() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/decisions/0022-swarm-plan-one-contract-from-intent-to-crew.md");
        let adr = std::fs::read_to_string(path).expect("ADR-0022 text");
        assert!(
            adr.contains("--kind gemini=1"),
            "the current gemini spelling must be in the acceptance"
        );
        assert!(
            adr.contains("--kind fx=1"),
            "the current fx spelling must be in the witness table"
        );
        assert!(
            adr.contains("--harness fx"),
            "the original spelling stays as history"
        );
    }

    fn registry() -> crate::models::ModelRegistry {
        use crate::models::ModelEntry;
        let mut models = BTreeMap::new();
        models.insert("gemini/gemini-2.5-pro".to_string(), ModelEntry::default());
        crate::models::ModelRegistry {
            models,
            slots: BTreeMap::new(),
            user_keys: Default::default(),
            defaults: None,
        }
    }

    /// ADR-0022 acceptance 1 (bd-6setu.10): one explicit-model ask is honored on
    /// gemini, whose launch argv carries it, and BLOCKED on fx, which has no model
    /// flag at all. The plan says which is which; the argv is read from the
    /// launcher, not from the status.
    #[test]
    fn acceptance1_gemini_honored_fx_blocked() {
        let cfg = crate::config::Config::default();
        let catalog = crate::profile::ProfileCatalog::default();
        let prefs = crate::preferences::PreferenceSet::new();
        let reg = registry();

        let items = spawn_items_from_plan(vec![("gemini".to_string(), 1)], None);
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "test",
            PlanAsk {
                items: &items,
                bare_model: "gemini-2.5-pro",
                bare_provider: "",
                overrides: &BTreeMap::new(),
            },
        )
        .expect("gemini plan builds");
        let field = plan.seats[0].model.as_ref().expect("model field");
        assert_eq!(
            field.status,
            crate::swarm_plan::FieldStatus::Honored,
            "gemini honours an explicit model: {field:?}"
        );
        // A pane seat launches through interactive_model_plan, not the raw
        // headless argv helper. Both resolve to the same argv today; asserting
        // on the launcher keeps this witness on the path a gemini seat uses.
        let launch = crate::headless::interactive_model_plan("gemini", "gemini-2.5-pro", None);
        let args = launch.args();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-m" && w[1] == "gemini-2.5-pro"),
            "the interactive launch carries -m gemini-2.5-pro: {args:?}"
        );

        let items = spawn_items_from_plan(vec![("fx".to_string(), 1)], None);
        let plan = build_dry_run_plan(
            &cfg,
            &catalog,
            &reg,
            &prefs,
            "test",
            PlanAsk {
                items: &items,
                bare_model: "gemini-2.5-pro",
                bare_provider: "",
                overrides: &BTreeMap::new(),
            },
        )
        .expect("fx plan builds");
        let field = plan.seats[0].model.as_ref().expect("model field");
        match &field.status {
            crate::swarm_plan::FieldStatus::Blocked { remedy } => assert!(
                remedy.contains("fx"),
                "the remedy names the harness: {remedy}"
            ),
            other => panic!("fx takes no model flag, so the ask is blocked: {other:?}"),
        }
    }
}

#[cfg(test)]
mod e4gal_harness_row_tests {
    use super::with_recorded_harness;

    /// The registry row must carry the harness the pane was launched with.
    #[test]
    fn a_profileless_spawn_row_is_found_by_harness_of() {
        let _home = crate::config::test_state_home();
        let root = std::env::temp_dir().join(format!("fw-e4gal-{}", std::process::id()));
        let _override = crate::state::set_registry_root_override(root);

        let row = crate::state::SpawnEntry::with_mail_identity(
            "block:probe-pane",
            "AmberProbe",
            "AmberProbe",
        );
        assert!(
            row.harness.is_none(),
            "precondition: a profileless constructor leaves harness unset"
        );
        let row = with_recorded_harness(row, "opencode");
        crate::state::save_spawn_registry("e4gal-probe", &[row]).unwrap();

        assert_eq!(
            crate::pane_read::harness_of("e4gal-probe", "block:probe-pane"),
            "opencode",
            "the operator-typing guard must resolve the harness for this exact target pane"
        );
    }
}

#[cfg(test)]
mod add_model_launch_tests {
    use super::add_model_launch;

    #[test]
    fn add_model_reaches_the_launch_argv_for_a_flagged_harness() {
        let (args, env_pin) = add_model_launch("pi", Some("zai/glm-5.2")).expect("pi launch");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "zai/glm-5.2"),
            "the launched argv must carry the ask: {args:?}"
        );
        assert_eq!(env_pin, None);
    }

    #[test]
    fn add_model_reaches_the_pane_env_for_opencode() {
        let (args, env_pin) = add_model_launch("opencode", Some("opencode-go/deepseek-v4.1-flash"))
            .expect("opencode launch");
        assert!(
            !args.iter().any(|a| a == "--model"),
            "opencode's root rejects --model: {args:?}"
        );
        assert_eq!(
            env_pin.as_deref(),
            Some("opencode-go/deepseek-v4.1-flash"),
            "the model must ride the pane env where argv cannot carry it"
        );
    }

    #[test]
    fn add_without_a_model_keeps_the_harness_default() {
        let (args, env_pin) = add_model_launch("pi", None).expect("no model");
        assert!(args.is_empty());
        assert_eq!(env_pin, None);
    }

    #[test]
    fn both_add_paths_wire_the_model_into_launch() {
        let src = include_str!("spawn.rs");
        for (name, body) in [
            ("rex", crate::tripwire::fn_body(src, "cmd_add_rex")),
            ("hc", crate::tripwire::fn_body(src, "cmd_add")),
        ] {
            assert!(
                body.contains("add_model_launch("),
                "{name} add must compute the model launch"
            );
            assert!(
                body.contains("apply_opencode_model_pin("),
                "{name} add must pin an env-delivered model"
            );
        }
    }
}
