//! `flywheel swarm plan | apply | status` (ADR-0022 rollout 6): the three
//! verbs that front the plan, a live patch, and the live view.
//!
//! `plan` prints the SAME `flywheel-plan.v1` object as
//! `spawn --dry-run --json`: it delegates to `cmd_spawn` with `dry_run` and
//! `json` set, so there is exactly one plan path.
//!
//! `apply` is the D7 one mechanism for a live change: a patch updates the
//! desired seats, the pure reconciler picks the cheapest effect, and the one
//! seat interpreter (`Ports::seat`) runs it under the effect journal. Never a
//! parallel path; `binding-rebind --model` routes here.
//!
//! `status` lists the live seats the reconciler sees, with the plane's status.

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::json;

use crate::cmd::spawn::cmd_spawn;
use crate::config::Config;
use crate::output::print_json;
use crate::seat_binder::{LiveSeat, SeatPlacement};
use crate::seat_reconcile::{DesiredSeat, ReconcilePolicy, SeatEffect, reconcile};
use crate::state::SpawnEntry;

#[derive(Subcommand)]
pub(crate) enum SwarmCmd {
    /// Print the flywheel-plan.v1 object (byte-identical to `spawn --dry-run --json`)
    Plan {
        project: String,
        #[command(flatten)]
        counts: crate::AgentCounts,
        /// spawn recipe name from recipes.toml
        #[arg(long)]
        recipe: Option<String>,
        /// extra harness kind counts: --kind claude=1 (repeatable)
        #[arg(long = "kind", value_name = "KIND=COUNT", value_parser = crate::parse_kind_count)]
        kinds: Vec<(String, u32)>,
        /// spawn agents from a profile
        #[arg(long)]
        profile: Option<String>,
        /// how many profile agents to plan (with --profile; default 1)
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// bare harness+model override for kind/count seats
        #[arg(long)]
        model: Option<String>,
        /// provider pin for --model's composite registry key
        #[arg(long, requires = "model")]
        provider: Option<String>,
        /// re-resolve one seat: `--patch <seat>.<field>=<value>`
        #[arg(long, value_name = "SEAT.FIELD=VALUE")]
        patch: Vec<String>,
    },
    /// Apply a seat patch on a live swarm through the reconciler
    Apply {
        project: String,
        /// `<seat>.<field>=<value>`, field in harness|model|effort|mode|role.
        /// `<seat>` is a pin, a role, or a terminal name.
        #[arg(long, value_name = "SEAT.FIELD=VALUE")]
        patch: Vec<String>,
        /// grow a role: `--count <role>=<n>` adds n seats (reconciler Spawn).
        #[arg(long, value_name = "ROLE=N")]
        count: Vec<String>,
        /// machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// List live seats with status
    Status {
        /// project slug (default: the workspace cwd)
        project: Option<String>,
        /// machine-readable rows
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_swarm(cfg: &Config, cmd: &SwarmCmd) -> Result<()> {
    match cmd {
        SwarmCmd::Plan {
            project,
            counts,
            recipe,
            kinds,
            profile,
            count,
            model,
            provider,
            patch,
        } => cmd_swarm_plan(
            cfg,
            project,
            PlanArgs {
                counts: counts.clone(),
                recipe: recipe.clone(),
                kinds: kinds.clone(),
                profile: profile.clone(),
                count: *count,
                model: model.clone(),
                provider: provider.clone(),
                patch: patch.clone(),
            },
        ),
        SwarmCmd::Apply {
            project,
            patch,
            count,
            json,
        } => cmd_swarm_apply(cfg, project, patch, count, *json),
        SwarmCmd::Status { project, json } => {
            let project = crate::cron::detect_project(cfg, project.as_deref()).ok_or_else(|| {
                anyhow::anyhow!(
                    "swarm status: no project — pass <project> or run from a workspace/<project> cwd"
                )
            })?;
            cmd_swarm_status(cfg, &project, *json)
        }
    }
}

/// The plan half of a spawn ask; `plan` is a pure front for it.
pub(crate) struct PlanArgs {
    pub counts: crate::AgentCounts,
    pub recipe: Option<String>,
    pub kinds: Vec<(String, u32)>,
    pub profile: Option<String>,
    pub count: u32,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub patch: Vec<String>,
}

/// Byte-identical to `spawn <project> … --dry-run --json`: same builder, same
/// printer, no second path.
fn cmd_swarm_plan(cfg: &Config, project: &str, args: PlanArgs) -> Result<()> {
    cmd_spawn(
        cfg,
        project,
        crate::SpawnOpts {
            counts: args.counts,
            recipe: args.recipe,
            kinds: args.kinds,
            profile_name: args.profile,
            profile_count: args.count,
            bare_model: args.model.unwrap_or_default(),
            bare_provider: args.provider.unwrap_or_default(),
            patches: args.patch,
            dry_run: true,
            json: true,
            ..crate::SpawnOpts::default()
        },
    )
}

/// One seat as the plan and the plane see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeatRow {
    pub pin: String,
    pub name: String,
    pub role: crate::kernel::SeatRole,
    pub harness: crate::kernel::SeatHarness,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub mode: Option<String>,
    pub pane_id: String,
    pub status: String,
}

/// Registry rows joined to the plane's agent status. Pure: the caller supplies
/// the agent facts. A row whose pane is not live reads `gone`.
pub(crate) fn seat_rows(
    registry: &[SpawnEntry],
    agents: &[(String, String)],
    captain: Option<&str>,
) -> Vec<SeatRow> {
    registry
        .iter()
        .filter_map(|e| {
            let pin = e
                .mail_identity
                .clone()
                .filter(|p| !p.is_empty())
                .or_else(|| Some(e.name.clone()))?;
            // The captain/operator seat is not a crew seat (role-resolved).
            if captain == Some(pin.as_str()) {
                return None;
            }
            let status = agents
                .iter()
                .find(|(pane, _)| pane == &e.pane_id)
                .map(|(_, status)| status.clone())
                .unwrap_or_else(|| "gone".to_string());
            Some(SeatRow {
                pin,
                name: e.name.clone(),
                role: e
                    .profile
                    .clone()
                    .unwrap_or_else(|| "crew".to_string())
                    .into(),
                harness: e.harness.clone().unwrap_or_default().into(),
                model: e.model.clone().or_else(|| e.model_pin.clone()),
                effort: e.effort.clone(),
                mode: e.mode.clone(),
                pane_id: e.pane_id.clone(),
                status,
            })
        })
        .collect()
}

fn cmd_swarm_status(cfg: &Config, project: &str, json: bool) -> Result<()> {
    let registry = crate::state::load_spawn_registry(project);
    if registry.is_empty() {
        bail!("swarm status: no spawn registry for {project} — is the swarm spawned?");
    }
    let agents: Vec<(String, String)> = crate::backend::multiplexer_backend(cfg)
        .ok()
        .and_then(|b| b.agent_list().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.pane_id, a.agent_status.as_str().to_string()))
        .collect();
    let captain = crate::state::captain_pin(
        project,
        &crate::project_dir_str(cfg, project).unwrap_or_else(|_| project.to_string()),
    );
    let rows = seat_rows(&registry, &agents, captain.as_deref());
    if json {
        print_json(&json!({
            "schema": "flywheel-seats.v1",
            "project": project,
            "seats": rows.iter().map(|r| json!({
                "pin": r.pin,
                "name": r.name,
                "role": r.role,
                "harness": r.harness,
                "model": r.model,
                "effort": r.effort,
                "mode": r.mode,
                "pane_id": r.pane_id,
                "status": r.status,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    for r in &rows {
        println!(
            "{:<16} {:<10} {:<8} {:<28} {:<10} {}",
            r.pin,
            r.role,
            r.harness,
            r.model.as_deref().unwrap_or("-"),
            r.status,
            r.pane_id
        );
    }
    Ok(())
}

/// Apply one or more `<seat>.<field>=<value>` patches to the live plan.
///
/// `<seat>` matches a pin, a role, or a terminal name. Unknown seats fail
/// naming the live ones. The reconciler then decides, and each non-`Keep`
/// effect runs through the one seat interpreter under the effect journal.
pub(crate) fn cmd_swarm_apply(
    cfg: &Config,
    project: &str,
    patches: &[String],
    counts: &[String],
    json: bool,
) -> Result<()> {
    let registry = crate::state::load_spawn_registry(project);
    if registry.is_empty() {
        bail!("swarm apply: no spawn registry for {project} — is the swarm spawned?");
    }
    let agents: Vec<(String, String)> = crate::backend::multiplexer_backend(cfg)
        .ok()
        .and_then(|b| b.agent_list().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.pane_id, a.agent_status.as_str().to_string()))
        .collect();
    let captain = crate::state::captain_pin(
        project,
        &crate::project_dir_str(cfg, project).unwrap_or_else(|_| project.to_string()),
    );
    let rows = seat_rows(&registry, &agents, captain.as_deref());
    if patches.is_empty() && counts.is_empty() {
        bail!("swarm apply: pass --patch <seat>.<field>=<value> and/or --count <role>=<n>");
    }
    let desired = patched_desired(&rows, patches, counts)?;
    let live: Vec<LiveSeat> = rows
        .iter()
        .map(|r| LiveSeat {
            pin: r.pin.clone(),
            role: r.role.clone(),
            harness: r.harness.clone(),
            model: r.model.clone(),
            mode: r.mode.clone(),
            effort: r.effort.clone(),
            placement: SeatPlacement::Pane,
            idle: matches!(r.status.as_str(), "idle" | "done"),
            fresh_session: false,
            idle_since_ms: None,
        })
        .collect();
    let by_env = |harness: &str| {
        matches!(
            crate::headless::interactive_model_plan(harness, "", None),
            crate::headless::InteractiveModelPlan::PinnedByEnv { .. }
        )
    };
    let effects = reconcile(&desired, &live, &by_env, &ReconcilePolicy::default());
    let applied: Vec<SeatEffect> = effects
        .into_iter()
        .filter(|e| !matches!(e, SeatEffect::Keep { .. }))
        .collect();
    if applied.is_empty() {
        if json {
            print_json(
                &json!({ "project": project, "applied": [], "note": "already at the patched plan" }),
            );
        } else {
            println!("swarm apply: the live swarm already matches the patch");
        }
        return Ok(());
    }

    let mut journal =
        crate::supervisor_host::ToronEffectJournal::open("swarm-apply", "swarm-apply")?;
    let mut ports = crate::supervisor_host::LivePorts::new(cfg, project, project);
    let rep = dispatch_effects(&mut journal, &mut ports, &applied);
    if rep.failed > 0 {
        bail!(
            "swarm apply: {} of {} seat effect(s) failed — see the journal",
            rep.failed,
            applied.len()
        );
    }
    if !rep.suppressed.is_empty() {
        eprintln!(
            "swarm apply: {} effect(s) suppressed (FLYWHEEL_ALLOW_EXEC=0) — the plan is decided, nothing ran",
            rep.suppressed.len()
        );
    }
    if json {
        print_json(&json!({
            "project": project,
            "effects": applied.iter().map(effect_json).collect::<Vec<_>>(),
            "ran": rep.ran,
            "skipped": rep.skipped,
            "suppressed": rep.suppressed.len(),
        }));
    } else {
        for effect in &applied {
            println!(
                "{} {}",
                if rep.ran > 0 { "applied" } else { "planned" },
                effect_line(effect)
            );
        }
    }
    Ok(())
}

/// One `flywheel retire` run's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetireOutcome {
    pub pins: Vec<String>,
    pub released: u64,
    pub closed: Vec<(String, String)>,
}

/// The retire core: ONE journaled `SeatEffect::Retire` through the seat
/// interpreter, then the pin's leases (operator-signed, via `release`), then
/// deregistration. Refuses the captain/operator seat by role, and a working
/// pin without `force_working` (mirrors `kill`). `release` is the one
/// operator-signed lease call, injected so the path is testable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn retire_with(
    backend: &dyn crate::backend::MultiplexerBackend,
    cfg: &Config,
    project: &str,
    pins: &[String],
    force_working: bool,
    journal: &mut dyn crate::supervisor_host::EffectJournal,
    ports: &mut dyn crate::supervisor_host::Ports,
    release: &mut dyn FnMut(&str) -> Result<u64>,
) -> Result<RetireOutcome> {
    if pins.is_empty() {
        bail!("retire: name at least one pin");
    }
    let key = crate::project_dir_str(cfg, project)?;
    let captain = crate::state::captain_pin(project, &key);
    for pin in pins {
        if captain.as_deref() == Some(pin.as_str()) {
            bail!("retire refuses the captain/operator seat {pin} (role-resolved)");
        }
    }
    let registry = crate::state::load_spawn_registry(project);
    let agents = backend.agent_list().unwrap_or_default();
    let mut closed = Vec::new();
    for pin in pins {
        let pane = registry
            .iter()
            .find(|e| e.mail_identity.as_deref() == Some(pin.as_str()))
            .map(|e| e.pane_id.clone())
            .filter(|p| !p.is_empty());
        let working = pane
            .as_ref()
            .and_then(|p| agents.iter().find(|a| &a.pane_id == p))
            .is_some_and(|a| a.agent_status == toron_core::pane_types::AgentStatus::Working);
        if working && !force_working {
            bail!("refuse retire: {pin} is working; pass --force-working to retire anyway");
        }
        closed.push((pin.clone(), pane.unwrap_or_default()));
    }

    // The one seat path: journal first, then the interpreter closes the pane.
    let rep = dispatch_effects(
        journal,
        ports,
        &[crate::seat_reconcile::SeatEffect::Retire {
            pins: pins.to_vec(),
        }],
    );
    if rep.failed > 0 {
        bail!(
            "retire: {} seat effect(s) failed — see the journal",
            rep.failed
        );
    }
    if rep.ran == 0 && !rep.suppressed.is_empty() {
        bail!("retire: the seat effect was suppressed (FLYWHEEL_ALLOW_EXEC=0); nothing closed");
    }

    let mut released = 0u64;
    for pin in pins {
        released += release(pin)?;
    }

    // Deregister: drop exactly these rows (siblings untouched).
    let mut entries = registry;
    entries.retain(|e| {
        !pins
            .iter()
            .any(|p| e.mail_identity.as_deref() == Some(p.as_str()))
    });
    crate::state::save_spawn_registry(project, &entries)?;
    Ok(RetireOutcome {
        pins: pins.to_vec(),
        released,
        closed,
    })
}

/// `flywheel retire <pin>... [--project p] [--force-working] [--as <pin>]`.
pub(crate) fn cmd_retire(
    cfg: &Config,
    project: &str,
    pins: &[String],
    force_working: bool,
    actor: Option<&str>,
    json: bool,
) -> Result<()> {
    let key = crate::project_dir_str(cfg, project)?;
    // Refuse the captain BEFORE touching the plane: the captain/operator seat
    // is never a retire target, and the refusal must not depend on a backend.
    let captain = crate::state::captain_pin(project, &key);
    for pin in pins {
        if captain.as_deref() == Some(pin.as_str()) {
            bail!("retire refuses the captain/operator seat {pin} (role-resolved)");
        }
    }
    let backend = crate::backend::multiplexer_backend(cfg)?;
    let actor = actor
        .map(str::to_string)
        .unwrap_or_else(crate::operator_pin);
    let mail = crate::cmd::mail::mail_or_die(cfg)?;
    let mut journal = crate::supervisor_host::ToronEffectJournal::open("retire", "retire")?;
    let mut ports = crate::supervisor_host::LivePorts::new(cfg, project, project);
    let mut release = |pin: &str| -> Result<u64> {
        let r = mail.release_by_holder(&key, &actor, pin, false)?;
        Ok(r.get("released")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    };
    let out = retire_with(
        &*backend,
        cfg,
        project,
        pins,
        force_working,
        &mut journal,
        &mut ports,
        &mut release,
    )?;
    if json {
        print_json(&json!({
            "project": project,
            "retired": out.pins,
            "released": out.released,
            "closed": out.closed.iter().map(|(p, pane)| json!({"pin": p, "pane_id": pane})).collect::<Vec<_>>(),
        }));
    } else {
        for (pin, pane) in &out.closed {
            println!("retired {pin} (pane {pane})");
        }
        println!(
            "released {} lease(s) held by {}",
            out.released,
            out.pins.join(", ")
        );
    }
    Ok(())
}

/// Dispatch the reconciler's effects through the 5d interpreter. Exposed for
/// the unit proof: exactly one `Seat` command per non-`Keep` effect, journal
/// first.
pub(crate) fn dispatch_effects(
    journal: &mut dyn crate::supervisor_host::EffectJournal,
    ports: &mut dyn crate::supervisor_host::Ports,
    effects: &[SeatEffect],
) -> crate::supervisor_host::RoundReport {
    let mut rep = crate::supervisor_host::RoundReport::default();
    for effect in effects {
        if matches!(effect, SeatEffect::Keep { .. }) {
            continue;
        }
        let key = crate::supervisor_program::seat_effect_key(effect);
        let _ = crate::supervisor_host::execute(
            journal,
            ports,
            &crate::supervisor_program::SupCmd::Seat {
                effect: effect.clone(),
                key,
            },
            &mut rep,
        );
    }
    rep
}

fn field_names(fields: &[crate::seat_reconcile::SeatField]) -> String {
    fields
        .iter()
        .map(|f| f.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn effect_json(effect: &SeatEffect) -> serde_json::Value {
    serde_json::to_value(effect).unwrap_or(serde_json::Value::Null)
}

fn effect_line(effect: &SeatEffect) -> String {
    match effect {
        SeatEffect::Relaunch { pin, fields, .. } => {
            format!("relaunch {pin} ({})", field_names(fields))
        }
        SeatEffect::Respawn { pin, fields, .. } => {
            format!("respawn {pin} ({})", field_names(fields))
        }
        SeatEffect::RetireIdle { pin } | SeatEffect::RetireAfterTurn { pin } => {
            format!("retire {pin}")
        }
        SeatEffect::Retire { pins } => format!("retire {}", pins.join(",")),
        SeatEffect::InjectRole { pin, role } => format!("inject {role} into {pin}"),
        SeatEffect::Spawn { role, count } => format!("spawn {count} {role}"),
        SeatEffect::Keep { pin } => format!("keep {pin}"),
    }
}

/// Apply the patches to the desired rows. A patch matches a pin, a role, or a
/// terminal name; an unknown seat fails naming the live ones.
fn patched_desired(
    rows: &[SeatRow],
    patches: &[String],
    counts: &[String],
) -> Result<Vec<DesiredSeat>> {
    let mut desired: Vec<DesiredSeat> = rows
        .iter()
        .map(|r| DesiredSeat {
            pin: Some(r.pin.clone()),
            role: r.role.clone(),
            harness: r.harness.clone(),
            model: r.model.clone(),
            effort: r.effort.clone(),
            mode: r.mode.clone(),
            placement: SeatPlacement::Pane,
            context_warm_allowed: !crate::seat_reconcile::role_is_fresh(&r.role),
        })
        .collect();
    for spec in patches {
        let patch = crate::swarm_plan::parse_patch(spec)
            .map_err(|e| anyhow::anyhow!("bad --patch {spec:?}: {e}"))?;
        let matched: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.pin == patch.seat || r.role.as_str() == patch.seat || r.name == patch.seat
            })
            .map(|(i, _)| i)
            .collect();
        if matched.is_empty() {
            let mut names: Vec<String> = rows
                .iter()
                .flat_map(|r| [r.pin.clone(), r.role.as_str().to_string(), r.name.clone()])
                .collect();
            names.sort();
            names.dedup();
            bail!(
                "swarm apply: unknown seat {:?} (live: {})",
                patch.seat,
                names.join(", ")
            );
        }
        for i in matched {
            let seat = &mut desired[i];
            match patch.field {
                crate::swarm_plan::PatchField::Harness => {
                    seat.harness = patch.value.clone().into();
                }
                crate::swarm_plan::PatchField::Model => {
                    seat.model = Some(patch.value.clone());
                }
                crate::swarm_plan::PatchField::Effort => {
                    seat.effort = Some(patch.value.clone());
                }
                crate::swarm_plan::PatchField::Mode => {
                    seat.mode = Some(patch.value.clone());
                }
                crate::swarm_plan::PatchField::Role => {
                    // A review/verify seat (current OR target) must not inherit
                    // context: the reconciler respawns instead of injecting.
                    let current_fresh = crate::seat_reconcile::role_is_fresh(&seat.role);
                    let target = crate::kernel::SeatRole::from(patch.value.as_str());
                    seat.role = target.clone();
                    seat.context_warm_allowed =
                        !current_fresh && !crate::seat_reconcile::role_is_fresh(&target);
                }
            }
        }
    }
    for spec in counts {
        let (role, delta) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("bad --count {spec:?}: want <role>=<n>"))?;
        let delta: usize =
            delta.trim().trim_start_matches('+').parse().map_err(|_| {
                anyhow::anyhow!("bad --count {spec:?}: n must be a positive integer")
            })?;
        if delta == 0 {
            continue;
        }
        let Some(template) = rows.iter().find(|r| r.role.as_str() == role) else {
            let mut names: Vec<&str> = rows.iter().map(|r| r.role.as_str()).collect();
            names.sort();
            names.dedup();
            return Err(anyhow::anyhow!(
                "swarm apply: --count role {:?} has no live seat (live: {})",
                role,
                names.join(", ")
            ));
        };
        for _ in 0..delta {
            desired.push(DesiredSeat {
                pin: None,
                role: role.into(),
                harness: template.harness.clone(),
                model: template.model.clone(),
                effort: template.effort.clone(),
                mode: template.mode.clone(),
                placement: SeatPlacement::Pane,
                context_warm_allowed: !crate::seat_reconcile::role_is_fresh(&role.into()),
            });
        }
    }
    Ok(desired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor_host::{MemJournal, Ports, RoundReport, execute};
    use crate::supervisor_program::SupCmd;
    use std::sync::{Arc, Mutex};

    fn entry(
        pane: &str,
        pin: &str,
        name: &str,
        role: &str,
        harness: &str,
        model: &str,
    ) -> SpawnEntry {
        let mut e = SpawnEntry::profiled(pane, name, role, harness, Some(pin.to_string()));
        e.model = Some(model.to_string());
        e
    }

    // ---- ADR-0022 D7 remainder: Spawn + InjectRole + effort/mode ----

    use crate::backend::MultiplexerBackend;
    use crate::config::lock_env;
    use crate::hc::AgentStatus;
    use crate::hc::{AgentInfo, PaneInfo, WorkspaceInfo};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hermetic cfg: temp workspace root + temp XDG dirs, and a real project
    /// dir so the registry key is the same whether the caller passes the name
    /// or the path.
    fn hermetic() -> (Config, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_home = dir.path().join("state");
        let config_home = dir.path().join("config");
        unsafe { std::env::set_var("XDG_STATE_HOME", &state_home) };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config_home) };
        std::fs::create_dir_all(config_home.join("flywheel")).expect("config dir");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("demo")).expect("project dir");
        let cfg = Config {
            workspace_root: Some(ws.to_string_lossy().into_owned()),
            ..Default::default()
        };
        (cfg, dir)
    }

    fn agent(name: &str, pane: &str, ws: &str) -> AgentInfo {
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
            workspace_id: ws.to_string(),
        }
    }

    fn agent_with_status(name: &str, pane: &str, ws: &str, status: AgentStatus) -> AgentInfo {
        let mut a = agent(name, pane, ws);
        a.agent_status = status;
        a
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

    struct FakeMux {
        agents: Mutex<Vec<AgentInfo>>,
        splits: AtomicUsize,
        prompts: Mutex<Vec<(String, String)>>,
        closed: Mutex<Vec<String>>,
        panes: Mutex<Vec<String>>,
    }

    impl Default for FakeMux {
        fn default() -> Self {
            Self {
                agents: Mutex::new(Vec::new()),
                splits: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
                panes: Mutex::new(vec!["wA:p1".to_string()]),
            }
        }
    }

    impl MultiplexerBackend for FakeMux {
        fn name(&self) -> &'static str {
            "fake-mux"
        }
        fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>, toron_plane::backend::BackendError> {
            Ok(vec![workspace("demo")])
        }
        fn pane_list(
            &self,
            _workspace_id: Option<&str>,
        ) -> Result<Vec<PaneInfo>, toron_plane::backend::BackendError> {
            Ok(self
                .panes
                .lock()
                .unwrap()
                .iter()
                .map(|id| {
                    let n = id
                        .rsplit_once(":p")
                        .and_then(|(_, n)| n.parse::<u32>().ok())
                        .unwrap_or(1);
                    pane("wA", n)
                })
                .collect())
        }
        fn pane_split(
            &self,
            _parent_pane_id: &str,
            _direction: &str,
            _cwd: Option<&str>,
        ) -> Result<PaneInfo, toron_plane::backend::BackendError> {
            let n = self.splits.fetch_add(1, Ordering::SeqCst) + 2;
            Ok(pane("wA", n as u32))
        }
        fn pane_close(&self, pane_id: &str) -> Result<(), toron_plane::backend::BackendError> {
            let mut panes = self.panes.lock().unwrap();
            let Some(i) = panes.iter().position(|p| p == pane_id) else {
                return Err(toron_plane::backend::BackendError::new(
                    toron_plane::backend::BackendErrorCode::Other("pane_not_found".into()),
                    format!("pane {pane_id} not found"),
                ));
            };
            panes.remove(i);
            self.closed.lock().unwrap().push(pane_id.to_string());
            Ok(())
        }
        fn agent_list(&self) -> Result<Vec<AgentInfo>, toron_plane::backend::BackendError> {
            Ok(self.agents.lock().unwrap().clone())
        }
        fn agent_start(
            &self,
            name: &str,
            _kind: &str,
            pane_id: &str,
            _args: &[String],
        ) -> Result<AgentInfo, toron_plane::backend::BackendError> {
            let a = agent(name, pane_id, "wA");
            self.agents.lock().unwrap().push(a.clone());
            Ok(a)
        }
        fn agent_rename(
            &self,
            _target: &str,
            _new_name: Option<&str>,
        ) -> Result<(), toron_plane::backend::BackendError> {
            Ok(())
        }
        fn prompt(
            &self,
            target: &str,
            text: &str,
            _wait: Option<&toron_plane::backend::WaitOptions>,
        ) -> Result<serde_json::Value, toron_plane::backend::BackendError> {
            self.prompts
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(serde_json::json!({"ok": true}))
        }
    }

    /// The three verbs parse (ADR-0022 rollout 6): `plan`, `apply`, `status`.
    #[test]
    fn swarm_verbs_parse() {
        let handle = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                use clap::Parser;
                let plan = crate::Cli::try_parse_from([
                    "flywheel", "swarm", "plan", "proj", "--pi", "1", "--model", "m",
                ])
                .expect("swarm plan parses");
                assert!(matches!(
                    plan.command,
                    Some(crate::Commands::Swarm {
                        cmd: SwarmCmd::Plan { .. }
                    })
                ));
                let apply = crate::Cli::try_parse_from([
                    "flywheel",
                    "swarm",
                    "apply",
                    "proj",
                    "--patch",
                    "reviewer.model=x",
                ])
                .expect("swarm apply parses");
                assert!(matches!(
                    apply.command,
                    Some(crate::Commands::Swarm {
                        cmd: SwarmCmd::Apply { .. }
                    })
                ));
                let status =
                    crate::Cli::try_parse_from(["flywheel", "swarm", "status", "proj", "--json"])
                        .expect("swarm status parses");
                assert!(matches!(
                    status.command,
                    Some(crate::Commands::Swarm {
                        cmd: SwarmCmd::Status { .. }
                    })
                ));
            })
            .expect("spawn the parse thread");
        handle.join().expect("the parse assertions must hold");
    }

