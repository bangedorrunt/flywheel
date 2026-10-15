//! `flywheel doorbell` subcommand types (split from `main.rs`, bd-qbi8).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::config::Config;

/// Subcommands of `flywheel doorbell` (bd-durable-doorbell-lngf): the
/// supervised pane-wake layer surface.
#[derive(Subcommand)]
pub(crate) enum DoorbellCmd {
    /// per-workspace liveness: dispatcher up/down, last wake, unanswered
    /// pins. Lazily heals dead dispatchers while reporting.
    Status { project: String },
    /// start/attach dispatchers for every registered workspace that ever
    /// ran one (the crash-recovery entry point).
    Start { project: String },
    /// stop dispatchers (per workspace; `all` for every registered one)
    Stop {
        project: String,
        /// workspace id, or `all`
        #[arg(default_value = "all")]
        workspace: String,
    },
    /// re-inject the peer-aware handler + identity line into EXISTING panes
    /// (covers herdr pane-split additions and post-crash respawns).
    ReInject { project: String },
    /// batch captain-relevant wakes since the last digest (ADR-0010 D5).
    /// Folds tick from Default over the doorbell journal (same trust
    /// surface as `why`); urgent wakes never batch. Wedge alarm when the
    /// queue cannot drain.
    Afk {
        project: String,
        /// newest entries to show (bd-z5ut: the folded queue is unbounded by
        /// construction — an older remainder is summarized, never silently
        /// dropped)
        #[arg(long, default_value = "100")]
        limit: usize,
    },
    /// generalized digest (bd-asr2.11, D8): per-identity wake digest over
    /// any window — earliest first, unanswered escalated on top. `afk`
    /// stays as the captain-default alias. Roster-free (no arrival triple).
    Digest {
        project: String,
        /// identity to digest for (pin, subscriber tag, or npub); empty = all
        #[arg(long)]
        identity: Option<String>,
        /// only rows at/after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
        /// output format: text (default) or json
        #[arg(long, default_value = "text")]
        format: String,
    },
    /// why did (or didn't) a pin get rung? Replays the doorbell journal —
    /// GS-007 bd-yw4s: the trust surface, not log tailing. Folds tick from Default.
    Why {
        project: String,
        /// pin (adjective+noun) to explain (use with --corr/--seq for assignment replay)
        pin: String,
        /// max entries to show (newest last)
        #[arg(long, default_value = "20")]
        limit: usize,
        /// replay by corr id (assignment) — folds tick from Default, not grep
        #[arg(long)]
        corr: Option<String>,
        /// replay by seq — folds tick from Default up to this seq
        #[arg(long)]
        seq: Option<u64>,
        /// replay only rows tagged for this subscriber (bd-asr2.11);
        /// untagged legacy rows are excluded under a filter
        #[arg(long)]
        subscriber: Option<String>,
    },
    /// operator console inject (D8, bd-woha): journal a Fact row plus queue
    /// one pending wake — digest-visible with zero swarm. Same-second
    /// ratchet + persisted per-pin 5/30/3 budget. The console is the
    /// operator (local loopback invocation), so no separate token.
    Inject {
        project: String,
        /// pin (adjective+noun) to ring
        pin: String,
        /// wake line
        line: String,
        /// journal scope (default `manual` — console injections are their
        /// own scope, never mixed into a workspace journal)
        #[arg(long, default_value = "manual")]
        workspace: String,
    },
    /// subscribe an identity to wake delivery (D8, bd-woha): identity ×
    /// scope × sink × budget row in the subscription store. Cross-workspace
    /// scopes and non-herdr sinks require the operator (refused loudly).
    Subscribe {
        /// home workspace
        #[arg(long)]
        workspace: String,
        /// pin (adjective+noun) to subscribe
        #[arg(long)]
        pin: String,
        /// sink: herdr (default), webhook, dm (endpoint binds at the adapter)
        #[arg(long, default_value = "herdr")]
        sink: String,
        /// where this row's wake lands (bd-8ulu): the webhook endpoint URL,
        /// e.g. `http://127.0.0.1:18999/wake`. Per-row, so two subscribers
        /// can target two endpoints. The host must be on the daemon's
        /// `DOORBELL_WEBHOOK_ALLOWLIST` or the daemon never registers it
        /// (the wake then stays digest-visible).
        #[arg(long)]
        url: Option<String>,
        /// extra workspaces for an explicit cross-workspace scope
        /// (repeatable; operator-only)
        #[arg(long)]
        extra: Vec<String>,
        /// project context so the daemon can auto-host this scope
        /// (bd-2o1v); omitted = legacy row, never auto-hosted
        #[arg(long)]
        project: Option<String>,
    },
    /// nearby join: herdr panes ∪ registry pins ∪ reservations (bd-gpj0.2).
    /// One query instead of pane list + tokens + whois + git log.
    Roster {
        project: String,
        /// structured rows (default is the NEARBY: text view)
        #[arg(long)]
        json: bool,
    },
    /// one-command introduce: contact request + channel create + subscribe
    /// (bd-gpj0.3). Idempotent. `--dry-run` prints the plan.
    Introduce {
        project: String,
        /// peer pin to introduce
        #[arg(long)]
        pin: String,
        /// our pin (default: AGENT_NAME / TORON_AGENT / FLYWHEEL_MAIL_AS)
        #[arg(long = "as")]
        as_pin: Option<String>,
        /// channel id (default: intro-<pin-lowercase>)
        #[arg(long)]
        channel: Option<String>,
        /// doorbell workspace (default: flywheel-<project>)
        #[arg(long)]
        workspace: Option<String>,
        /// print the three steps, do not run them
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// list subscription store rows.
    Subscriptions {},
    /// drop one subscription row by id.
    Unsubscribe {
        /// subscription id (see `subscriptions`)
        id: String,
    },
}

#[allow(clippy::too_many_arguments)] // clap leaf: one param per CLI flag, single call site.
pub(crate) fn cmd_doorbell_introduce(
    cfg: &Config,
    project: &str,
    pin: &str,
    as_pin: Option<&str>,
    channel: Option<&str>,
    workspace: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    crate::check_project_spelling(project, "doorbell introduce")?;
    let _ = cfg;
    if pin.trim().is_empty() {
        anyhow::bail!("doorbell introduce requires --pin <adjectiveNoun>");
    }
    let as_pin = as_pin
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| crate::authority::HostAuthority::resolve().agent_name)
        .ok_or_else(|| anyhow::anyhow!("doorbell introduce needs --as <pin> (or AGENT_NAME)"))?;
    let channel = channel
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::doorbell::introduce_channel_id(pin));
    let workspace = workspace
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::swarm_label(project));
    let steps = crate::doorbell::introduce_steps(project, pin, &as_pin, &channel, &workspace);
    if dry_run {
        if json {
            let rows: Vec<serde_json::Value> = steps
                .iter()
                .map(|s| serde_json::json!({ "verb": s.verb, "argv": s.argv }))
                .collect();
            crate::output::print_json(&serde_json::json!({
                "project": project,
                "pin": pin,
                "as": as_pin,
                "channel": channel,
                "workspace": workspace,
                "steps": rows,
            }));
        } else {
            println!(
                "introduce dry-run pin={pin} as={as_pin} channel={channel} workspace={workspace}"
            );
            for s in &steps {
                println!("  {}", s.argv.join(" "));
            }
        }
        return Ok(());
    }
    let mut results = Vec::new();
    for step in &steps {
        let outcome = match step.verb {
            "subscribe" => run_introduce_subscribe(step, &workspace, pin, project),
            _ => run_introduce_toron(&step.argv),
        };
        match outcome {
            Ok(msg) => {
                println!("{}: {msg}", step.verb);
                results.push((step.verb, true, msg));
            }
            Err(e) => {
                let msg = e.to_string();
                if crate::doorbell::introduce_idempotent(&msg) {
                    println!("{}: already ({msg})", step.verb);
                    results.push((step.verb, true, msg));
                } else {
                    anyhow::bail!("{} failed: {msg}", step.verb);
                }
            }
        }
    }
    if json {
        let rows: Vec<serde_json::Value> = results
            .iter()
            .map(|(v, ok, m)| serde_json::json!({"verb": v, "ok": ok, "detail": m}))
            .collect();
        crate::output::print_json(&serde_json::json!({ "ok": true, "steps": rows }));
    }
    Ok(())
}

