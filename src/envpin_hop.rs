//! Env-pin reroute hop (ADR-0022 D7 acceptance #5), moved out of `main.rs`
//! (bd-6143s). One job: decide the hop from the reconciler's verdict, ring a
//! journaled `RebindIntent`, respawn the pinned seat, and close the old pane
//! only after the new one launches.

use anyhow::anyhow;
use std::time::Duration;

use crate::config::Config;
use crate::hc::{AgentInfo, AgentStatus};
use crate::{backend, hc, project_dir, utf8_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RerouteEnvPin {
    /// The pane's existing env pin already matches. Proceed.
    AlreadyPinned,
    /// The reroute wants a different model than the pane is pinned to.
    ModelChanged,
}

/// Pure so the policy is testable without a pane. `current_model` is the model
/// the pane was pinned with at split time (None when the pane carried no pin).
pub(crate) fn reroute_env_pin_verdict(
    current_model: Option<&str>,
    reroute_pin: &str,
) -> RerouteEnvPin {
    match current_model {
        Some(m) if m == reroute_pin => RerouteEnvPin::AlreadyPinned,
        _ => RerouteEnvPin::ModelChanged,
    }
}

/// Env-pin reroute hop (ADR-0022 D7 acceptance #5): respawn with the new
/// env pin when the reconciler says Respawn, keeping the same agent name
/// (mail identity and toron bead reservations carry over). The old pane
/// closes only after the new one launches; a failed launch never closes
/// it. Journal first: exactly one RebindIntent (Prepared → InFlight →
/// Settled) via commit_rebind_opt, gated by allowed_from_failure (no hop
/// on Timeout/Unknown/empty evidence) and the one-hop flag.
#[derive(Debug, Clone)]
pub(crate) struct EnvpinHop {
    pub project: String,
    pub agent: String,
    pub old_pane: String,
    pub next_harness: String,
    pub next_args: Vec<String>,
    pub next_env: std::collections::BTreeMap<String, String>,
    pub next_profile: String,
    pub next_model: String,
    pub from_profile: String,
    pub from_model: String,
    pub from_harness: String,
    pub kind: crate::failure::FailureKind,
    pub evidence: String,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub(crate) enum EnvpinHopOutcome {
    Hopped { agent: Box<AgentInfo> },
    AlreadySatisfied,
    SuppressedGate,
    AlreadyRerouted,
}

/// Decide with the reconciler, then interpret: Respawn hops, Keep means
/// the hop is already satisfied. Anything else fails loud — the reroute
/// loop only reaches here for an env-pinned next model, so a different
/// verdict is a bug, never a silent downgrade.
pub(crate) fn envpin_respin_hop(
    backend: &dyn backend::MultiplexerBackend,
    respawn: &dyn Fn(&str) -> anyhow::Result<AgentInfo>,
    hop: &EnvpinHop,
    already_rerouted: bool,
) -> anyhow::Result<EnvpinHopOutcome> {
    if already_rerouted {
        return Ok(EnvpinHopOutcome::AlreadyRerouted);
    }
    if !crate::agent_ref::RebindIntent::allowed_from_failure(hop.kind, &hop.evidence) {
        return Ok(EnvpinHopOutcome::SuppressedGate);
    }
    // The decision consults only harness+model (same role/placement and
    // empty effort/mode on both sides so those never fire): that matches
    // today's verdict domain exactly, so a same-model pane keeps working
    // in place and a different env-pinned model respawns.
    let live = crate::seat_binder::LiveSeat {
        pin: hop.agent.clone(),
        role: "coder".into(),
        harness: hop.from_harness.clone().into(),
        model: Some(hop.from_model.clone()),
        mode: None,
        effort: None,
        placement: crate::seat_binder::SeatPlacement::Pane,
        idle: false,
        fresh_session: false,
        idle_since_ms: None,
    };
    let want = crate::seat_reconcile::DesiredSeat {
        pin: Some(hop.agent.clone()),
        role: "coder".into(),
        harness: hop.next_harness.clone().into(),
        model: Some(hop.next_model.clone()),
        effort: None,
        mode: None,
        placement: crate::seat_binder::SeatPlacement::Pane,
        context_warm_allowed: true,
    };
    let effect = crate::seat_reconcile::envpin_reroute_effect(&live, &want, &|harness: &str| {
        matches!(
            crate::headless::interactive_model_plan(harness, "", None),
            crate::headless::InteractiveModelPlan::PinnedByEnv { .. }
        )
    });
    match effect {
        crate::seat_reconcile::SeatEffect::Keep { .. } => {
            return Ok(EnvpinHopOutcome::AlreadySatisfied);
        }
        crate::seat_reconcile::SeatEffect::Respawn { .. } => {}
        other => anyhow::bail!(
            "envpin hop: reconciler verdict {other:?} is neither Keep nor Respawn — failing loud \
             instead of reporting the hop satisfied while the pane still runs the old model"
        ),
    }
    let agent = crate::agent_ref::AgentRef::new(&hop.project, &hop.agent);
    let from = crate::agent_ref::AgentBinding {
        profile_name: hop.from_profile.clone(),
        model: hop.from_model.clone(),
        harness: hop.from_harness.clone(),
        location: crate::agent_ref::BindingLocation::Pane,
    };
    let to = crate::agent_ref::AgentBinding {
        profile_name: hop.next_profile.clone(),
        model: hop.next_model.clone(),
        harness: hop.next_harness.clone(),
        location: crate::agent_ref::BindingLocation::Pane,
    };
    let intent = crate::agent_ref::RebindIntent::prepare(
        agent,
        from,
        to,
        crate::agent_ref::RebindReason::ProfileFallback {
            kind: hop.kind.to_string(),
            evidence: hop.evidence.clone(),
        },
        hop.attempt,
    );
    let launched = crate::agent_ref::commit_rebind_opt(&hop.project, intent, |_| {
        let panes = backend.pane_list(None).map_err(anyhow::Error::from)?;
        let anchor = panes
            .iter()
            .find(|p| p.pane_id != hop.old_pane)
            .map(|p| p.pane_id.clone())
            .unwrap_or_else(|| hop.old_pane.clone());
        let info = respawn(&anchor)?;
        backend
            .pane_close(&hop.old_pane)
            .map_err(anyhow::Error::from)?;
        Ok(Some(info))
    })?;
    match launched {
        Some(agent) => {
            eprintln!(
                "flywheel: pane reroute {} -> {} (profile fallback, respawned with env pin; old pane closed after launch)",
                hop.from_profile, hop.next_profile
            );
            Ok(EnvpinHopOutcome::Hopped {
                agent: Box::new(agent),
            })
        }
        None => anyhow::bail!("envpin hop produced no agent"),
    }
}

/// Split refused: this backend cannot carry a pane env pin, so an env-pin
/// hop through it would launch the new pane without the pin — the new
/// process would run the previous model while the journal records a
/// RebindIntent to the new one. The old REFUSED message existed to prevent
/// exactly this silent downgrade.
#[derive(Debug)]
pub(crate) struct EnvPinSplitUnsupported {
    backend: String,
}

impl std::fmt::Display for EnvPinSplitUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} cannot carry a pane env pin; refusing a silent model downgrade",
            self.backend
        )
    }
}

