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
        /// `<seat>.<field>=<value>`, field in harness|model|effort|mode.
        /// `<seat>` is a pin, a role, or a terminal name.
        #[arg(long, value_name = "SEAT.FIELD=VALUE", required = true)]
        patch: Vec<String>,
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
            json,
        } => cmd_swarm_apply(cfg, project, patch, *json),
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
    pub role: String,
    pub harness: String,
    pub model: Option<String>,
    pub pane_id: String,
    pub status: String,
}

/// Registry rows joined to the plane's agent status. Pure: the caller supplies
/// the agent facts. A row whose pane is not live reads `gone`.
pub(crate) fn seat_rows(registry: &[SpawnEntry], agents: &[(String, String)]) -> Vec<SeatRow> {
    registry
        .iter()
        .filter_map(|e| {
            let pin = e
                .mail_identity
                .clone()
                .filter(|p| !p.is_empty())
                .or_else(|| Some(e.name.clone()))?;
            let status = agents
                .iter()
                .find(|(pane, _)| pane == &e.pane_id)
                .map(|(_, status)| status.clone())
                .unwrap_or_else(|| "gone".to_string());
            Some(SeatRow {
                pin,
                name: e.name.clone(),
                role: e.profile.clone().unwrap_or_else(|| "crew".to_string()),
                harness: e.harness.clone().unwrap_or_default(),
                model: e.model.clone().or_else(|| e.model_pin.clone()),
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
    let rows = seat_rows(&registry, &agents);
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
    let rows = seat_rows(&registry, &agents);
    let desired = patched_desired(&rows, patches)?;
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

fn effect_json(effect: &SeatEffect) -> serde_json::Value {
    serde_json::to_value(effect).unwrap_or(serde_json::Value::Null)
}

fn effect_line(effect: &SeatEffect) -> String {
    match effect {
        SeatEffect::Relaunch { pin, fields, .. } => {
            format!("relaunch {pin} ({})", fields.join(","))
        }
        SeatEffect::Respawn { pin, fields, .. } => {
            format!("respawn {pin} ({})", fields.join(","))
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
fn patched_desired(rows: &[SeatRow], patches: &[String]) -> Result<Vec<DesiredSeat>> {
    let mut desired: Vec<DesiredSeat> = rows
        .iter()
        .map(|r| DesiredSeat {
            pin: Some(r.pin.clone()),
            role: r.role.clone(),
            harness: r.harness.clone(),
            model: r.model.clone(),
            effort: None,
            mode: None,
            placement: SeatPlacement::Pane,
            context_warm_allowed: true,
        })
        .collect();
    for spec in patches {
        let patch = crate::swarm_plan::parse_patch(spec)
            .map_err(|e| anyhow::anyhow!("bad --patch {spec:?}: {e}"))?;
        let matched: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.pin == patch.seat || r.role == patch.seat || r.name == patch.seat)
            .map(|(i, _)| i)
            .collect();
        if matched.is_empty() {
            let mut names: Vec<String> = rows
                .iter()
                .flat_map(|r| [r.pin.clone(), r.role.clone(), r.name.clone()])
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
                    seat.harness = patch.value.clone();
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
            }
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
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let desired = patched_desired(&rows, &["reviewer.model=new".into()]).expect("patches");
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
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let desired =
            patched_desired(&rows, &["coder.model=opencode/new".into()]).expect("patches");
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
            pane_id: "pane-1".into(),
            status: "idle".into(),
        }];
        let err = patched_desired(&rows, &["nobody.model=x".into()]).expect_err("unknown seat");
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
        let rows = seat_rows(&registry, &agents);
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