fn run_introduce_toron(argv: &[String]) -> Result<String> {
    let out = std::process::Command::new("toron").args(argv).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        Ok(if stdout.is_empty() {
            "ok".into()
        } else {
            stdout
        })
    } else {
        anyhow::bail!(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn run_introduce_subscribe(
    step: &crate::doorbell::IntroduceStep,
    workspace: &str,
    pin: &str,
    project: &str,
) -> Result<String> {
    let _ = step;
    cmd_doorbell_subscribe(
        &Config::default(),
        workspace,
        pin,
        "herdr",
        None,
        &[],
        Some(project),
    )?;
    Ok(format!("subscribed {pin} on {workspace}"))
}

pub(crate) fn cmd_doorbell_roster(cfg: &Config, project: &str, json: bool) -> Result<()> {
    crate::check_project_spelling(project, "doorbell roster")?;
    let registry = crate::state::load_spawn_registry(project);
    let reg: Vec<(String, String, String)> = registry
        .iter()
        .filter_map(|e| {
            let pin = e
                .mail_identity
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| e.name.clone());
            if pin.is_empty() {
                return None;
            }
            Some((pin, e.pane_id.clone(), e.name.clone()))
        })
        .collect();
    let herdr = nearby_herdr_agents(cfg);
    let leases = nearby_leases(project);
    let rows = crate::doorbell::join_nearby(&reg, &herdr, &leases);
    if json {
        crate::output::print_json(&serde_json::to_value(&rows).unwrap_or_default());
    } else {
        for line in crate::doorbell::render_nearby(&rows) {
            println!("{line}");
        }
    }
    Ok(())
}

fn nearby_herdr_agents(cfg: &Config) -> Vec<(String, String, String)> {
    let Ok(mut c) = crate::herdr_shim::Client::connect(Some(&cfg.socket_path())) else {
        return Vec::new();
    };
    let Ok(agents) = c.agent_list() else {
        return Vec::new();
    };
    agents
        .into_iter()
        .map(|a| {
            let name = a
                .name
                .filter(|n| !n.is_empty())
                .or(a.agent)
                .unwrap_or_default();
            let state = format!("{:?}", a.agent_status).to_lowercase();
            (a.pane_id, name, state)
        })
        .collect()
}

fn nearby_leases(project: &str) -> Vec<(String, String)> {
    let out = std::process::Command::new("toron")
        .args(["reserve", "list", "--project", project, "--json"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v.get("reservations")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            Some((
                r.get("holder")?.as_str()?.to_string(),
                r.get("path")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

pub(crate) fn cmd_doorbell_status(cfg: &Config, project: &str) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi: a blank positional used
    // to alias onto the workspace root's workspaces.
    crate::check_project_spelling(project, "doorbell status")?;
    if crate::proxy_doorbell_verb(cfg, "status", &[])? {
        return Ok(());
    }
    crate::ensure_doorbell_supervisor();
    let _ = cfg;
    let state_dir = crate::config::state_dir();
    // bd-bdpx: unbound-but-live panes are invisible to role wake and skipped
    // by the sweep — surface them in status (not just the per-sweep stderr
    // warn) so the operator sees them in one place. Re-bind via
    // `toron role pin <role> <pin> --project <project>`.
    let unpinned = crate::unpinned_registry_names(&crate::state::load_spawn_registry(project));
    let unpinned_seg = if unpinned.is_empty() {
        String::new()
    } else {
        format!(" unpinned={}", unpinned.join(","))
    };
    for ws in crate::doorbell_project_workspaces(project) {
        let pid = crate::doorbell::read_pid(&crate::doorbell::pid_file(&state_dir, &ws));
        let alive = pid.is_some_and(crate::doorbell::pid_alive);
        let wake =
            crate::doorbell::load_wake_state(&crate::doorbell::wake_state_file(&state_dir, &ws));
        let last = wake
            .last_wake_unix
            .map(|t| {
                format!(
                    "last_wake={t} pin={}",
                    wake.last_wake_pin.as_deref().unwrap_or("?")
                )
            })
            .unwrap_or_else(|| "last_wake=never".into());
        // mcc9: the wake → brain counters, from the snapshot the host writes,
        // so a dropping hop is visible here even with no daemon answering.
        let wake_brain =
            wake_brain_status_segment(doorbell_rt::wake_brain::read_snapshot(&state_dir, &ws));
        println!(
            "{project}/{ws}: dispatcher {} {last}{}{}{}{wake_brain}",
            if alive { "up" } else { "DOWN" },
            {
                let pins = crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(
                    &state_dir, &ws,
                ));
                if pins.is_empty() {
                    String::new()
                } else {
                    format!(" unanswered={}", pins.join(","))
                }
            },
            unpinned_seg,
            // bd-7qg6: plane-S cursor age makes a hung-but-alive watch child
            // inspectable — dispatcher `up` with an ancient cursor and fresh
            // bus traffic is the live-pid-deaf signature (heal_tick only
            // restarts dead pids). Age alone never triggers a restart here.
            match wake.plane_s_cursor_us {
                Some(us) => {
                    let age = crate::doorbell_now_unix().saturating_sub((us / 1_000_000) as u64);
                    format!(" watch_idle={age}s")
                }
                None => " watch_idle=never".to_string(),
            },
        );
        if !alive {
            // lazy heal: report AND restart (requirement 1)
            match crate::doorbell::ensure_running(
                &std::env::current_exe()?,
                &crate::config::state_dir(),
                &ws,
                project,
            ) {
                Ok(a) => println!("  healed ({a:?})"),
                Err(e) => println!("  heal FAILED: {e:#}"),
            }
        }
    }
    Ok(())
}

/// mcc9: ` wake_brain=dropped:N,full:M,backpressured:B,resumed:R,lost:L` from the
/// host's snapshot, or ` wake_brain=unmeasured` when no host wrote one (never
/// a fabricated zero).
fn wake_brain_status_segment(snap: Option<doorbell_rt::wake_brain::WakeBrainSnapshot>) -> String {
    match snap {
        Some(s) => format!(
            " wake_brain=dropped:{},full:{},backpressured:{},resumed:{},lost:{}",
            s.dropped, s.wake_brain_full, s.backpressured, s.resumed, s.events_lost
        ),
        None => " wake_brain=unmeasured".to_string(),
    }
}

#[cfg(test)]
mod wake_brain_status_tests {
    use super::wake_brain_status_segment;
    use doorbell_rt::wake_brain::WakeBrainSnapshot;

    /// mcc9: `doorbell status` names the exact counts; zero is printed as zero
    /// (negative control) and a missing snapshot as `unmeasured`.
    #[test]
    fn doorbell_status_prints_wake_brain_counts() {
        let flooded = WakeBrainSnapshot {
            dropped: 499,
            wake_brain_full: 1,
            backpressured: 0,
            resumed: 1,
            events_lost: 7,
            ..WakeBrainSnapshot::default()
        };
        assert_eq!(
            wake_brain_status_segment(Some(flooded)),
            " wake_brain=dropped:499,full:1,backpressured:0,resumed:1,lost:7"
        );
        assert_eq!(
            wake_brain_status_segment(Some(WakeBrainSnapshot::default())),
            " wake_brain=dropped:0,full:0,backpressured:0,resumed:0,lost:0"
        );
        assert_eq!(wake_brain_status_segment(None), " wake_brain=unmeasured");
    }
}

pub(crate) fn cmd_doorbell_start(cfg: &Config, project: &str) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi
    crate::check_project_spelling(project, "doorbell start")?;
    let workspaces = crate::doorbell_project_workspaces(project);
    // bd-enov (F17): ARM the plane with this project's workspaces. The old
    // call proxied a bare `doorbell start` (no --host) — which only adopts the
    // daemon in STANDBY (mode:standby, subscribers:0). Every wake path then
    // degrades to manual for the whole swarm and nothing notices. A stale
    // `doorbell` binary that predates `--host` now fails LOUD here instead of
    // silently arming a wake-blind plane (proxy bails on non-success).
    let mut arm: Vec<String> = Vec::new();
    for ws in &workspaces {
        arm.push("--host".to_string());
        arm.push(format!("{project}/{ws}"));
    }
    if crate::proxy_doorbell_verb(cfg, "start", &arm)? {
        return Ok(());
    }
    crate::ensure_doorbell_supervisor();
    let exe = std::env::current_exe()?;
    if workspaces.is_empty() {
        eprintln!("doorbell: absent (no swarm workspace yet — attach after spawn)");
        return Ok(());
    }
    for ws in workspaces {
        let action =
            crate::doorbell::ensure_running(&exe, &crate::config::state_dir(), &ws, project)
                .with_context(|| format!("doorbell start {project}/{ws}"))?;
        println!("{project}/{ws}: {action:?}");
    }
    Ok(())
}

pub(crate) fn cmd_doorbell_stop(cfg: &Config, project: &str, workspace: &str) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi
    crate::check_project_spelling(project, "doorbell stop")?;
    // Daemon mode owns all wake: any stop targets the daemon (per-workspace
    // dispatchers no longer exist there; Phase 3 removes them everywhere).
    if workspace == "all" && crate::proxy_doorbell_verb(cfg, "stop", &[])? {
        return Ok(());
    }
    let workspaces: Vec<String> = if workspace == "all" {
        crate::doorbell_project_workspaces(project)
    } else {
        vec![workspace.to_string()]
    };
    for ws in workspaces {
        crate::doorbell::stop(&crate::config::state_dir(), &ws);
        println!("{project}/{ws}: stopped");
    }
    Ok(())
}

/// budget denials exit non-zero naming the reason (no silent drop).
pub(crate) fn cmd_doorbell_inject(
    _cfg: &Config,
    project: &str,
    pin: &str,
    line: &str,
    workspace: &str,
) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi
    crate::check_project_spelling(project, "doorbell inject")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match crate::doorbell::inject_wake_in(
        &crate::config::state_dir(),
        project,
        workspace,
        pin,
        line,
        now,
    ) {
        Ok(summary) => {
            println!("injected: {summary}");
            Ok(())
        }
        Err(crate::doorbell::InjectDeny::Ratchet) => {
            anyhow::bail!("inject refused: identical pin+line already injected this second")
        }
        Err(crate::doorbell::InjectDeny::Budget) => {
            anyhow::bail!("inject refused: pin {pin} over the 3-per-30min console budget")
        }
    }
}

/// Subscribe an identity (D8, bd-woha): thin CLI over the subscription
/// store. The store itself enforces operator-only cross-workspace scopes
pub(crate) fn cmd_doorbell_subscribe(
    _cfg: &Config,
    workspace: &str,
    pin: &str,
    sink: &str,
    url: Option<&str>,
    extra: &[String],
    project: Option<&str>,
) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi: None is meaningful here
    // (workspace-scoped door), Some("") is not.
    crate::ensure_project_opt(project, "doorbell subscribe")?;
    // bd-zawk G8: a missing --project stores a legacy row that is never
    // auto-hosted — the operator reads "subscribed" while nothing can ring.
    // Refuse the silent store LOUDLY instead of minting a dead row.
    // governed-by: bd-zawk
    let project = match project {
        Some(p) => p,
        None => {
            eprintln!(
                "doorbell subscribe: --project is required (no silent store: a project-less row is never auto-hosted, so the wake would never ring). Pass --project <slug>."
            );
            anyhow::bail!("doorbell subscribe requires --project <slug>");
        }
    };
    use doorbell_rt::subscribe::{
        Actor, Budget, Filter, Identity, Scope, SinkKind, Subscription, SubscriptionStore,
    };
    let kind = match sink {
        "herdr" => SinkKind::Herdr,
        "webhook" => SinkKind::Webhook,
        "dm" => SinkKind::Dm,
        other => anyhow::bail!("unknown sink {other} (herdr|webhook|dm)"),
    };
    let mut ws = vec![workspace.to_string()];
    for e in extra {
        if !ws.contains(e) {
            ws.push(e.clone());
        }
    }
    let scope = if ws.len() == 1 {
        Scope::One(ws[0].clone())
    } else {
        Scope::Many(ws.clone())
    };
    // Console is the operator: cross-scope rows are operator-intended.
    // Single-workspace herdr rows stay local-process.
    let cross = scope.is_cross(workspace) || kind != SinkKind::Herdr;
    let actor = if cross {
        Actor::operator()
    } else {
        Actor::local_process()
    };
    // bd-8ulu: the endpoint is the row's destination, so a webhook row
    // without one is stored as-is (the daemon then registers nothing and the
    // wake stays digest-visible) — never silently pointed at a default host.
    let endpoint = url
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let sub = Subscription {
        id: format!("cli-{workspace}-{pin}"),
        identity: Identity::Pin(pin.to_string()),
        scope,
        filter: Filter {
            classes: Vec::new(),
            kinds: Vec::new(),
        },
        sink: kind,
        budget: Budget::default_530(),
        project: Some(project.to_string()),
        endpoint,
    };
    let mut store = SubscriptionStore::open(&crate::config::state_dir());
    match store.subscribe(actor, sub, workspace) {
        Ok(row) => {
            println!("subscribed: {} scope={:?} sink={sink}", row.id, row.scope);
            Ok(())
        }
        Err(e) => anyhow::bail!("subscribe refused: {e:?}"),
    }
}