impl std::error::Error for EnvPinSplitUnsupported {}

/// The backend reason when a hop died on [`EnvPinSplitUnsupported`], so a
/// reroute site can print today's refusal plus the reason instead of
/// failing loud on a broken hop.
pub(crate) fn envpin_split_refusal(err: &anyhow::Error) -> Option<String> {
    err.downcast_ref::<EnvPinSplitUnsupported>()
        .map(|unsupported| unsupported.to_string())
}

/// Pane split for the env-pin hop: hc backends split with the new env pin
/// (a pane's env is fixed at split) using the workspace dir and socket from
/// `cfg` and `project`. Anything else fails closed with
/// [`EnvPinSplitUnsupported`] before anything launches — a trait split
/// carries no env map, and hc without cfg/project cannot locate the
/// workspace or daemon. [`envpin_hop_respawn`] routes hc here; a backend with
/// another launch path never calls it.
pub(crate) fn envpin_hop_split(
    cfg: Option<&Config>,
    project: Option<&str>,
    backend: &dyn backend::MultiplexerBackend,
    anchor: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<String> {
    if backend.name() == "hc"
        && let (Some(cfg), Some(project)) = (cfg, project)
    {
        let dir = project_dir(cfg, project);
        let cwd = utf8_path(&dir, "workspace dir")?;
        let c = match hc::Client::connect(Some(&cfg.socket_path())) {
            Ok(c) => c,
            Err(e) => return Err(anyhow::anyhow!("hc connect for env-pin hop: {e}")),
        };
        return Ok(hc::pane_split(&c, anchor, "down", Some(&cwd), Some(env))?.pane_id);
    }
    Err(EnvPinSplitUnsupported {
        backend: backend.name().to_string(),
    }
    .into())
}

/// Respawn the env-pinned hop on a fresh pane under `anchor`. hc pins the
/// model at split (a pane's env is fixed there); rex takes a plain split and
/// delivers the same pin as an `env` command prefix through the spawn path
/// ([`crate::pane::rex_env_command`]) instead of refusing. The arm follows the
/// ANCHOR's id, not the backend name: the live rex backends call themselves
/// `superlogical` and `auto` (a per-target router), so a name match would
/// leave production refusing. A backend with neither path stays refused by
/// [`EnvPinSplitUnsupported`].
pub(crate) fn envpin_hop_respawn(
    cfg: Option<&Config>,
    project: &str,
    backend: &dyn backend::MultiplexerBackend,
    hop: &EnvpinHop,
    anchor: &str,
) -> anyhow::Result<AgentInfo> {
    if crate::inbox_ports::is_rex_block(anchor) {
        return envpin_hop_rex_launch(cfg, project, backend, hop, anchor);
    }
    match backend.name() {
        "hc" => {
            let pane = envpin_hop_split(cfg, Some(project), backend, anchor, &hop.next_env)?;
            backend
                .agent_start(&hop.agent, &hop.next_harness, &pane, &hop.next_args)
                .map_err(anyhow::Error::from)
        }
        unsupported => Err(EnvPinSplitUnsupported {
            backend: unsupported.to_string(),
        }
        .into()),
    }
}

/// Rex arm of [`envpin_hop_respawn`]: split plain, then launch through the
/// same env-prefixed `pane_run` spawn uses, so the new pane carries the model
/// pin (and the agent's mail identity) without one.
pub(crate) fn envpin_hop_rex_launch(
    cfg: Option<&Config>,
    project: &str,
    backend: &dyn backend::MultiplexerBackend,
    hop: &EnvpinHop,
    anchor: &str,
) -> anyhow::Result<AgentInfo> {
    let Some(cfg) = cfg else {
        return Err(EnvPinSplitUnsupported {
            backend: backend.name().to_string(),
        }
        .into());
    };
    let dir = project_dir(cfg, project);
    let split = backend
        .pane_split(anchor, "down", Some(&utf8_path(&dir, "workspace dir")?))
        .map_err(anyhow::Error::from)?;
    crate::pane::wait_rex_shell_ready(
        backend,
        &split.pane_id,
        &hop.agent,
        &split.pane_id,
        Duration::from_secs(15),
    )?;
    let identity = envpin_hop_mail_identity(project, &hop.agent);
    let mut pane_env =
        crate::pane::pane_env_for_launch(cfg, identity.as_deref(), Some(&hop.next_model))?;
    pane_env.extend(hop.next_env.clone());
    let command = crate::pane::rex_env_command(&pane_env, &hop.next_harness, &hop.next_args);
    if let Err(e) = backend.pane_run(&split.pane_id, &command) {
        let _ = backend.pane_close(&split.pane_id);
        return Err(anyhow::anyhow!(
            "rex env-pin hop run on {}: {e}",
            split.pane_id
        ));
    }
    if let Err(e) = backend.agent_rename(&split.pane_id, Some(&hop.agent)) {
        eprintln!(
            "flywheel: warn: rex env-pin hop rename {} → {} failed: {e}",
            split.pane_id, hop.agent
        );
    }
    backend
        .agent_wait(
            &split.pane_id,
            &[AgentStatus::Idle, AgentStatus::Working],
            Some(90_000),
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "rex env-pin hop launch on {} not confirmed: {e}",
                split.pane_id
            )
        })
}

/// The mail pin a respawned pane must keep carrying: the registry entry the
/// spawn recorded for this agent, never a fresh draw.
pub(crate) fn envpin_hop_mail_identity(project: &str, agent: &str) -> Option<String> {
    crate::state::load_spawn_registry(project)
        .iter()
        .find(|entry| entry.name == agent)
        .and_then(|entry| entry.mail_identity.clone())
        .filter(|identity| !identity.is_empty())
}