    /// V6/plan: `swarm plan` delegates to `spawn --dry-run --json`; the
    /// delegation is the byte-equality proof (one builder, one printer).
    #[test]
    fn swarm_plan_is_the_spawn_dry_run_path() {
        let src = include_str!("swarm.rs");
        let body = &src[src.find("fn cmd_swarm_plan").expect("plan fn")..];
        let body = &body[..body.find("\n}\n").expect("fn end")];
        assert!(body.contains("cmd_spawn("), "one plan builder: {body}");
        assert!(body.contains("dry_run: true"), "plan is a dry run: {body}");
        assert!(body.contains("json: true"), "plan prints JSON: {body}");
        assert!(
            !body.contains("Plan::new"),
            "never a second plan builder: {body}"
        );
    }

    /// A model patch on an argv-delivered seat emits exactly one Relaunch
    /// through the 5d interpreter, journaled before it runs.
    #[test]
    fn apply_model_patch_emits_one_relaunch_through_the_interpreter() {
        let rows = vec![SeatRow {
            pin: "Reviewer".into(),
            name: "reviewer-1".into(),
            role: "reviewer".into(),
            harness: "pi".into(),
            model: Some("old".into()),
            effort: None,
            mode: None,
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let desired = patched_desired(&rows, &["reviewer.model=new".into()], &[]).expect("patches");
        let live: Vec<LiveSeat> = rows
            .iter()
            .map(|r| LiveSeat {
                pin: r.pin.clone(),
                role: r.role.clone(),
                harness: r.harness.clone(),
                model: r.model.clone(),
                mode: None,
                effort: None,
                placement: SeatPlacement::Pane,
                idle: false,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();
        let effects = reconcile(&desired, &live, &|_| false, &ReconcilePolicy::default());
        assert_eq!(effects.len(), 1, "one effect: {effects:?}");
        assert!(
            matches!(&effects[0], SeatEffect::Relaunch { pin, .. } if pin == "Reviewer"),
            "argv model relaunches: {effects:?}"
        );

        let fake = Fake::default();
        let mut ports = fake.clone();
        let mut j = MemJournal::default();
        let rep = dispatch_effects(&mut j, &mut ports, &effects);
        assert_eq!(rep.ran, 1, "one command ran: {rep:?}");
        assert_eq!(fake.0.lock().unwrap().seats.len(), 1, "one seat call");
        assert!(
            j.rows
                .keys()
                .any(|k| k.starts_with("seat:relaunch:Reviewer"))
        );
    }

    /// An env-pinned model patch respawns (never relaunches), and the respawn
    /// is one `Seat` command carrying the target binding.
    #[test]
    fn apply_env_model_patch_emits_one_respawn_with_the_target() {
        let rows = vec![SeatRow {
            pin: "Coder".into(),
            name: "coder-1".into(),
            role: "coder".into(),
            harness: "opencode".into(),
            model: Some("opencode/old".into()),
            effort: None,
            mode: None,
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let desired =
            patched_desired(&rows, &["coder.model=opencode/new".into()], &[]).expect("patches");
        let live: Vec<LiveSeat> = rows
            .iter()
            .map(|r| LiveSeat {
                pin: r.pin.clone(),
                role: r.role.clone(),
                harness: r.harness.clone(),
                model: r.model.clone(),
                mode: None,
                effort: None,
                placement: SeatPlacement::Pane,
                idle: false,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();
        let by_env = |h: &str| h == "opencode";
        let effects = reconcile(&desired, &live, &by_env, &ReconcilePolicy::default());
        let SeatEffect::Respawn { pin, to, .. } = &effects[0] else {
            panic!("env model respawns: {effects:?}");
        };
        assert_eq!(pin, "Coder");
        assert_eq!(
            to.as_ref().and_then(|t| t.model.clone()).as_deref(),
            Some("opencode/new"),
            "the effect carries the target binding"
        );

        let fake = Fake::default();
        let mut ports = fake.clone();
        let mut j = MemJournal::default();
        let rep = dispatch_effects(&mut j, &mut ports, &effects);
        assert_eq!(rep.ran, 1, "one command ran");
        assert_eq!(fake.0.lock().unwrap().seats, effects);
    }

    /// An unknown seat names the live ones instead of silently doing nothing.
    #[test]
    fn apply_unknown_seat_names_the_live_ones() {
        let rows = vec![SeatRow {
            pin: "Reviewer".into(),
            name: "reviewer-1".into(),
            role: "reviewer".into(),
            harness: "pi".into(),
            model: None,
            effort: None,
            mode: None,
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let err =
            patched_desired(&rows, &["nobody.model=x".into()], &[]).expect_err("unknown seat");
        assert!(err.to_string().contains("Reviewer"), "{err}");
        assert!(err.to_string().contains("reviewer"), "{err}");
    }

    /// `status` lists live seats with the plane's status and reads a missing
    /// pane as `gone`.
    #[test]
    fn status_lists_live_seats_with_status() {
        let registry = vec![
            entry("pane-1", "Alpha", "alpha", "coder", "pi", "m1"),
            entry(
                "pane-2",
                "Beta",
                "beta",
                "reviewer",
                "opencode",
                "opencode/m2",
            ),
        ];
        let agents = vec![("pane-1".to_string(), "working".to_string())];
        let rows = seat_rows(&registry, &agents, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pin, "Alpha");
        assert_eq!(rows[0].status, "working");
        assert_eq!(rows[1].pin, "Beta");
        assert_eq!(rows[1].status, "gone");
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);

    #[derive(Default)]
    struct FakeState {
        seats: Vec<SeatEffect>,
    }

    impl Ports for Fake {
        fn observe(
            &mut self,
            _model: &crate::supervisor_program::SupModel,
            _now_ms: u64,
        ) -> Vec<crate::supervisor_program::SupMsg> {
            Vec::new()
        }
        fn relaunch(
            &mut self,
            _child: &str,
            _generation: u64,
            _kind: crate::supervisor_program::ChildKind,
        ) -> Result<(), String> {
            Ok(())
        }
        fn exit(&mut self, _child: &str, _generation: u64) -> Result<(), String> {
            Ok(())
        }
        fn append_steer(&mut self, _s: &crate::supervisor_host::SteerSend) -> Result<u64, String> {
            Ok(0)
        }
        fn assign(&mut self, _a: &crate::supervisor_host::AssignSend) -> Result<u64, String> {
            Ok(0)
        }
        fn escalate(&mut self, _n: u64, _reason: &str, _evidence: &str) -> Result<(), String> {
            Ok(())
        }
        fn park(&mut self, _child: &str, _incarnations: u32) -> Result<(), String> {
            Ok(())
        }
        fn page(&mut self, _kind: &str, _pin: &str, _line: &str) -> Result<(), String> {
            Ok(())
        }
        fn reject(&mut self, _n: u64, _cause: &str, _child: &str) -> Result<(), String> {
            Ok(())
        }
        fn seat(&mut self, effect: &SeatEffect) -> Result<(), String> {
            self.0.lock().unwrap().seats.push(effect.clone());
            Ok(())
        }
    }

    /// `--count role=+1` emits exactly one Spawn through the reconciler.
    #[test]
    fn apply_count_plus_one_emits_one_spawn() {
        let rows = vec![SeatRow {
            pin: "Coder".into(),
            name: "coder-1".into(),
            role: "coder".into(),
            harness: "pi".into(),
            model: Some("m".into()),
            effort: None,
            mode: None,
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let desired = patched_desired(&rows, &[], &["coder=+1".into()]).expect("counts");
        let live: Vec<LiveSeat> = rows
            .iter()
            .map(|r| LiveSeat {
                pin: r.pin.clone(),
                role: r.role.clone(),
                harness: r.harness.clone(),
                model: r.model.clone(),
                mode: r.mode.clone(),
                effort: r.effort.clone(),
                placement: SeatPlacement::Pane,
                idle: false,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();
        let effects: Vec<SeatEffect> =
            reconcile(&desired, &live, &|_| false, &ReconcilePolicy::default())
                .into_iter()
                .filter(|e| !matches!(e, SeatEffect::Keep { .. }))
                .collect();
        assert_eq!(effects.len(), 1, "one non-Keep effect: {effects:?}");
        assert!(
            matches!(&effects[0], SeatEffect::Spawn { role, count } if role.as_str() == "coder" && *count == 1),
            "count grows the role: {effects:?}"
        );
    }

    /// The interpreter adds exactly one seat within crew.max.
    #[test]
    fn spawn_seats_adds_one_within_cap() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let mux = FakeMux::default();
        crate::supervisor_host::spawn_seats(&mux, &cfg, "demo", "coder", 1)
            .expect("one seat within cap");
        let rows = crate::state::load_spawn_registry("demo");
        assert_eq!(rows.len(), 1, "one seat recorded: {rows:?}");
        assert_eq!(rows[0].profile.as_deref(), Some("coder"));
        assert_eq!(mux.splits.load(Ordering::SeqCst), 1, "one pane split");
        assert_eq!(mux.agents.lock().unwrap().len(), 1, "one agent started");
    }

    /// Over `crew.max` is a typed refusal before any pane effect.
    #[test]
    fn spawn_seats_over_cap_is_a_typed_refusal() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let cap = cfg.crew_max() as usize;
        let mut rows = Vec::new();
        for i in 0..cap {
            rows.push(crate::state::SpawnEntry::profiled(
                format!("wA:p{i}"),
                format!("coder-{i}"),
                "coder",
                "pi",
                None,
            ));
        }
        crate::state::save_spawn_registry("demo", &rows).expect("save registry");
        let mux = FakeMux::default();
        let err = crate::supervisor_host::spawn_seats(&mux, &cfg, "demo", "coder", 1)
            .expect_err("over cap refuses");
        assert!(err.contains("crew.max"), "{err}");
        assert_eq!(mux.splits.load(Ordering::SeqCst), 0, "no pane effect");
    }

    /// A role-only patch with context:warm emits one InjectRole and the
    /// interpreter delivers exactly one prompt turn.
    #[test]
    fn inject_role_delivers_one_turn() {
        let _g = lock_env();
        let (_cfg, _dir) = hermetic();
        let mut entry = crate::state::SpawnEntry::profiled(
            "wA:p1",
            "crew-1",
            "coder",
            "pi",
            Some("Coder".into()),
        );
        entry.model = Some("m".into());
        crate::state::save_spawn_registry("demo", &[entry]).expect("save registry");
        let mux = FakeMux::default();
        crate::supervisor_host::inject_role_into(&mux, "demo", "Coder", "planner")
            .expect("inject ok");
        let prompts = mux.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1, "one turn: {prompts:?}");
        assert_eq!(prompts[0].0, "wA:p1");
        assert!(prompts[0].1.contains("planner"), "{}", prompts[0].1);
    }

    /// A review seat is context:fresh by rule and never receives InjectRole.
    #[test]
    fn inject_role_never_reaches_a_review_seat() {
        let _g = lock_env();
        let (_cfg, _dir) = hermetic();
        let entry = crate::state::SpawnEntry::profiled(
            "wA:p1",
            "crew-1",
            "reviewer",
            "pi",
            Some("Reviewer".into()),
        );
        crate::state::save_spawn_registry("demo", &[entry]).expect("save registry");
        let mux = FakeMux::default();
        let err = crate::supervisor_host::inject_role_into(&mux, "demo", "Reviewer", "reviewer")
            .expect_err("review seats refuse injection");
        assert!(err.contains("context:fresh"), "{err}");
        assert!(mux.prompts.lock().unwrap().is_empty(), "no turn sent");
    }

    /// Effort/mode persisted on the registry row -> the next reconcile is
    /// Keep (no phantom diff).
    #[test]
    fn effort_persisted_second_reconcile_is_keep() {
        let mut entry = crate::state::SpawnEntry::profiled(
            "wA:p1",
            "crew-1",
            "coder",
            "pi",
            Some("Coder".into()),
        );
        entry.model = Some("m".into());
        entry.effort = Some("high".into());
        entry.mode = Some("workspace-write".into());
        let rows = seat_rows(&[entry], &[], None);
        assert_eq!(rows[0].effort.as_deref(), Some("high"));
        let desired = patched_desired(&rows, &[], &[]).expect("no patches");
        let live: Vec<LiveSeat> = rows
            .iter()
            .map(|r| LiveSeat {
                pin: r.pin.clone(),
                role: r.role.clone(),
                harness: r.harness.clone(),
                model: r.model.clone(),
                mode: r.mode.clone(),
                effort: r.effort.clone(),
                placement: SeatPlacement::Pane,
                idle: false,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();
        let effects = reconcile(&desired, &live, &|_| false, &ReconcilePolicy::default());
        assert!(
            matches!(effects.as_slice(), [SeatEffect::Keep { .. }]),
            "persisted effort/mode reconcile to Keep: {effects:?}"
        );
    }

    /// Old registry JSON without effort/mode still loads (serde default).
    #[test]
    fn old_registry_json_without_effort_mode_loads() {
        let old =
            r#"[{"pane_id":"wA:p1","name":"crew-1","profile":"coder","harness":"pi","model":"m"}]"#;
        let rows: Vec<crate::state::SpawnEntry> =
            serde_json::from_str(old).expect("old JSON loads");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].effort, None);
        assert_eq!(rows[0].mode, None);
    }

    /// ADR-0022 D7: the captain/operator seat is never a reconcile target,
    /// so with `idle_retire_after` set no Seat command can close it.
    #[test]
    fn captain_seat_is_never_a_reconcile_target() {
        let entry = crate::state::SpawnEntry::profiled(
            "wA:p1",
            "captain-1",
            "captain",
            "pi",
            Some("Captain".into()),
        );
        let agents = vec![agent("captain-1", "wA:p1", "wA")];
        let mut idle = std::collections::BTreeMap::new();
        let (desired, live) = crate::supervisor_host::seat_observation(
            std::slice::from_ref(&entry),
            &agents,
            Some("Captain"),
            &mut idle,
            1_000,
        );
        assert!(
            desired.is_empty(),
            "captain is not a desired seat: {desired:?}"
        );
        assert!(live.is_empty(), "captain is not a live seat: {live:?}");
        let policy = ReconcilePolicy {
            idle_retire_after_ms: Some(0),
            now_ms: 2_000,
        };
        let effects = reconcile(&desired, &live, &|_| false, &policy);
        assert!(
            effects.is_empty(),
            "no Seat cmd for the captain: {effects:?}"
        );
        assert!(
            seat_rows(
                &[entry],
                &[("wA:p1".into(), "idle".into())],
                Some("Captain")
            )
            .is_empty(),
            "swarm status never lists the captain as a crew seat"
        );
    }

    /// A gone row is its own state: not a live seat, never a retire target.
    #[test]
    fn gone_row_is_not_a_live_seat() {
        let entry = crate::state::SpawnEntry::profiled(
            "wA:p9",
            "gone-1",
            "coder",
            "pi",
            Some("Gone".into()),
        );
        let mut idle = std::collections::BTreeMap::new();
        let (desired, live) =
            crate::supervisor_host::seat_observation(&[entry], &[], None, &mut idle, 1_000);
        assert!(desired.is_empty(), "gone is not desired: {desired:?}");
        assert!(live.is_empty(), "gone is not live: {live:?}");
        let effects = reconcile(
            &desired,
            &live,
            &|_| false,
            &ReconcilePolicy {
                idle_retire_after_ms: Some(0),
                now_ms: 2_000,
            },
        );
        assert!(effects.is_empty(), "gone never retires: {effects:?}");
    }

    /// The captain role resolver reads the alias (the role source), never a
    /// name literal.
    #[test]
    fn captain_pin_reads_the_alias() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        crate::state::save_captain_alias("demo", "Captain").expect("save alias");
        let key = crate::project_dir_str(&cfg, "demo").expect("project key");
        assert_eq!(
            crate::state::captain_pin("demo", &key).as_deref(),
            Some("Captain"),
            "the captain pin comes from the alias file"
        );
    }

    /// `add --oc` rows persist harness and model, so status/reconcile see them.
    #[test]
    fn add_oc_rows_carry_harness_and_model() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let reg = crate::models::ModelRegistry::load_routing().ok();
        crate::cmd::spawn::record_added_seat(
            &cfg,
            "demo",
            "demo-oc-1",
            "wA:p1",
            "opencode",
            Some("BlueLake"),
            reg.as_ref(),
        );
        // `record_spawn_entry` keys the absolute project dir; read it back the
        // same way (the hermetic cfg root and the ambient `Config::load` root
        // differ in tests).
        let key = crate::project_dir_str(&cfg, "demo").expect("project key");
        let rows = crate::state::load_spawn_registry(&key);
        assert_eq!(rows.len(), 1, "one add row: {rows:?}");
        assert_eq!(rows[0].harness.as_deref(), Some("opencode"));
        assert!(
            rows[0].model.is_some(),
            "opencode model persisted: {rows:?}"
        );
        let view = seat_rows(&rows, &[("wA:p1".into(), "idle".into())], None);
        assert_eq!(view[0].harness.as_str(), "opencode");
        assert!(view[0].model.is_some(), "status shows the model");
    }

    // ---- bd-rilvn: flywheel retire ----

    fn retire_registry(pins: &[(&str, &str, &str)]) {
        let rows: Vec<SpawnEntry> = pins
            .iter()
            .map(|(pane, pin, name)| {
                SpawnEntry::profiled(*pane, *name, "coder", "pi", Some((*pin).to_string()))
            })
            .collect();
        crate::state::save_spawn_registry("demo", &rows).expect("save registry");
    }

    /// One journaled `SeatEffect::Retire` for exactly that pin, its lease
    /// released once, its row deregistered, the sibling untouched.
    #[test]
    fn retire_dispatches_one_retire_and_releases_only_that_pin() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        retire_registry(&[("wA:p1", "Alpha", "a-1"), ("wA:p2", "Beta", "b-1")]);
        let mux = FakeMux::default();
        let mut ports = Fake::default();
        let mut journal = MemJournal::default();
        let mut released: Vec<String> = Vec::new();
        let mut release = |pin: &str| {
            released.push(pin.to_string());
            Ok(1)
        };
        let out = retire_with(
            &mux,
            &cfg,
            "demo",
            &["Alpha".into()],
            false,
            &mut journal,
            &mut ports,
            &mut release,
        )
        .expect("retire ok");
        assert_eq!(out.released, 1, "one lease released");
        assert_eq!(released, vec!["Alpha".to_string()], "only that pin");
        let seats = ports.0.lock().unwrap().seats.clone();
        assert_eq!(seats.len(), 1, "one seat command: {seats:?}");
        assert!(
            matches!(&seats[0], SeatEffect::Retire { pins } if pins == &vec!["Alpha".to_string()]),
            "the one seat path is Retire: {seats:?}"
        );
        let rows = crate::state::load_spawn_registry("demo");
        assert_eq!(rows.len(), 1, "the sibling stays: {rows:?}");
        assert_eq!(rows[0].mail_identity.as_deref(), Some("Beta"));
    }

    /// A working pin is refused without `--force-working`, mirroring kill.
    #[test]
    fn retire_refuses_working_without_flag() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        retire_registry(&[("wA:p1", "Alpha", "a-1")]);
        let mux = FakeMux::default();
        mux.agents.lock().unwrap().push(agent_with_status(
            "a-1",
            "wA:p1",
            "wA",
            AgentStatus::Working,
        ));
        let mut ports = Fake::default();
        let mut journal = MemJournal::default();
        let mut release = |_: &str| Ok(0u64);
        let err = retire_with(
            &mux,
            &cfg,
            "demo",
            &["Alpha".into()],
            false,
            &mut journal,
            &mut ports,
            &mut release,
        )
        .expect_err("working pin refused");
        assert!(err.to_string().contains("--force-working"), "{err}");
        assert!(ports.0.lock().unwrap().seats.is_empty(), "no effect ran");
        assert!(
            crate::state::load_spawn_registry("demo").len() == 1,
            "the row is untouched"
        );
    }

    /// The captain/operator seat is refused by role. The alias is seeded
    /// EXACTLY as `toron contact captain --project <slug> --pin X` writes it
    /// (the row is keyed by the SLUG), while the lookup passes a path key —
    /// the live mismatch that made bd-8fiwv fail.
    #[test]
    fn retire_refuses_the_captain_seat() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        let mb = crate::mail::comms_mailbox().expect("mailbox");
        let rt = crate::mail::runtime().expect("runtime");
        // The same upsert `toron contact captain --project <slug>` writes:
        // the row is keyed by the SLUG and says SlateHarbor.
        rt.block_on(mb.set_captain_alias("demo", "SlateHarbor"))
            .expect("set the captain row keyed by the slug");
        // A DISAGREEING stale flywheel mirror file (the live split-brain: it
        // said WindyCreek since Nov 7). toron owns identity; the file must
        // never win.
        let stale = crate::state::captain_alias_file("demo");
        std::fs::create_dir_all(stale.parent().expect("parent")).expect("dir");
        std::fs::write(&stale, r#"{"pin":"WindyCreek"}"#).expect("stale mirror");
        // The live lookup passes a PATH-form key; the row is keyed by the slug.
        assert_eq!(
            crate::state::captain_pin("demo", "/tmp/captainkeytest/demo").as_deref(),
            Some("SlateHarbor"),
            "toron's fold wins over the stale flywheel file"
        );
        assert_eq!(
            crate::state::load_captain_alias("demo").as_deref(),
            Some("SlateHarbor"),
            "the stale file is never read as the captain value"
        );
        retire_registry(&[("wA:p1", "SlateHarbor", "c-1")]);
        let mux = FakeMux::default();
        let mut ports = Fake::default();
        let mut journal = MemJournal::default();
        let mut release = |_: &str| Ok(0u64);
        let err = retire_with(
            &mux,
            &cfg,
            "demo",
            &["SlateHarbor".into()],
            true,
            &mut journal,
            &mut ports,
            &mut release,
        )
        .expect_err("captain refused");
        assert!(err.to_string().contains("captain"), "{err}");
        assert!(ports.0.lock().unwrap().seats.is_empty());
    }

    /// The pane effect closes exactly that pane, never a sibling's.
    #[test]
    fn close_panes_for_pins_closes_exactly_that_pane() {
        let _g = lock_env();
        let (_cfg, _dir) = hermetic();
        retire_registry(&[("wA:p1", "Alpha", "a-1"), ("wA:p2", "Beta", "b-1")]);
        let mux = FakeMux::default();
        let closed = crate::supervisor_host::close_panes_for_pins(&mux, "demo", &["Alpha".into()])
            .expect("close ok");
        assert_eq!(closed, vec![("Alpha".to_string(), "wA:p1".to_string())]);
        assert_eq!(
            mux.closed.lock().unwrap().clone(),
            vec!["wA:p1".to_string()]
        );
    }

    /// bd-0uzrq.21: after a host blackout the registry still names panes the
    /// plane no longer has. A close on an absent pane errors (`pane_not_found`),
    /// which used to strand the pin before the lease release + deregistration.
    /// An absent pane counts as closed: no close call, and retire completes.
    #[test]
    fn retire_gone_seat_counts_as_closed_and_retire_completes() {
        let _g = lock_env();
        let (cfg, _dir) = hermetic();
        retire_registry(&[("wA:p1", "Alpha", "a-1"), ("wA:p9", "Beta", "b-1")]);
        let mux = FakeMux::default();
        // The plane lists wA:p1 only; Beta's wA:p9 died with the session.
        let closed = crate::supervisor_host::close_panes_for_pins(&mux, "demo", &["Beta".into()])
            .expect("a pane the plane no longer lists is already closed");
        assert_eq!(closed, vec![("Beta".to_string(), "wA:p9".to_string())]);
        assert!(
            mux.closed.lock().unwrap().is_empty(),
            "no close RPC for an absent pane"
        );

        let mut ports = Fake::default();
        let mut journal = MemJournal::default();
        let mut released: Vec<String> = Vec::new();
        let mut release = |pin: &str| {
            released.push(pin.to_string());
            Ok(1u64)
        };
        let out = retire_with(
            &mux,
            &cfg,
            "demo",
            &["Beta".into()],
            true,
            &mut journal,
            &mut ports,
            &mut release,
        )
        .expect("retire a gone seat");
        assert_eq!(out.closed, vec![("Beta".to_string(), "wA:p9".to_string())]);
        assert_eq!(released, vec!["Beta".to_string()], "leases are released");
        assert!(
            crate::state::load_spawn_registry("demo")
                .iter()
                .all(|e| e.mail_identity.as_deref() != Some("Beta")),
            "the row is deregistered"
        );
    }

    /// Unused-import guard: `execute`/`RoundReport`/`SupCmd` are the 5d
    /// interpreter surface this module proves against.
    #[test]
    fn interpreter_surface_is_reachable() {
        let fake = Fake::default();
        let mut ports = fake.clone();
        let mut j = MemJournal::default();
        let mut rep = RoundReport::default();
        let _ = execute(
            &mut j,
            &mut ports,
            &SupCmd::Seat {
                effect: SeatEffect::Keep {
                    pin: "x".to_string(),
                },
                key: "seat:keep:x".to_string(),
            },
            &mut rep,
        );
    }
}

/// bd-6setu.2: the binder's view of a live seat. `seat_rows` is the one
/// registry+plane fold (`swarm status`), so the dispatch cannot see a
/// different fleet than the operator does.
pub(crate) fn live_seat(row: &SeatRow) -> crate::seat_binder::LiveSeat {
    crate::seat_binder::LiveSeat {
        pin: row.pin.clone(),
        role: row.role.as_str().to_string().into(),
        harness: row.harness.as_str().to_string().into(),
        model: row.model.clone(),
        mode: row.mode.clone(),
        effort: row.effort.clone(),
        placement: crate::seat_binder::SeatPlacement::Pane,
        idle: matches!(row.status.as_str(), "idle" | "done" | "blocked"),
        fresh_session: false,
        idle_since_ms: None,
    }
}

#[cfg(test)]
mod live_seat_tests {
    use super::*;

    /// The mapping keeps the identity the binder answers with and reads the
    /// injectable statuses as idle (a working pane is not borrowable).
    #[test]
    fn a_seat_row_maps_to_the_binders_live_seat() {
        let row = SeatRow {
            pin: "SlateHarbor".into(),
            name: "t".into(),
            role: "reviewer".to_string().into(),
            harness: "opencode".to_string().into(),
            model: Some("m".into()),
            effort: Some("high".into()),
            mode: Some("read-only".into()),
            pane_id: "block:1".into(),
            status: "idle".into(),
        };
        let live = live_seat(&row);
        assert_eq!(live.pin, "SlateHarbor");
        assert_eq!(live.role.as_str(), "reviewer");
        assert!(live.idle, "idle is borrowable");
        assert_eq!(live.model.as_deref(), Some("m"));
        let busy = SeatRow {
            status: "working".into(),
            ..row
        };
        assert!(!live_seat(&busy).idle, "a working seat is never borrowed");
    }
}