/// Generalized digest (bd-asr2.11, D8): per-identity wake digest over any
/// window. `afk` above stays byte-identical (captain default); this verb
pub(crate) fn cmd_doorbell_digest(
    cfg: &Config,
    project: &str,
    identity: Option<&str>,
    since: Option<&str>,
    format: &str,
) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi
    crate::check_project_spelling(project, "doorbell digest")?;
    let _ = cfg;
    let _ = project;
    let entries = crate::doorbell::journal_read_all(&crate::config::state_dir());
    let since_us = since.and_then(|s| doorbell_core::parse_rfc3339_micros(s).ok());
    // Unanswered pins escalate first (deaf services surface, S3).
    let mut unanswered: std::collections::BTreeSet<String> = Default::default();
    for ws in crate::doorbell::journal_workspaces_list(&crate::config::state_dir()) {
        for pin in crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(
            &crate::config::state_dir(),
            &ws,
        )) {
            if identity.is_none_or(|id| pin == id) {
                unanswered.insert(pin);
            }
        }
    }
    let mut rows: Vec<(String, String, String, String, String)> = Vec::new();
    for (ws, entry) in &entries {
        if !crate::doorbell::journal_version_accepted(entry) {
            continue;
        }
        if let Some(id) = identity {
            let tag = entry.get("subscriber").and_then(|s| s.as_str());
            let in_pins = entry
                .get("pins")
                .and_then(|p| p.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).any(|p| p == id))
                .unwrap_or(false);
            // bd-ff92: watch-decided rows carry pins:[] — the addressed pin
            // lives only inside actions[].Digest.pin (digest-bound wake for
            // a pane-less pin). Without this the observed live wake line is
            // invisible to `digest --identity`, exactly the reported symptom.
            let in_actions = entry
                .get("actions")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter().any(|v| {
                        v.get("Digest")
                            .and_then(|d| d.get("pin"))
                            .and_then(|p| p.as_str())
                            == Some(id)
                            || v.get("Prompt")
                                .and_then(|d| d.get("pin"))
                                .and_then(|p| p.as_str())
                                == Some(id)
                    })
                })
                .unwrap_or(false);
            if tag != Some(id) && !in_pins && !in_actions {
                continue;
            }
        }
        if let Some(floor) = since_us {
            let ts_us = entry
                .get("ts")
                .and_then(|t| t.as_str())
                .and_then(|t| doorbell_core::parse_rfc3339_micros(t).ok());
            if ts_us.is_some_and(|t| t < floor) {
                continue;
            }
        }
        let ts = entry
            .get("ts")
            .and_then(|t| t.as_str())
            .unwrap_or("?")
            .to_string();
        let stage = entry
            .get("executed")
            .and_then(|e| e.get("stage"))
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        let who = entry
            .get("subscriber")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                entry
                    .get("pins")
                    .and_then(|p| p.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default()
            });
        // bd-bk9t: a digest-bound wake (a pane-less subscribed pin) has no
        // prompt route, so its `Digest` action row IS the delivery. Without
        // the line here the batch could only be read by grepping the journal.
        let detail = crate::digest_action_entries(entry)
            .into_iter()
            .map(|(_, line, _)| line)
            .collect::<Vec<_>>()
            .join(" | ");
        rows.push((ws.clone(), ts, stage, who, detail));
    }
    let roster = crate::digest_roster_lines(cfg, project, crate::doorbell_now_unix());
    if format == "json" {
        println!(
            "{}",
            serde_json::json!({
                "identity": identity,
                "since": since,
                "unanswered": unanswered.iter().collect::<Vec<_>>(),
                "rows": rows.iter().map(|(ws, ts, stage, who, detail)| {
                    serde_json::json!({"workspace": ws, "ts": ts, "stage": stage, "who": who, "detail": detail})
                }).collect::<Vec<_>>(),
                "roster": roster,
            })
        );
        return Ok(());
    }
    if !unanswered.is_empty() {
        println!(
            "UNANSWERED (escalated): {}",
            unanswered
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if rows.is_empty() {
        println!("doorbell digest: nothing queued");
    }
    for (ws, ts, stage, who, detail) in rows {
        if detail.is_empty() {
            println!("{ts} {ws}/{stage} {who}");
        } else {
            println!("{ts} {ws}/{stage} {who} — {detail}");
        }
    }
    for line in roster {
        println!("{line}");
    }
    Ok(())
}

/// bd-durable-doorbell-lngf requirement 4: re-inject the peer-aware handler
/// and identity line into EXISTING panes — covers herdr pane-split additions
/// and post-crash respawns that never got the spawn-time injection.
pub(crate) fn cmd_doorbell_re_inject(cfg: &Config, project: &str) -> Result<()> {
    // bd-blank-project-spelling-non-mail-doors-76hi
    crate::check_project_spelling(project, "doorbell re-inject")?;
    crate::ensure_doorbell_supervisor();
    let registry = crate::state::load_spawn_registry(project);
    let peer_tpl = include_str!("../../integrations/peer-aware.md.tpl");
    let mut c = crate::herdr_shim::Client::connect(Some(&cfg.socket_path()))?;
    let mut injected = 0;
    for e in &registry {
        if e.pane_id.is_empty() {
            continue;
        }
        let Some(identity) = e.mail_identity.as_deref() else {
            continue;
        };
        let mut lines = vec![crate::doorbell::identity_prime(&e.name, identity)];
        lines.push(crate::peer_prompt_template(
            peer_tpl, &e.name, identity,
            // ADR-0005 budget defaults (5/30/3) — same as `spawn`'s.
            5, 30, 3,
        ));
        for line in lines {
            // ADR-0005 H2: agent.prompt is gated until the pane's first
            // prompt lands — the identity prime IS that neutral opener, so
            // the handler inject behind it goes through.
            c.agent_prompt(&e.pane_id, &line, None).map_err(|err| {
                anyhow::anyhow!(
                    "re-inject {}: pane {} not promptable ({err}) — is the agent up?",
                    e.name,
                    e.pane_id
                )
            })?;
        }
        injected += 1;
        println!("{} ({}): handler + identity re-injected", e.name, e.pane_id);
    }
    println!("re-injected {injected} pane(s) in {project}");
    Ok(())
}

pub(crate) fn cmd_internal_doorbell(
    cfg: &Config,
    project: &str,
    workspace: &str,
    once: bool,
    report: bool,
    status: &[String],
) -> Result<()> {
    if report {
        print!(
            "{}",
            crate::doorbell::report_text(&crate::config::state_dir(), workspace)
        );
        return Ok(());
    }
    if once {
        // herdr-mock: no pid file (must not clobber a live dispatcher).
        let mut map = BTreeMap::new();
        for pair in status {
            if let Some((pin, st)) = crate::doorbell::parse_status_pair(pair) {
                map.insert(pin, st);
            }
        }
        let mut stdin = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut stdin)
            .context("read doorbell --once stdin")?;
        print!("{}", crate::doorbell::once_json(&stdin, &map));
        return Ok(());
    }

    // bd-asr2.5: host selection — one implementation, two hosts. Daemon
    // mode never runs a local loop (subscription registration via API
    // lands in bd-asr2.8); embedded mode drives doorbell-rt's Brain.
    match crate::doorbell::resolve_host(
        &crate::config::state_dir(),
        cfg.doorbell_url(),
        Some((project, workspace)),
    ) {
        crate::doorbell::DoorbellHost::Proxy { url } => {
            eprintln!("flywheel doorbell: daemon host {url} — no local loop (attach only)");
            Ok(())
        }
        crate::doorbell::DoorbellHost::Embedded => {
            crate::doorbell_host::run_embedded(cfg, project, workspace)
        }
    }
}
