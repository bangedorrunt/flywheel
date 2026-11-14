//! `flywheel doorbell` subcommand types (split from `main.rs`, bd-qbi8).

use std::collections::BTreeMap;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result};
use clap::Subcommand;

#[cfg(test)]
use crate::budget_effect;
use crate::cmd::workflow::{WorkflowCmd, cmd_workflow};
use crate::config::Config;
use crate::{
    backend, cmd_binding_rebind, config, doorbell, doorbell_tick, live_decide_snapshot,
    project_dir_str, snapshot, state, workspace,
};

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
    /// Run the reboot-persistent supervisor loop (the installed unit runs
    /// this; it re-runs the idempotent start every minute and exits on its
    /// own binary change so the service manager restarts the new binary).
    #[command(hide = true)]
    Supervise { project: String },
    /// stop dispatchers (per workspace; `all` for every registered one)
    Stop {
        project: String,
        /// workspace id, or `all`
        #[arg(default_value = "all")]
        workspace: String,
    },
    /// re-inject the peer-aware handler + identity line into EXISTING panes
    /// (covers hc terminal-split additions and post-crash respawns).
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
        /// pin (adjective+noun) to explain; omit to explain all pins in scope
        /// (use with --corr/--seq for assignment replay)
        pin: Option<String>,
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
        /// only rows from this workspace (session id or label)
        #[arg(long)]
        workspace: Option<String>,
        /// only rows at/after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
        /// machine-readable replay with matched/shown/withheld counts
        #[arg(long)]
        json: bool,
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
    /// scopes and non-hc sinks require the operator (refused loudly).
    Subscribe {
        /// home workspace
        #[arg(long)]
        workspace: String,
        /// pin (adjective+noun) to subscribe
        #[arg(long)]
        pin: String,
        /// sink: hc (default), webhook, dm (endpoint binds at the adapter)
        #[arg(long, default_value = "hc")]
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
    /// nearby join: hc terminals ∪ registry pins ∪ reservations (bd-gpj0.2).
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
    /// prune audit logs of workspaces no dispatcher will serve again.
    ///
    /// Rotation caps a live file; nothing removes one. Without `--yes` this
    /// only reports what it would delete.
    Gc {
        /// also prune a quiet workspace's audit file after this many days
        #[arg(long, default_value_t = crate::doorbell::AUDIT_PRUNE_DAYS)]
        older_than_days: u64,
        /// Required to delete
        #[arg(long)]
        yes: bool,
    },
}

/// The `--as` actor of a doorbell: the ONE identity ladder (bd-05ar).
///
/// Pure so the order is provable without mutating the process env — the
/// doorbell actor must be the same pin that holds the lease and attributes
/// the commit, or the wake lands on a stranger's pin.
pub(crate) fn resolve_as_pin(
    explicit: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    crate::authority::select_agent_identity(explicit, env)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_doorbell_introduce(
    project: &str,
    pin: &str,
    as_pin: Option<&str>,
    channel: Option<&str>,
    workspace: Option<&str>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    crate::check_project_spelling(project, "doorbell introduce")?;
    if pin.trim().is_empty() {
        anyhow::bail!("doorbell introduce requires --pin <adjectiveNoun>");
    }
    let as_pin = resolve_as_pin(as_pin, &|k| std::env::var(k).ok())
        .ok_or_else(|| anyhow::anyhow!("doorbell introduce needs --as <pin> or AGENT_NAME"))?;
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

/// Passthrough to the toron CLI for introduce steps (contact request,
/// channel create). Kept as a subprocess by design (bd-wave2): routing the
/// contact verb alone would split one atomic introduce across two transports.
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
        "hc",
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
    let hc = nearby_plane_agents(cfg);
    let leases = nearby_leases(cfg, project);
    let rows = crate::doorbell::join_nearby(&reg, &hc, &leases);
    if json {
        crate::output::print_json(&serde_json::to_value(&rows).unwrap_or_default());
    } else {
        for line in crate::doorbell::render_nearby(&rows) {
            println!("{line}");
        }
    }
    Ok(())
}

fn nearby_plane_agents(cfg: &Config) -> Vec<(String, String, String)> {
    let Ok(backend) = crate::backend::multiplexer_backend(cfg) else {
        return Vec::new();
    };
    let Ok(agents) = backend.agent_list() else {
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

fn nearby_leases(cfg: &Config, project: &str) -> Vec<(String, String)> {
    let mail = match crate::mail_or_die(cfg) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let v = match mail.list_active(project, None, None) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
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
    let project = &canonical_project_key(project, "doorbell status")?;
    if crate::cmd::spawn::proxy_doorbell_verb(cfg, "status", &[])? {
        return Ok(());
    }
    crate::cmd::spawn::ensure_doorbell_supervisor(cfg, project);
    let state_dir = crate::config::state_dir();
    let snap = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix());
    let live_label = snap.workspace.clone();
    let live_id = snap.workspace_id.clone();
    let workspaces = crate::cmd::spawn::doorbell_project_workspaces(project);
    let unpinned = crate::unpinned_registry_names(&crate::state::load_spawn_registry(project));
    let unpinned_seg = if unpinned.is_empty() {
        String::new()
    } else {
        format!(" unpinned={}", unpinned.join(","))
    };
    // bd-bj9xk: name a dead session row instead of hiding it. The live plane
    // list this host can prove today is the snapshot's own session id.
    let live_ids: Vec<String> = if live_id.is_empty() {
        Vec::new()
    } else {
        vec![live_id.clone()]
    };
    let reaped = reap_gone_dispatchers(cfg, project);
    if !reaped.is_empty() {
        println!(
            "{project}: reaped {} gone dispatcher(s): {}",
            reaped.len(),
            reaped.join(",")
        );
    }
    for (ws, pid) in crate::doorbell::project_dispatchers(&state_dir, project) {
        let alive = pid.is_some_and(crate::doorbell::pid_alive);
        println!(
            "{project}/{ws}: dispatcher {} pid={}",
            if alive { "up" } else { "DOWN" },
            pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
        );
    }
    for ws in canonical_workspaces(&workspaces, &live_label, &live_id) {
        let gone = match dispatcher_gone(&ws, &live_ids) {
            Some(GoneReason::SessionAbsent) => " gone=session-absent",
            Some(GoneReason::LabelAlias) => " gone=label-alias",
            None => "",
        };
        let alias = workspaces
            .iter()
            .find(|raw| raw.as_str() != ws && doorbell_host_key(raw, &live_label, &live_id) == ws)
            .map(|raw| format!(" label={raw}"))
            .unwrap_or_default();
        let pid = crate::doorbell::dispatcher_pid(&state_dir, &ws);
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
        let wake_brain =
            wake_brain_status_segment(doorbell_rt::wake_brain::read_snapshot(&state_dir, &ws));
        println!(
            "{project}/{ws}{alias}{gone}: dispatcher {} {last}{}{}{}{wake_brain}",
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
            match wake.plane_s_cursor_us {
                Some(us) => {
                    let age = crate::doorbell_now_unix().saturating_sub((us / 1_000_000) as u64);
                    format!(" watch_idle={age}s")
                }
                None => " watch_idle=never".to_string(),
            },
        );
        if !alive {
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

/// bd-supervisor-collab-rering-87gzr: whether `proxy_doorbell_verb` would
/// exec the local `doorbell` binary against a loopback daemon — the only
/// case arm recording is for. Mirrors the proxy's own resolve + host check
/// so a remote (out-of-band) daemon never gains a misleading local record.
fn proxy_targets_loopback_daemon(cfg: &Config) -> bool {
    let crate::doorbell::DoorbellHost::Proxy { url } =
        crate::doorbell::resolve_host(&crate::config::state_dir(), cfg.doorbell_url(), None)
    else {
        return false;
    };
    let host = url
        .trim_start_matches("http://")
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    host == "127.0.0.1" || host == "localhost"
}

/// bd-supervisor-collab-rering-87gzr: the arm record the running daemon
/// actually reads. Byte-compatible with doorbell-cli `armstate::persist`
/// (`{"v":1,"hosts":[[project,workspace],...]}`) — that crate owns the
/// format, this is the only other writer, and the shape test below pins
/// the compatibility. Atomic tmp+rename, 0600, same as the owner.
fn doorbell_hosts_file(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("hosts.json")
}

/// Merge persisted ∪ requested (persisted order first, dedup) and record
/// it — the same merge doorbell-cli applies at startup. Fail-closed on a
/// corrupt file (never an empty set), like the owner: treating unreadable
/// state as "no hosts" is the exact bug bd-u54l exists to end.
fn record_doorbell_arms(
    state_dir: &std::path::Path,
    requested: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let path = doorbell_hosts_file(state_dir);
    let mut merged: Vec<(String, String)> = match std::fs::read_to_string(&path) {
        Ok(raw) if raw.trim().is_empty() => Vec::new(),
        Ok(raw) => {
            let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!(
                    "doorbell: {} is CORRUPT ({e}). Refusing to arm: fix the file or remove it to re-arm.",
                    path.display()
                )
            })?;
            let arr = v.get("hosts").and_then(|h| h.as_array()).ok_or_else(|| {
                anyhow::anyhow!("doorbell: {} has no `hosts` array", path.display())
            })?;
            let mut out = Vec::new();
            for item in arr {
                let pair = item.as_array().filter(|p| p.len() == 2).ok_or_else(|| {
                    anyhow::anyhow!(
                        "doorbell: {} has a malformed host entry: {item}",
                        path.display()
                    )
                })?;
                let h = (
                    pair[0]
                        .as_str()
                        .ok_or_else(|| {
                            anyhow::anyhow!("doorbell: {} has a non-string project", path.display())
                        })?
                        .to_string(),
                    pair[1]
                        .as_str()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "doorbell: {} has a non-string workspace",
                                path.display()
                            )
                        })?
                        .to_string(),
                );
                if !out.contains(&h) {
                    out.push(h);
                }
            }
            out
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => anyhow::bail!("doorbell: cannot read {}: {e}", path.display()),
    };
    for h in requested {
        if !merged.contains(h) {
            merged.push(h.clone());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::json!({
        "v": 1,
        "hosts": merged
            .iter()
            .map(|(p, w)| serde_json::json!([p, w]))
            .collect::<Vec<_>>(),
    });
    let mut bytes = serde_json::to_vec_pretty(&body)?;
    bytes.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        #[cfg(unix)]
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        #[cfg(not(unix))]
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(merged)
}

pub(crate) fn cmd_doorbell_start(cfg: &Config, project: &str) -> Result<()> {
    let project = &canonical_project_key(project, "doorbell start")?;
    let snap = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix());
    let live_label = snap.workspace.clone();
    let live_id = snap.workspace_id.clone();
    let mut workspaces = crate::cmd::spawn::doorbell_project_workspaces(project);
    if workspaces.is_empty() && !live_id.is_empty() && !live_label.is_empty() {
        workspaces.push(live_label.clone());
    }
    let workspaces = canonical_workspaces(&workspaces, &live_label, &live_id);
    let mut arm: Vec<String> = Vec::new();
    for ws in &workspaces {
        arm.push("--host".to_string());
        arm.push(format!("{project}/{ws}"));
    }
    // bd-supervisor-collab-rering-87gzr: `doorbell start` adopts a running
    // daemon WITHOUT applying the requested arms (it warns and exits 0).
    // Record persisted ∪ requested first so the adopt path sees no
    // divergence (no misleading warning) and the arm is durable — the
    // running daemon applies it on restart. Loopback only.
    if proxy_targets_loopback_daemon(cfg) {
        let requested: Vec<(String, String)> = workspaces
            .iter()
            .map(|ws| (project.to_string(), ws.clone()))
            .collect();
        let merged = record_doorbell_arms(&crate::config::state_dir(), &requested)?;
        eprintln!(
            "doorbell: arms recorded ({} host(s)); a running daemon applies them on restart",
            merged.len()
        );
    }
    let proxied = crate::cmd::spawn::proxy_doorbell_verb(cfg, "start", &arm)?;
    if !crate::doorbell::local_loop_after_proxy(proxied, crate::comms_send::enabled(cfg)) {
        return Ok(());
    }
    crate::cmd::spawn::ensure_doorbell_supervisor(cfg, project);
    let exe = std::env::current_exe()?;
    if workspaces.is_empty() {
        eprintln!("doorbell: absent (no swarm workspace yet — attach after spawn)");
        return Ok(());
    }
    for ws in &workspaces {
        let action =
            crate::doorbell::ensure_running(&exe, &crate::config::state_dir(), ws, project)
                .with_context(|| format!("doorbell start {project}/{ws}"))?;
        println!("{project}/{ws}: {action:?}");
    }
    Ok(())
}

/// The reboot-persistent supervisor body: run the idempotent start every
/// minute, and exit when this executable changes so launchd/systemd restarts
/// the new binary. The installed unit runs exactly this command.
pub(crate) fn cmd_doorbell_supervise(cfg: &Config, project: &str) -> Result<()> {
    let project = &canonical_project_key(project, "doorbell supervise")?;
    let _ = reap_gone_dispatchers(cfg, project);
    let exe = std::env::current_exe()?;
    let stamp = binary_stamp(&exe);
    loop {
        if let Err(e) = cmd_doorbell_start(cfg, project) {
            eprintln!("flywheel doorbell supervise: start failed: {e:#}");
        }
        if binary_stamp(&exe) != stamp {
            eprintln!(
                "flywheel doorbell supervise: binary changed; exiting for the supervisor restart"
            );
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn binary_stamp(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// The plane resolves the live session workspace by its ID, while the state
/// files and the central daemon arms use the stable project label. A host
/// started with the label retires with `workspace-gone` even though the
/// panes are live; map the label to the snapshot's ID for the local host.
/// Why a dispatcher's workspace is gone (bd-bj9xk reap). The live host keys by
/// rex session id (bd-g9hqu), so a `session:<id>` absent from the plane is
/// dead, and a bare label is the legacy alias form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoneReason {
    SessionAbsent,
    LabelAlias,
}

/// Pure: does this dispatcher workspace still belong to the plane? A caller
/// that has the live session ids passes them in; nothing is guessed.
pub(crate) fn dispatcher_gone(workspace: &str, live: &[String]) -> Option<GoneReason> {
    match workspace.strip_prefix("session:") {
        Some(id) => {
            if live.iter().any(|s| s == id || s == workspace) {
                None
            } else {
                Some(GoneReason::SessionAbsent)
            }
        }
        None => Some(GoneReason::LabelAlias),
    }
}

/// The plane's live session ids. `workspace_list` IS the session list on rex
/// (each WorkspaceInfo is one session; hc's workspaces are hc panes), so no
/// second accessor is invented for it (bd-bj9xk).
fn live_session_ids(cfg: &Config) -> Vec<String> {
    let Ok(backend) = crate::backend::multiplexer_backend(cfg) else {
        return Vec::new();
    };
    let Ok(list) = backend.workspace_list() else {
        return Vec::new();
    };
    list.into_iter().map(|w| w.workspace_id).collect()
}

/// Reap every dispatcher whose workspace no longer belongs to the plane:
/// journal-first intent, stop (which also drops the record), outcome audit.
/// A failed stop keeps the record so the next status retries it.
fn reap_gone_dispatchers(cfg: &Config, project: &str) -> Vec<String> {
    let live = live_session_ids(cfg);
    if live.is_empty() {
        return Vec::new();
    }
    let state_dir = crate::config::state_dir();
    let mut reaped = Vec::new();
    for (ws, pid) in crate::doorbell::project_dispatchers(&state_dir, project) {
        let Some(reason) = dispatcher_gone(&ws, &live) else {
            continue;
        };
        let reason = match reason {
            GoneReason::SessionAbsent => "session-absent",
            GoneReason::LabelAlias => "label-alias",
        };
        crate::doorbell::audit_append(
            &state_dir,
            &ws,
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "kind": "dispatcher_gone_intent",
                "project": project,
                "workspace": ws,
                "pid": pid,
                "reason": reason,
            }),
        );
        crate::doorbell::stop(&state_dir, &ws);
        let still = crate::doorbell::dispatcher_pid(&state_dir, &ws)
            .is_some_and(crate::doorbell::pid_alive);
        crate::doorbell::audit_append(
            &state_dir,
            &ws,
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "kind": if still { "dispatcher_gone_stop_failed" } else { "dispatcher_gone_reaped" },
                "project": project,
                "workspace": ws,
                "pid": pid,
                "reason": reason,
            }),
        );
        if !still {
            eprintln!(
                "flywheel doorbell: reaped gone dispatcher {project}/{ws} (pid {pid:?}, {reason})"
            );
            reaped.push(ws);
        }
    }
    reaped
}

/// The project key every door folds through: a path names the same project
/// as its slug, and one project must not get two dispatchers (bd-1mi26).
fn canonical_project_key(project: &str, door: &str) -> Result<String> {
    crate::check_project_spelling(project, door)?;
    Ok(crate::doorbell::canonical_project(project))
}

fn doorbell_host_key<'a>(label: &'a str, live_label: &'a str, live_id: &'a str) -> &'a str {
    crate::doorbell::canonical_workspace(label, live_label, live_id)
}

/// One workspace, one key: `workspaces` collapsed to their canonical
/// dispatcher keys (the live session id for a label), order preserved.
/// Registering, starting, stopping, healing and reporting all go through
/// this, so a label and its id never act as two workspaces (bd-g9hqu).
fn canonical_workspaces(workspaces: &[String], live_label: &str, live_id: &str) -> Vec<String> {
    crate::doorbell::canonical_workspaces(workspaces, live_label, live_id)
}

pub(crate) fn cmd_doorbell_stop(cfg: &Config, project: &str, workspace: &str) -> Result<()> {
    let project = &canonical_project_key(project, "doorbell stop")?;
    if workspace == "all" && crate::cmd::spawn::proxy_doorbell_verb(cfg, "stop", &[])? {
        return Ok(());
    }
    let snap = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix());
    let live_label = snap.workspace.clone();
    let live_id = snap.workspace_id.clone();
    let workspaces: Vec<String> = if workspace == "all" {
        let mut list = crate::cmd::spawn::doorbell_project_workspaces(project);
        if list.is_empty() && !live_id.is_empty() && !live_label.is_empty() {
            list.push(live_label.clone());
        }
        list
    } else {
        vec![workspace.to_string()]
    };
    for ws in workspaces {
        let host_ws = doorbell_host_key(&ws, &live_label, &live_id);
        crate::doorbell::stop(&crate::config::state_dir(), host_ws);
        if host_ws != ws {
            crate::doorbell::stop(&crate::config::state_dir(), &ws);
        }
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
    crate::ensure_project_opt(project, "doorbell subscribe")?;
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
        "hc" => SinkKind::Hc,
        "webhook" => SinkKind::Webhook,
        "dm" => SinkKind::Dm,
        other => anyhow::bail!("unknown sink {other} (hc|webhook|dm)"),
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
    let cross = scope.is_cross(workspace) || kind != SinkKind::Hc;
    let actor = if cross {
        Actor::operator()
    } else {
        Actor::local_process()
    };
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
    crate::check_project_spelling(project, "doorbell digest")?;
    let state_dir = crate::config::state_dir();
    let scoped = crate::doorbell::project_workspaces(&state_dir, project);
    let entries: Vec<(String, serde_json::Value)> = crate::doorbell::journal_read_all(&state_dir)
        .into_iter()
        .filter(|(ws, _)| scoped.contains(ws))
        .collect();
    let since_us = since.and_then(|s| doorbell_core::parse_rfc3339_micros(s).ok());
    let mut unanswered: std::collections::BTreeSet<String> = Default::default();
    for ws in &scoped {
        for pin in
            crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(&state_dir, ws))
        {
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
        let detail = digest_action_entries(entry)
            .into_iter()
            .map(|(_, line, _)| line)
            .collect::<Vec<_>>()
            .join(" | ");
        rows.push((ws.clone(), ts, stage, who, detail));
    }
    let roster = digest_roster_lines(cfg, project, doorbell_now_unix());
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
/// and identity line into EXISTING panes — covers hc terminal-split additions
/// and post-crash respawns that never got the spawn-time injection.
pub(crate) fn cmd_doorbell_re_inject(cfg: &Config, project: &str) -> Result<()> {
    crate::check_project_spelling(project, "doorbell re-inject")?;
    crate::cmd::spawn::ensure_doorbell_supervisor(cfg, project);
    let registry = crate::state::load_spawn_registry(project);
    let peer_tpl = include_str!("../../integrations/peer-aware.md.tpl");
    // rex-first: the plane backend is the one the panes actually run on.
    // `hc::Client` here failed every re-inject on the default rex plane.
    let backend = crate::backend::multiplexer_backend(cfg)?;
    let captain = crate::state::load_captain_alias(project);
    let mut injected = 0;
    for e in &registry {
        if e.pane_id.is_empty() {
            continue;
        }
        let Some(identity) = e.mail_identity.as_deref() else {
            continue;
        };
        // The operator/captain pane is the session the captain is typing in;
        // injecting a peer handler there changes the operator context, and a
        // busy captain must not abort the pass for every other pane.
        if captain.as_deref() == Some(identity) {
            println!(
                "{} ({}): captain/operator pane — skipped",
                e.name, e.pane_id
            );
            continue;
        }
        let mut lines = vec![crate::doorbell::identity_prime(&e.name, identity)];
        lines.push(crate::peer_prompt_template(
            peer_tpl, &e.name, identity, 5, 30, 3,
        ));
        let mut ok = true;
        for line in lines {
            if let Err(err) = backend.prompt(&e.pane_id, &line, None) {
                eprintln!(
                    "re-inject {}: pane {} not promptable ({err}) — skipped; the rest continue",
                    e.name, e.pane_id
                );
                ok = false;
                break;
            }
        }
        if ok {
            injected += 1;
            println!("{} ({}): handler + identity re-injected", e.name, e.pane_id);
        }
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

    let comms_on = crate::comms_send::enabled(cfg);
    match crate::doorbell::host_for(comms_on, || {
        crate::doorbell::resolve_host(
            &crate::config::state_dir(),
            cfg.doorbell_url(),
            Some((project, workspace)),
        )
    }) {
        crate::doorbell::DoorbellHost::Proxy { url } => {
            eprintln!("flywheel doorbell: daemon host {url} — no local loop (attach only)");
            Ok(())
        }
        crate::doorbell::DoorbellHost::Embedded => {
            crate::doorbell_host::run_embedded(cfg, project, workspace)
        }
    }
}

/// `flywheel doorbell gc`: delete the audit trail of dead workspaces.
///
/// The candidate rule (live pid wins, vanished project path dead now, bare
/// project name dead on age) lives in [`crate::doorbell::dead_audit_files`],
/// so the decision is testable without deleting anything.
pub(crate) fn cmd_doorbell_gc(older_than_days: u64, yes: bool) -> Result<()> {
    let state_dir = crate::config::state_dir();
    let candidates = crate::doorbell::dead_audit_files(
        &state_dir,
        older_than_days,
        std::time::SystemTime::now(),
        crate::doorbell::pid_alive,
    );
    let prunable_bytes: u64 = candidates.iter().map(|c| c.bytes).sum();
    let mut freed = 0u64;
    let mut rows = Vec::with_capacity(candidates.len());
    for c in &candidates {
        if yes {
            std::fs::remove_file(&c.path)
                .with_context(|| format!("remove {}", c.path.display()))?;
            freed += c.bytes;
        }
        rows.push(serde_json::json!({
            "workspace": c.workspace,
            "path": c.path.display().to_string(),
            "bytes": c.bytes,
            "reason": c.reason,
        }));
    }
    crate::output::print_json(&serde_json::json!({
        "state_dir": state_dir.join("doorbell").display().to_string(),
        "older_than_days": older_than_days,
        "applied": yes,
        "prunable_files": candidates.len(),
        "prunable_bytes": prunable_bytes,
        "freed_bytes": freed,
        "files": rows,
        "hint": if yes {
            serde_json::Value::Null
        } else {
            serde_json::json!("pass --yes to delete")
        },
    }));
    Ok(())
}

#[cfg(test)]
mod doorbell_arm_record_tests {
    use super::*;

    fn sandbox(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fw-arm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// bd-supervisor-collab-rering-87gzr: the recorded arm is persisted ∪
    /// requested (persisted order first) — the running daemon's adopt path
    /// then sees no divergence and the arm survives a restart.
    #[test]
    fn recorded_arms_merge_persisted_and_requested() {
        let dir = sandbox("merge");
        std::fs::write(
            doorbell_hosts_file(&dir),
            "{\"v\":1,\"hosts\":[[\"flywheel\",\"flywheel-flywheel\"]]}\n",
        )
        .unwrap();
        let merged = record_doorbell_arms(
            &dir,
            &[
                ("flywheel".into(), "flywheel-flywheel".into()),
                ("flywheel".into(), "flywheel-other".into()),
            ],
        )
        .expect("merge records");
        assert_eq!(
            merged,
            vec![
                ("flywheel".to_string(), "flywheel-flywheel".to_string()),
                ("flywheel".to_string(), "flywheel-other".to_string()),
            ],
            "persisted order first, requested appended, deduped"
        );
        let raw = std::fs::read_to_string(doorbell_hosts_file(&dir)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["v"], 1, "shape matches armstate::persist");
        assert_eq!(
            v["hosts"],
            serde_json::json!([
                ["flywheel", "flywheel-flywheel"],
                ["flywheel", "flywheel-other"]
            ])
        );
        assert!(
            !dir.join("hosts.json.tmp").exists(),
            "atomic tmp is renamed, never left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fail-closed like the owner: a corrupt record refuses the arm.
    #[test]
    fn corrupt_hosts_refuse_the_arm() {
        let dir = sandbox("corrupt");
        std::fs::write(doorbell_hosts_file(&dir), "not json\n").unwrap();
        assert!(
            record_doorbell_arms(&dir, &[("p".into(), "w".into())]).is_err(),
            "corrupt state is never read as empty"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

/// bd-yw4s: `doorbell why [pin]` — replay the doorbell journal and explain
/// what the tick decided (prompted / queued / drained / died / sla). Omitting
/// the pin explains every pin in scope. The trust surface; never log tailing.
/// Folds tick from Default
/// over state_dir/journals/doorbell-journal-{workspace}.jsonl and asserts
/// the same Cmd sequence as the live run (replay, not grep). Supports
/// --corr and --seq for assignment replay.
#[derive(Debug, Clone)]
struct WhyDisplayRow {
    workspace: String,
    ts: String,
    ts_us: Option<i64>,
    seq: u64,
    kind: String,
    status: String,
    actions: String,
    dropped: Vec<String>,
    folded: bool,
    console: bool,
}

/// Global journal order is workspace-grouped; a global head limit therefore
/// hides every later workspace. Sort the matched rows by time and keep the
/// newest `limit`, so a multi-workspace project cannot silently lose a scope.
fn why_newest(mut rows: Vec<WhyDisplayRow>, limit: usize) -> (Vec<WhyDisplayRow>, usize) {
    rows.sort_by(|a, b| {
        a.ts_us
            .cmp(&b.ts_us)
            .then_with(|| a.workspace.cmp(&b.workspace))
            .then_with(|| a.seq.cmp(&b.seq))
    });
    let withheld = rows.len().saturating_sub(limit);
    let start = rows.len().saturating_sub(limit);
    let shown = rows.split_off(start);
    (shown, withheld)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_doorbell_why(
    cfg: &Config,
    project: &str,
    pin: Option<&str>,
    limit: usize,
    corr: Option<&str>,
    seq: Option<u64>,
    subscriber: Option<&str>,
    workspace_filter: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> Result<()> {
    crate::check_project_spelling(project, "doorbell why")?;
    let since_us = since
        .map(|s| {
            doorbell_core::parse_rfc3339_micros(s)
                .map_err(|e| anyhow::anyhow!("doorbell why: invalid --since {s:?}: {e}"))
        })
        .transpose()?;
    let scoped = doorbell_scoped_workspaces(cfg, project);
    let entries = doorbell::journal_read_all(&config::state_dir());
    {
        let mut workspaces: std::collections::BTreeSet<String> =
            entries.iter().map(|(ws, _)| ws.clone()).collect();
        if let Ok(dir) = std::fs::read_dir(config::state_dir().join("journals")) {
            for entry in dir.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(rest) = name
                    .strip_prefix("doorbell-journal-")
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    && !rest.is_empty()
                    && !rest.contains('.')
                {
                    workspaces.insert(rest.to_string());
                }
            }
        }
        for ws in &workspaces {
            if let Some(allowed) = &scoped
                && !allowed.iter().any(|a| a == ws)
            {
                continue;
            }
            let (torn, first_seq) = doorbell::journal_fold_status(&config::state_dir(), ws);
            if let Some(t) = torn {
                eprintln!("doorbell why: {t} (workspace {ws})");
            }
            if let (Some(q), Some(first)) = (seq, first_seq)
                && q < first
            {
                eprintln!(
                    "doorbell why: seq {q} predates retention, first foldable seq is {first} (workspace {ws})"
                );
            }
        }
    }
    let presence_rows = std::fs::read_to_string(crate::mail::presence_path())
        .ok()
        .map(|body| timeout_presence_rows(&body, doorbell_now_unix()))
        .unwrap_or_default();
    let mut models: std::collections::BTreeMap<String, workspace::WorkspaceModel> =
        std::collections::BTreeMap::new();
    let mut replay_seq: u64 = 0;
    let mut rows: Vec<WhyDisplayRow> = Vec::new();
    for (workspace, entry) in &entries {
        if let Some(wanted) = workspace_filter
            && workspace != wanted
        {
            continue;
        }
        if !journal_row_in_scope(workspace, entry, &scoped, project) {
            continue;
        }
        if !doorbell::journal_version_accepted(entry) {
            eprintln!(
                "doorbell why: refusing entry with unknown journal v (workspace {workspace}) — no fold-past"
            );
            continue;
        }
        if let Some(sub) = subscriber {
            match entry.get("subscriber").and_then(|s| s.as_str()) {
                Some(tag) if tag == sub => {}
                _ => continue,
            }
        }
        let msg_val = entry.get("msg").cloned().unwrap_or(serde_json::Value::Null);
        let mut actions_val = entry
            .get("actions")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        normalize_action_shapes(&mut actions_val);
        let replay_msg: Option<workspace::Msg> = if let Some(m) = journal_msg_to_workspace(&msg_val)
        {
            Some(m)
        } else if let Ok(m) = serde_json::from_value::<workspace::Msg>(msg_val.clone()) {
            Some(m)
        } else if let Ok(dm) = serde_json::from_value::<doorbell_tick::DoorbellMsg>(msg_val.clone())
        {
            Some(match dm {
                doorbell_tick::DoorbellMsg::WatchNdjson { line } => {
                    workspace::Msg::WatchNdjson { line }
                }
                doorbell_tick::DoorbellMsg::InboxItems { raw, pin } => {
                    workspace::Msg::InboxItems { raw, pin }
                }
                doorbell_tick::DoorbellMsg::InboxRows { rows, pin } => {
                    workspace::Msg::InboxRows { rows, pin }
                }
                doorbell_tick::DoorbellMsg::PlaneStatus { pin, status } => {
                    workspace::Msg::PlaneStatus { pin, status }
                }
                doorbell_tick::DoorbellMsg::PromptRefused { pin, line } => {
                    workspace::Msg::PromptRefused { pin, line }
                }
                doorbell_tick::DoorbellMsg::Fact { summary } => workspace::Msg::Fact { summary },
                doorbell_tick::DoorbellMsg::AckSlaTick { now } => {
                    workspace::Msg::AckSlaTick { now }
                }
                doorbell_tick::DoorbellMsg::WatchDied => workspace::Msg::WatchDied,
            })
        } else {
            None
        };
        let expected: Vec<workspace::Cmd> =
            if let Ok(v) = serde_json::from_value::<Vec<workspace::Cmd>>(actions_val.clone()) {
                v
            } else if let Ok(legacy) =
                serde_json::from_value::<Vec<doorbell_tick::DoorbellAction>>(actions_val.clone())
            {
                legacy
                    .into_iter()
                    .map(workspace::Cmd::from_doorbell)
                    .collect()
            } else {
                Vec::new()
            };
        let folded = replay_msg.is_some();
        let actual: Vec<workspace::Cmd> = if let Some(msg) = replay_msg.clone() {
            let (next, actual) = {
                let model = models.entry(workspace.clone()).or_insert_with(|| {
                    let mut m = workspace::WorkspaceModel::from_environment();
                    rebuild_assignment_from_receipts(&mut m);
                    seed_why_presence(&mut m, presence_rows.clone());
                    m
                });
                if let Some(pins) = entry.get("pins").and_then(|p| p.as_array()) {
                    for pin in pins.iter().filter_map(|v| v.as_str()) {
                        model.doorbell.index.insert_pin(pin.to_string(), None);
                    }
                }
                workspace::tick(model, &msg)
            };
            if actual != expected {
                eprintln!(
                    "replay mismatch at seq {replay_seq} workspace {workspace}: expected {expected:?} got {actual:?}"
                );
            }
            models.insert(workspace.clone(), next);
            actual
        } else {
            expected.clone()
        };
        replay_seq += 1;
        let dropped = why_dropped_pins(entry);
        let seq_match = seq.map(|s| s == replay_seq - 1).unwrap_or(true);
        let corr_match = match corr {
            Some(c) => {
                models
                    .get(workspace)
                    .map(|m| m.assignment.contains_key(c))
                    .unwrap_or(false)
                    || actual.iter().any(|cmd| match cmd {
                        workspace::Cmd::WaitCorr { corr } => corr == c,
                        workspace::Cmd::SqliteUpsert { corr, .. } => corr == c,
                        _ => false,
                    })
            }
            None => true,
        };
        let pin_match = if corr.is_some() || seq.is_some() {
            true
        } else {
            let entry_pins: Vec<String> = entry
                .get("pins")
                .and_then(|p| p.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let effective: Vec<workspace::Cmd> = actual
                .iter()
                .filter(|c| match c {
                    workspace::Cmd::Prompt { pin: p, .. } => !dropped.contains(p),
                    _ => true,
                })
                .cloned()
                .collect();
            pin.is_none_or(|pin| {
                why_entry_mentions_pin(pin, replay_msg.as_ref(), &effective, &entry_pins)
            })
        };
        if !corr_match || !seq_match || !pin_match {
            continue;
        }
        let ts = entry
            .get("ts")
            .and_then(|t| t.as_str())
            .unwrap_or("?")
            .to_string();
        let ts_us = doorbell_core::parse_rfc3339_micros(&ts).ok();
        if let Some(since_us) = since_us
            && ts_us.is_none_or(|t| t < since_us)
        {
            continue;
        }
        let msg = entry.get("msg").cloned().unwrap_or(serde_json::Value::Null);
        let actions = entry
            .get("actions")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        if let Some(p) = v
                            .get("Prompt")
                            .and_then(|p| p.get("pin"))
                            .and_then(|p| p.as_str())
                            && dropped.iter().any(|d| d == p)
                        {
                            return None;
                        }
                        Some(if v.get("Prompt").is_some() {
                            "prompt"
                        } else if v.get("CheckAckSla").is_some() {
                            "check-ack-sla"
                        } else if v.get("RestartWatch").is_some() {
                            "restart-watch"
                        } else if v.get("FactMiss").is_some() {
                            "fact-miss"
                        } else if v.get("Audit").is_some() {
                            "audit"
                        } else if v.get("Digest").is_some() {
                            "digest"
                        } else if v.get("FetchInbox").is_some() || v.as_str() == Some("FetchInbox")
                        {
                            "fetch-inbox"
                        } else {
                            "unknown"
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let kind = if let Some(rt) = msg.get("rt_kind").and_then(|k| k.as_str()) {
            rt
        } else {
            match msg.get("WatchNdjson") {
                Some(_) => "watch",
                None => match msg.get("PlaneStatus") {
                    Some(_) => "status",
                    None => match msg.get("AckSlaTick") {
                        Some(_) => "ack-sla",
                        None => match msg.get("WatchDied") {
                            Some(_) => "watch-died",
                            None => "other",
                        },
                    },
                },
            }
        };
        rows.push(WhyDisplayRow {
            workspace: workspace.clone(),
            ts,
            ts_us,
            seq: replay_seq - 1,
            kind: kind.to_string(),
            status: why_plane_status(&msg).unwrap_or("").to_string(),
            actions,
            dropped,
            folded,
            console: workspace == DOORBELL_CONSOLE_SCOPE,
        });
    }
    let matched = rows.len();
    let mut workspaces: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for row in &rows {
        *workspaces.entry(row.workspace.clone()).or_default() += 1;
    }
    let (shown, withheld) = why_newest(rows, limit);
    if json {
        let out_rows: Vec<serde_json::Value> = shown
            .iter()
            .map(|row| {
                let actions: Vec<&str> = if row.actions.is_empty() {
                    Vec::new()
                } else {
                    row.actions.split(',').collect()
                };
                serde_json::json!({
                    "workspace": row.workspace,
                    "ts": row.ts,
                    "seq": row.seq,
                    "kind": row.kind,
                    "status": if row.status.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(row.status.clone())
                    },
                    "actions": actions,
                    "dropped": row.dropped,
                    "folded": row.folded,
                    "console": row.console,
                })
            })
            .collect();
        crate::output::print_json(&serde_json::json!({
            "schema": "flywheel-doorbell-why.v1",
            "project": project,
            "pin": pin,
            "limit": limit,
            "matched": matched,
            "shown": shown.len(),
            "withheld": withheld,
            "workspace": workspace_filter,
            "since": since,
            "workspaces": workspaces,
            "rows": out_rows,
        }));
        return Ok(());
    }
    for row in &shown {
        let mut line = format!("{} {} seq={} {}", row.workspace, row.ts, row.seq, row.kind);
        if !row.status.is_empty() {
            line.push_str(&format!(" ->{}", row.status));
        }
        line.push_str(&format!(" actions=[{}]", row.actions));
        if !row.dropped.is_empty() {
            line.push_str(&format!(" dropped=[{}]", row.dropped.join(",")));
        }
        if !row.folded {
            line.push_str(" [journaled]");
        }
        if row.console {
            line.push_str(" [console-inject]");
        }
        println!("{line}");
    }
    if matched == 0 {
        if let Some(c) = corr {
            println!("no journal entries for corr {c} (seq {:?})", seq);
        } else if let Some(s) = seq {
            println!("no journal entry at seq {s}");
        } else {
            match pin {
                Some(pin) => println!(
                    "no doorbell journal entries mention pin {pin} (unaddressed, filtered before classify, or journal empty)"
                ),
                None => println!(
                    "no doorbell journal entries in scope (workspace/since filters may be empty)"
                ),
            }
        }
    } else {
        let ws_summary = workspaces
            .iter()
            .map(|(ws, n)| format!("{ws}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "why: matched {matched}, shown {} of {matched}, withheld {withheld} (limit {limit}); workspaces {ws_summary}",
            shown.len()
        );
        if withheld > 0 {
            println!(
                "why: {withheld} older row(s) withheld — raise --limit or narrow with --workspace/--since"
            );
        }
    }
    Ok(())
}

/// ADR-0010 D5 (bd-65he): `flywheel doorbell afk <project>` — batch the
/// captain-relevant wakes the live dispatcher queued since the last digest.
/// Folds tick from Default over the doorbell journal (same trust surface
/// as `why`), then renders the folded digest_queue with ages from the
/// current clock. Urgent wakes never batch (prompted immediately live),
/// so their presence here is record, not backlog. Wedge alarm when the
/// queue cannot drain (overdue predicate); empty queue is clean, not an
/// error — AFK with nothing queued means nothing needs the captain.
pub(crate) fn cmd_doorbell_afk(cfg: &Config, project: &str, limit: usize) -> Result<()> {
    crate::check_project_spelling(project, "doorbell afk")?;
    let scoped = doorbell_scoped_workspaces(cfg, project);
    let entries = doorbell::journal_read_all(&config::state_dir());
    if entries.is_empty() {
        println!("doorbell afk: no journal entries — nothing queued");
        return Ok(());
    }
    let mut models: std::collections::BTreeMap<String, workspace::WorkspaceModel> =
        std::collections::BTreeMap::new();
    let mut bound: Vec<(String, String, String, String)> = Vec::new();
    for (ws, entry) in &entries {
        if !journal_row_in_scope(ws, entry, &scoped, project) {
            continue;
        }
        if !doorbell::journal_version_accepted(entry) {
            eprintln!(
                "doorbell afk: refusing entry with unknown journal v (workspace {ws}) — no fold-past"
            );
            continue;
        }
        let ts = entry
            .get("ts")
            .and_then(|t| t.as_str())
            .unwrap_or("?")
            .to_string();
        for (pin, line, why) in digest_action_entries(entry) {
            bound.push((ts.clone(), ws.clone(), pin, format!("[{why}] {line}")));
        }
        let msg_val = entry.get("msg").cloned().unwrap_or(serde_json::Value::Null);
        if let Some(msg) = snapshot::replay_msg(&msg_val) {
            let model = models.entry(ws.clone()).or_insert_with(|| {
                let mut m = workspace::WorkspaceModel::from_environment();
                rebuild_assignment_from_receipts(&mut m);
                m
            });
            if let Some(pins) = entry.get("pins").and_then(|p| p.as_array()) {
                for pin in pins.iter().filter_map(|v| v.as_str()) {
                    model.doorbell.index.insert_pin(pin.to_string(), None);
                }
            }
            let (next, _) = workspace::tick(model, &msg);
            *model = next;
        }
    }
    let mut queue: Vec<workspace::DigestEntry> = Vec::new();
    let last_digest_unix: u64 = u64::MAX;
    let mut counters: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for m in models.values() {
        queue.extend(m.wake.digests.iter().cloned());
        for k in ["benign", "routine", "captain", "urgent"] {
            *counters.entry(k).or_insert(0) += m.wake_counters.get(k).copied().unwrap_or(0);
        }
    }
    queue.sort_by_key(|e| e.at_unix);
    let withheld = queue.len().saturating_sub(limit);
    if withheld > 0 {
        queue.drain(..withheld);
    }
    let now = doorbell_now_unix();
    if queue.is_empty() {
        println!("doorbell afk: queue empty — nothing needs the captain");
    } else {
        print!("{}", workspace::render_digest_with_holds(&queue, &[], now));
    }
    if withheld > 0 {
        println!(
            "… {withheld} older entr{} withheld — raise --limit to see them",
            if withheld == 1 { "y" } else { "ies" }
        );
    }
    bound.sort_by(|a, b| a.0.cmp(&b.0));
    if !bound.is_empty() {
        println!("digest-bound wakes ({}):", bound.len());
        let hidden = bound.len().saturating_sub(limit);
        for (ts, ws, pin, line) in bound.iter().skip(hidden) {
            println!("- {ts} {ws} {pin} {line}");
        }
        if hidden > 0 {
            println!("… {hidden} older withheld — raise --limit to see them");
        }
    }
    let counters: Vec<String> = ["benign", "routine", "captain", "urgent"]
        .iter()
        .map(|k| format!("{k}={}", counters.get(*k).copied().unwrap_or(0)))
        .collect();
    println!("wake counters: {}", counters.join(" "));
    if workspace::digest_overdue(queue.len(), last_digest_unix, now) {
        println!(
            "WEDGE ALARM: digest queue cannot drain (stale batch, dispatcher may be down) — inspect `flywheel doorbell status`"
        );
    }
    for line in digest_roster_lines(cfg, project, now) {
        println!("{line}");
    }
    Ok(())
}

/// Scope-bound roster lines for digests (bd-asr2.14, D5/E2). Empty unless
/// the arrival-triple flag opts in — flag-off output stays byte-identical.
pub(crate) fn digest_roster_lines(cfg: &Config, project: &str, now: u64) -> Vec<String> {
    if !cfg.doorbell_arrival_triple() {
        return Vec::new();
    }
    let registry = state::load_spawn_registry(project);
    let pins: Vec<(String, String)> = registry
        .iter()
        .filter_map(|e| {
            e.mail_identity.clone().map(|pin| {
                let harness = e
                    .harness
                    .clone()
                    .or_else(|| e.profile.clone())
                    .unwrap_or_else(|| "?".to_string());
                (pin, harness)
            })
        })
        .collect();
    if pins.is_empty() {
        return Vec::new();
    }
    let roles: std::collections::BTreeMap<String, String> = state::load_role_binds(project)
        .unwrap_or_default()
        .into_iter()
        .map(|(role, pin)| (pin, role))
        .collect();
    let mut presence: Vec<(String, String, u64)> = Vec::new();
    if let Ok(body) = std::fs::read_to_string(crate::mail::presence_path()) {
        let mut latest: std::collections::HashMap<String, (u64, String)> =
            std::collections::HashMap::new();
        for line in body.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let (Some(name), Some(state), Some(ts)) = (
                v.get("name").and_then(|n| n.as_str()),
                v.get("state").and_then(|s| s.as_str()),
                v.get("ts").and_then(|t| t.as_u64()),
            ) {
                latest.insert(name.to_string(), (ts, state.to_string()));
            }
        }
        for (pin, (ts, state)) in latest {
            presence.push((pin, state, ts));
        }
    }
    let mut beads: std::collections::BTreeMap<String, String> = Default::default();
    if let Ok(rows) = crate::br::Bead::in_progress(None) {
        for b in rows {
            if let Some(who) = b.assignee {
                beads.insert(who, b.id);
            }
        }
    }
    doorbell::render_roster(&doorbell::build_roster(
        &pins, &roles, &presence, &beads, now,
    ))
}

pub(crate) fn cmd_doorbell_subscriptions(_cfg: &Config) -> Result<()> {
    use doorbell_rt::subscribe::SubscriptionStore;
    let store = SubscriptionStore::open(&config::state_dir());
    for sub in store.list() {
        println!(
            "{} identity={:?} scope={:?} sink={:?}",
            sub.id, sub.identity, sub.scope, sub.sink
        );
    }
    Ok(())
}

pub(crate) fn cmd_doorbell_unsubscribe(_cfg: &Config, id: &str) -> Result<()> {
    use doorbell_rt::subscribe::{Actor, SubscriptionStore};
    let mut store = SubscriptionStore::open(&config::state_dir());
    match store.unsubscribe(Actor::operator(), id) {
        Ok(row) => {
            println!("unsubscribed: {}", row.id);
            Ok(())
        }
        Err(e) => anyhow::bail!("unsubscribe refused: {e:?}"),
    }
}

/// bd-a8ld/bd-z5ut: the journal workspaces this project owns — `<project>`
/// itself plus every `<project>-<workspace>` the journal dir holds. `None`
/// means "no scoping" (`all`), which the global scan needs.
///
/// The journal dir accumulates workspaces that are not the caller's business:
/// synthetic `manual` / `facts-lapsed-*` / `facts-close-*` rows written by the
/// fact helpers, and other projects' hosts. Folding them made `why` report a
/// mismatch per row and made `afk` print an unbounded digest of another
/// workspace's noise — in both cases burying the answer that was asked for.
///
/// The one exception is the operator console scope. `doorbell inject` writes
/// there by DEFAULT (`--workspace manual`), so excluding it wholesale made the
/// console door unexplainable: `why <pin>` reported "no journal entries" for a
/// row that demonstrably existed. But `manual` is GLOBAL (every project
/// injects into the same file), so including it by name would show another
/// project's injects — trading invisibility for a silent cross-project blend.
/// So the console scope is admitted per ROW, by the project the row records,
/// never by the scope name alone. See [`console_row_is_for_project`].
fn doorbell_scoped_workspaces(cfg: &Config, project: &str) -> Option<Vec<String>> {
    let state_dir = config::state_dir();
    let live_id = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix()).workspace_id;
    doorbell_scope_with_live(
        project,
        doorbell::journal_workspaces_list(&state_dir),
        Some(live_id.as_str()),
        // The live host may be keyed by a plane session ID while the snapshot
        // resolves the canonical label. `project_workspaces` reads each
        // dispatcher's unified state (`state.json`) project, so an independently
        // hosted session is still this project's wake surface.
        doorbell::project_workspaces(&state_dir, project),
    )
}

/// Add the live session workspace and recorded project-owned dispatchers to
/// the label-based project scope. The host may be keyed by the plane session
/// ID (`session:...`), which matches none of the project prefixes; without
/// this `why` cannot explain the live wake it is meant to explain.
fn doorbell_scope_with_live(
    project: &str,
    workspaces: impl IntoIterator<Item = String>,
    live_id: Option<&str>,
    recorded: impl IntoIterator<Item = String>,
) -> Option<Vec<String>> {
    let mut scoped = doorbell_scope(project, workspaces)?;
    if let Some(id) = live_id.filter(|id| !id.trim().is_empty()) {
        scoped.push(id.to_string());
    }
    scoped.extend(recorded);
    scoped.sort();
    scoped.dedup();
    Some(scoped)
}

/// The workspace labels that belong to `project`: the bare project key, the
/// canonical `flywheel-{project}` dispatcher, and their children. The live
/// dispatcher is `flywheel-toron-rk-...`, which the old `{project}-` prefix
/// missed entirely; keeping this pure makes the scope testable without a
/// live journal directory.
fn doorbell_scope(
    project: &str,
    workspaces: impl IntoIterator<Item = String>,
) -> Option<Vec<String>> {
    (project != "all").then(|| {
        let canonical = format!("flywheel-{project}");
        let project_prefix = format!("{project}-");
        let canonical_prefix = format!("{canonical}-");
        let mut w = vec![project.to_string(), canonical.clone()];
        w.extend(workspaces.into_iter().filter(|x| {
            x.as_str() == project
                || x == &canonical
                || x.starts_with(&project_prefix)
                || x.starts_with(&canonical_prefix)
        }));
        w.sort();
        w.dedup();
        w
    })
}

/// The console scope `doorbell inject` defaults to. Shared by every project,
/// so it is scoped by the recorded project, never by name.
pub(crate) const DOORBELL_CONSOLE_SCOPE: &str = "manual";

/// True when a row in the shared console scope was written FOR this project.
/// Only console-inject rows qualify: they carry `executed.project`. A row
/// with no recorded project is refused (fail-closed) rather than assumed ours.
fn console_row_is_for_project(entry: &serde_json::Value, project: &str) -> bool {
    entry
        .get("executed")
        .and_then(|e| e.get("project"))
        .and_then(|p| p.as_str())
        .is_some_and(|p| p == project)
}

/// The one scope test `why` and `afk` share, so the two can never disagree
/// about what a project owns.
///
/// A row is in scope when its workspace is the project's own, OR when it sits
/// in the shared console scope and records this project. `scoped = None`
/// means `all` (no scoping). The console arm is deliberately per-ROW: the
/// scope name is global, so admitting it wholesale would blend projects.
fn journal_row_in_scope(
    workspace: &str,
    entry: &serde_json::Value,
    scoped: &Option<Vec<String>>,
    project: &str,
) -> bool {
    let Some(allowed) = scoped else { return true };
    if allowed.iter().any(|w| w == workspace) {
        return true;
    }
    workspace == DOORBELL_CONSOLE_SCOPE && console_row_is_for_project(entry, project)
}

#[cfg(test)]
mod journal_scope_tests {
    use super::*;

    fn inject_row(project: &str) -> serde_json::Value {
        serde_json::json!({
            "msg": {"Fact": {"summary": format!("inject P: hello ({project})")}},
            "pins": ["P"],
            "executed": {"stage": "inject", "pin": "P", "project": project},
            "workspace": DOORBELL_CONSOLE_SCOPE,
        })
    }

    fn scoped_to(project: &str) -> Option<Vec<String>> {
        Some(vec![project.to_string(), format!("{project}-crew")])
    }

    /// The console door must be explainable: a default inject lands in the
    /// shared `manual` scope, and `why` still has to see it.
    #[test]
    fn console_inject_row_is_in_scope_for_its_own_project() {
        let row = inject_row("proj-a");
        assert!(journal_row_in_scope(
            DOORBELL_CONSOLE_SCOPE,
            &row,
            &scoped_to("proj-a"),
            "proj-a"
        ));
    }

    /// The reason the console arm filters per ROW rather than by scope name:
    /// `manual` is global, so admitting it wholesale would show another
    /// project's injects — a silent cross-project blend.
    #[test]
    fn console_row_from_another_project_is_refused() {
        let row = inject_row("proj-b");
        assert!(
            !journal_row_in_scope(DOORBELL_CONSOLE_SCOPE, &row, &scoped_to("proj-a"), "proj-a"),
            "proj-b's inject must not answer a proj-a question"
        );
    }

    /// Fail-closed: a console row with no recorded project is NOT assumed
    /// ours. Refusing is the safe direction; guessing would blend projects.
    #[test]
    fn console_row_without_recorded_project_is_refused() {
        let row = serde_json::json!({"msg": {"Fact": {"summary": "inject P: legacy"}}});
        assert!(!journal_row_in_scope(
            DOORBELL_CONSOLE_SCOPE,
            &row,
            &scoped_to("proj-a"),
            "proj-a"
        ));
    }

    /// The live dispatcher is `flywheel-<project>-rk-...`; the canonical
    /// prefix must claim it, while a sibling project stays out.
    #[test]
    fn canonical_flywheel_workspace_and_children_are_in_scope() {
        let scoped = doorbell_scope(
            "toron",
            vec![
                "flywheel-toron".to_string(),
                "flywheel-toron-rk-live-doorbell".to_string(),
                "toron-crew".to_string(),
                "flywheel-other".to_string(),
                "other-crew".to_string(),
            ],
        )
        .expect("project scope");
        assert!(
            scoped
                .iter()
                .any(|w| w == "flywheel-toron-rk-live-doorbell")
        );
        assert!(scoped.iter().any(|w| w == "toron-crew"));
        assert!(!scoped.iter().any(|w| w == "flywheel-other"));
        assert!(!scoped.iter().any(|w| w == "other-crew"));
    }

    /// The live host is keyed by its plane session ID, not the project
    /// prefix. `why` must still see that workspace's status wake.
    #[test]
    fn live_session_workspace_is_added_to_the_scope() {
        let scoped = doorbell_scope_with_live(
            "toron",
            vec!["flywheel-toron-rk-live".to_string(), "other".to_string()],
            Some("session:abc"),
            Vec::<String>::new(),
        )
        .expect("project scope");
        assert!(scoped.iter().any(|w| w == "flywheel-toron-rk-live"));
        assert!(scoped.iter().any(|w| w == "session:abc"));
        assert!(!scoped.iter().any(|w| w == "other"));
        let without = doorbell_scope_with_live("toron", vec![], None, Vec::<String>::new())
            .expect("project scope");
        assert!(!without.iter().any(|w| w.starts_with("session:")));
    }

    /// The snapshot can resolve the canonical label while a separate session
    /// host owns the status journal. The recorded project dispatcher must be
    /// admitted even when the live ID is not the session.
    #[test]
    fn recorded_project_workspace_is_added_when_live_id_is_canonical() {
        let scoped = doorbell_scope_with_live(
            "toron",
            vec!["flywheel-toron-rk-live".to_string()],
            Some("flywheel-toron-rk-live"),
            vec!["session:01a1".to_string()],
        )
        .expect("project scope");
        assert!(scoped.iter().any(|w| w == "session:01a1"));
        assert!(scoped.iter().any(|w| w == "flywheel-toron-rk-live"));
    }

    /// The original exclusion (bd-a8ld) must survive: the synthetic
    /// facts-lapsed-* / facts-close-* scopes are still not ours.
    #[test]
    fn synthetic_fact_scopes_stay_excluded() {
        let row = serde_json::json!({"msg": {"Fact": {"summary": "closed"}}});
        for ws in ["facts-lapsed-1", "facts-close-1", "some-other-project"] {
            assert!(
                !journal_row_in_scope(ws, &row, &scoped_to("proj-a"), "proj-a"),
                "{ws} must stay out of proj-a's scope"
            );
        }
    }

    /// `all` means no scoping — the global scan still reads everything.
    #[test]
    fn no_scoping_admits_every_row() {
        let row = serde_json::json!({"msg": {"Fact": {"summary": "x"}}});
        assert!(journal_row_in_scope("anything", &row, &None, "proj-a"));
    }
}

/// bd-a8ld: rewrite unit doorbell actions written in their pre-bd-z5ou
/// object form (`{"FetchInbox":{"since":…}}`) to the current unit string.
/// Only the three unit variants are touched: `Prompt`/`Audit`/`FactMiss`/
/// `Digest` are struct variants in BOTH encodings and must stay objects.
pub(crate) fn normalize_action_shapes(actions: &mut serde_json::Value) {
    let Some(arr) = actions.as_array_mut() else {
        return;
    };
    for a in arr.iter_mut() {
        for unit in ["FetchInbox", "CheckAckSla", "RestartWatch"] {
            let payloaded = a.get(unit).is_some_and(|v| !v.is_boolean() && !v.is_null());
            if payloaded {
                *a = serde_json::Value::String(unit.to_string());
                break;
            }
        }
    }
}

/// bd-bk9t: the `(pin, line, why)` of a journal row's `Digest` actions, in
/// order. `route_or_queue` emits one per unroutable-pin wake, so a row can
/// carry several. Non-`Digest` actions are ignored.
///
/// bd-z5ut: `afk` reads these too. A digest-bound wake is never produced by
/// the embedded fold (`workspace::tick` has no `InboxFetched` arm, which is
/// bd-c9jw), so without reading the rows the captain's batch surface missed
/// exactly the wake that had just arrived.
pub(crate) fn digest_action_entries(entry: &serde_json::Value) -> Vec<(String, String, String)> {
    entry
        .get("actions")
        .and_then(|a| a.as_array())
        .map(|actions| {
            actions
                .iter()
                .filter_map(|a| {
                    let d = a.get("Digest")?;
                    Some((
                        d.get("pin")?.as_str()?.to_string(),
                        d.get("line")?.as_str()?.to_string(),
                        d.get("why")
                            .and_then(|w| w.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod bk9t_digest_line_tests {
    use super::digest_action_entries;

    #[test]
    fn reads_only_digest_entries_in_order() {
        let entry = serde_json::json!({
            "actions": [
                {"Audit": {"kind": "queued", "pin": "OtherPin", "detail": "x"}},
                {"Digest": {"pin": "OperatorPin", "line": "[doorbell] plane-P urgent", "why": "no_route"}},
                {"FetchInbox": 0},
                {"Digest": {"pin": "OperatorPin", "line": "[doorbell] channel", "why": "no_route"}},
            ]
        });
        assert_eq!(
            digest_action_entries(&entry),
            vec![
                (
                    "OperatorPin".to_string(),
                    "[doorbell] plane-P urgent".to_string(),
                    "no_route".to_string()
                ),
                (
                    "OperatorPin".to_string(),
                    "[doorbell] channel".to_string(),
                    "no_route".to_string()
                ),
            ],
            "a digest-bound wake must be readable from the row, in order"
        );
    }

    #[test]
    fn rows_without_a_digest_action_are_empty() {
        let entry = serde_json::json!({"actions": ["FetchInbox"]});
        assert_eq!(
            digest_action_entries(&entry),
            [] as [(String, String, String); 0]
        );
        assert_eq!(
            digest_action_entries(&serde_json::json!({})),
            [] as [(String, String, String); 0]
        );
    }
}

#[cfg(test)]
mod action_shape_tests {
    use super::normalize_action_shapes;

    #[test]
    fn legacy_object_shaped_unit_actions_become_unit_strings() {
        let mut v = serde_json::json!([
            {"FetchInbox": {"since": "2026-09-10T00:57:23Z"}},
            "CheckAckSla",
            {"Prompt": {"pin": "P", "line": "x"}},
            {"Digest": {"pin": "P", "line": "l", "why": "no_route"}},
        ]);
        normalize_action_shapes(&mut v);
        assert_eq!(v[0], serde_json::json!("FetchInbox"));
        assert_eq!(v[1], serde_json::json!("CheckAckSla"));
        assert_eq!(
            v[2],
            serde_json::json!({"Prompt": {"pin": "P", "line": "x"}})
        );
        assert_eq!(
            v[3],
            serde_json::json!({"Digest": {"pin": "P", "line": "l", "why": "no_route"}})
        );
    }

    #[test]
    fn non_array_actions_are_left_alone() {
        let mut v = serde_json::json!({"FetchInbox": {"since": "x"}});
        normalize_action_shapes(&mut v);
        assert!(
            v.get("FetchInbox").is_some(),
            "a non-array actions field must not be rewritten"
        );
    }
}

/// Journal rows use the daemon's `rt_kind` envelope; serde's externally-tagged
/// `Msg`/`DoorbellMsg` forms are only the legacy inputs. Parse the live shape
/// here so `why` can replay the row that actually woke the pin.
fn why_plane_status(v: &serde_json::Value) -> Option<&str> {
    v.get("PlaneStatus")
        .and_then(|h| h.get("status"))
        .and_then(|s| s.as_str())
        .or_else(|| v.get("status").and_then(|s| s.as_str()))
}

fn journal_msg_to_workspace(v: &serde_json::Value) -> Option<workspace::Msg> {
    let text = |key: &str| v.get(key).and_then(|x| x.as_str()).map(str::to_string);
    match text("rt_kind")?.as_str() {
        "PlaneStatus" => Some(workspace::Msg::PlaneStatus {
            pin: text("pin")?,
            status: text("status")?,
        }),
        "RoleBound" => Some(workspace::Msg::RoleBound {
            role: text("role")?,
            pin: text("pin")?,
        }),
        "Fact" => Some(workspace::Msg::Fact {
            summary: text("summary")?,
        }),
        "AckSlaTick" => Some(workspace::Msg::AckSlaTick {
            now: v.get("now")?.as_u64()?,
        }),
        "Presence" => Some(workspace::Msg::Presence {
            pin: text("pin")?,
            state: text("state")?,
        }),
        "SlaFired" => Some(workspace::Msg::SlaFired {
            posts: v
                .get("posts")?
                .as_array()?
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect(),
        }),
        "WatchDied" => Some(workspace::Msg::WatchDied),
        _ => None,
    }
}

fn why_entry_mentions_pin(
    pin: &str,
    msg: Option<&workspace::Msg>,
    cmds: &[workspace::Cmd],
    entry_pins: &[String],
) -> bool {
    let cmd_hit = cmds.iter().any(|c| match c {
        workspace::Cmd::Prompt { pin: p, .. } => p == pin,
        workspace::Cmd::Audit { pin: p, .. } => p == pin,
        _ => false,
    });
    if cmd_hit {
        return true;
    }
    if entry_pins.iter().any(|p| p == pin) {
        return true;
    }
    let Some(msg) = msg else { return false };
    match msg {
        workspace::Msg::InboxItems { pin: p, .. }
        | workspace::Msg::InboxRows { pin: p, .. }
        | workspace::Msg::PlaneStatus { pin: p, .. } => p == pin,
        workspace::Msg::WatchNdjson { line } => line.contains(pin),
        _ => false,
    }
}

#[cfg(test)]
mod timeout_presence_tests {
    #[test]
    fn timeout_arm_ticks_presence_not_only_hear() {
        let host = include_str!("../doorbell_host.rs");
        assert!(
            host.contains("presence.jsonl") && host.contains("Msg::Presence"),
            "RED (55nx.5): sweep must project presence.jsonl rows into tick Msg::Presence"
        );
        let brain = include_str!("../../crates/doorbell-rt/src/brain.rs");
        assert!(
            brain.contains("sweep_maintenance") && brain.contains("BrainMsg::Presence"),
            "RED (55nx.5): brain must tick Presence rows every sweep, not hear-only"
        );
    }

    #[test]
    fn presence_rows_keep_fresh_states_latest_wins() {
        let now = 1_000_000u64;
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::json!({"name": "ZebraPin", "state": "idle", "ts": now - 10}),
            serde_json::json!({"name": "BlueRidge", "state": "idle", "ts": now - 100}),
            serde_json::json!({"name": "BlueRidge", "state": "working", "ts": now - 5}),
            serde_json::json!({"name": "LilacChapel", "state": "working", "ts": now - 500}),
        );
        let rows = super::timeout_presence_rows(&body, now);
        assert_eq!(
            rows,
            vec![
                ("BlueRidge".to_string(), "working".to_string()),
                ("ZebraPin".to_string(), "idle".to_string()),
            ],
            "latest fresh row wins; stale rows drop; pin order is deterministic (bd-bpj1 F2): {rows:?}"
        );
    }

    #[test]
    fn presence_tick_working_blocks_prompt_without_20001() {
        let (next, _) = crate::workspace::tick(
            &crate::workspace::WorkspaceModel::default(),
            &crate::workspace::Msg::Presence {
                pin: "BlueRidge".into(),
                state: "working".into(),
            },
        );
        assert_eq!(
            next.presence.get("BlueRidge"),
            Some(&crate::workspace::Presence::from_str("working")),
            "jsonl working must enter Model.presence even with no 20001 seen"
        );
    }
}

/// Parse presence.jsonl body into fresh (pin, state) rows for the timeout path.
/// Latest row per name wins; only rows <30s old are fresh. State comes from the
/// row's `state` field (the Kind 20001 projection); rows without one default to
/// `unknown`, which never prompts.
pub(crate) fn timeout_presence_rows(body: &str, now: u64) -> Vec<(String, String)> {
    let mut latest: std::collections::BTreeMap<String, (u64, String)> =
        std::collections::BTreeMap::new();
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(name), Some(ts)) = (
            v.get("name").and_then(|n| n.as_str()),
            v.get("ts").and_then(serde_json::Value::as_u64),
        ) else {
            continue;
        };
        let state = v
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        latest
            .entry(name.to_string())
            .and_modify(|e| {
                if ts > e.0 {
                    *e = (ts, state.clone());
                }
            })
            .or_insert((ts, state));
    }
    latest
        .into_iter()
        .filter(|(_, (ts, _))| now.saturating_sub(*ts) < 30)
        .map(|(pin, (_, state))| (pin, state))
        .collect()
}

/// Spawn the CLI `toron watch` plane-S child + its NDJSON reader thread.
/// Shared by the initial fallback and both respawn paths (Died burst +
/// 1s retry gate) so every restart uses the SAME kinds and wiring.
/// Kinds MUST match the in-process filter (presence 20001 + profile 30013).
const DOORBELL_WATCH_KINDS: &str = "9,14,20001,30013,43001,30031,39002";

/// Wake-bus socket consumer (bd-qe1r wave 2) — replaces the `toron watch`
/// child: subscribes the daemon `wake.sock`, outcomes map onto `BrainMsg`
/// at the call site (mapping byte-identical by the doorbell-rt golden test).
/// GS062-ADV-5 semantics (try_send Watch / Died blocking / stamp-after-Ok)
/// live in the call-site mapper (`wake_outcome_to_brain` / FileHost restart).
pub(crate) fn spawn_wake_consumer<F>(
    project: &str,
    workspace: &str,
    state_dir: &std::path::Path,
    resume_from: Option<String>,
    retired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    on_event: F,
) -> (
    tokio::task::AbortHandle,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
)
where
    F: Fn(doorbell_rt::wake_sock::WakeOutcome) + Send + 'static,
{
    use doorbell_rt::wake_sock::{self, WakeOpts};
    let mut opts = WakeOpts::new(
        wake_sock::wake_sock_path(),
        wake_sock::parse_kinds(DOORBELL_WATCH_KINDS),
        Some(project.to_string()),
    );
    opts.resume_from = resume_from;
    let stored = std::sync::Arc::clone(&retired);
    opts.retired = retired;
    let state_dir = state_dir.to_path_buf();
    let workspace = workspace.to_string();
    let handle = wake_sock::spawn_wake_task(
        opts,
        move |kind, detail| {
            doorbell::audit_append(
                &state_dir,
                &workspace,
                serde_json::json!({"kind": kind, "detail": detail}),
            );
        },
        on_event,
    );
    (handle, stored)
}

/// Pins plus npubs so live kind-9 `p` tags (hex) resolve to the pin.
pub(crate) fn doorbell_self_index(
    cfg: &Config,
    project: &str,
    pins: &[String],
) -> doorbell::SelfIndex {
    let mut index = doorbell::SelfIndex::from_pins(pins.iter().cloned());
    let Ok(key) = project_dir_str(cfg, project) else {
        return index;
    };
    let Ok(mail) = backend::mail_backend(cfg) else {
        return index;
    };
    for pin in pins {
        if let Ok(w) = backend::run_mail(mail.whois(&key, pin))
            && !w.npub.is_empty()
        {
            index.insert_pin(pin.clone(), Some(w.npub));
        }
    }
    index
}

pub(crate) fn doorbell_now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// bd-8vci: sleep to the next wall-clock second boundary (bounded, <1s) so a
/// re-stamp lands strictly after the prior same-second wake stamp.
fn sleep_to_next_second() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    std::thread::sleep(Duration::from_nanos(
        1_000_000_000 - u64::from(now.subsec_nanos()),
    ));
}

/// GS-039 P0-1: stamp last_wake from a REAL observation without clobbering
/// plane-P/S high-waters (refresh/sweep callers lack the live since map).
/// Advance only when the observation is new/meaningful: wall clock moved
/// strictly past the prior stamp. Same-second re-notes must not bump the
/// prior stamp by one — that let background polls ratchet `last_wake` without
/// a fresh observation (invent-adjacent). bd-8vci: a refused same-second
/// stamp retries ONCE after sleeping to the next second boundary, so two
/// sends landing in one wall second still advance the stamp for the caller's
/// wake-advance gate; a second refusal leaves the caller's bail standing.
pub(crate) fn doorbell_note_observed_wake(workspace: &str, pin: &str) -> Result<()> {
    let path = doorbell::wake_state_file(&config::state_dir(), workspace);
    let mut st = doorbell::load_wake_state(&path);
    let mut now = doorbell_now_unix();
    if let Some(prev) = st.last_wake_unix
        && now <= prev
    {
        sleep_to_next_second();
        now = doorbell_now_unix();
        st = doorbell::load_wake_state(&path);
        if let Some(prev) = st.last_wake_unix
            && now <= prev
        {
            return Ok(());
        }
    }
    doorbell::note_wake(&mut st, pin, now);
    doorbell::save_wake_state(&path, &st).with_context(|| {
        format!(
            "doorbell observed-wake save failed at {} — refuse silent drop (GS-039 P0-1)",
            path.display()
        )
    })?;
    Ok(())
}

fn upsert_workspace_receipt(corr: &str, receipt: &workspace::Receipt) {
    let state = match receipt.state.as_str() {
        "owed" => toron_core::mailbox::receipts::ReceiptState::Owed,
        "acked" => toron_core::mailbox::receipts::ReceiptState::Acked,
        "resulted" => toron_core::mailbox::receipts::ReceiptState::Resulted,
        "degraded" => toron_core::mailbox::receipts::ReceiptState::Degraded,
        _ => return,
    };
    let wake = match receipt.wake.as_str() {
        "standing" => Some(toron_core::mailbox::send_tags::Wake::Standing),
        "tasked" => Some(toron_core::mailbox::send_tags::Wake::Tasked),
        _ => None,
    };
    let r = toron_core::mailbox::receipts::Receipt {
        corr: corr.to_string(),
        state,
        wake,
        due_at: receipt.due_at.map(|v| v as i64),
        result_path: receipt.result_path.clone(),
        updated_at: chrono::Utc::now().timestamp_millis(),
    };
    // The embedded host's `tick` runs on its Tokio runtime; the receipt
    // write synchronously block_on's the shared mail runtime, so it must be
    // moved off that thread or the host panics at startup.
    let _ = crate::backend::run_mail_off_runtime_sync(|| {
        crate::mail::block_on(async {
            if let Some(mb) = crate::mail::try_mailbox()
                && let Ok(store) = mb.receipt_store()
            {
                let _ = store.upsert(&r).await;
            }
            Ok::<(), anyhow::Error>(())
        })
    });
}

/// Pins the journal effect marks dropped for one entry (`executed.effect`).
/// Absent effect (legacy lines, unbudgeted paths) means no drops.
fn why_dropped_pins(entry: &serde_json::Value) -> Vec<String> {
    entry
        .get("executed")
        .and_then(|e| e.get("effect"))
        .and_then(|f| f.get("dropped"))
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn handle_workspace_cmds(
    workspace_model: &mut workspace::WorkspaceModel,
    cmds: &[workspace::Cmd],
    project: &str,
    cfg: &Config,
    drain_waits: bool,
) -> bool {
    let mut fetch_inbox = false;
    for cmd in cmds {
        match cmd {
            workspace::Cmd::SqliteUpsert { corr, receipt } => {
                upsert_workspace_receipt(corr, receipt);
            }
            workspace::Cmd::FetchInbox => {
                fetch_inbox = true;
            }
            workspace::Cmd::WaitCorr { .. } => {}
            workspace::Cmd::Publish {
                kind,
                content,
                corr,
                result_path,
            } => {
                publish_human_question(project, *kind, content, corr, result_path.as_deref());
            }
            workspace::Cmd::FactMiss { why } => {
                eprintln!("flywheel: [FactMiss] {why}");
            }
            workspace::Cmd::LoudMiss { why } => {
                eprintln!("flywheel: [LoudMiss] {why}");
            }
            workspace::Cmd::Invoke {
                workflow,
                input,
                corr,
            } => {
                let mut inputs = input.clone();
                if let Some(obj) = inputs.as_object_mut() {
                    obj.insert("corr".into(), serde_json::json!(corr));
                }
                let run = WorkflowCmd::Run {
                    name: workflow.clone(),
                    inputs: Some(inputs.to_string()),
                    crew: None,
                    execute: true,
                    mode: "pane".to_string(),
                    project: Some(project.to_string()),
                    reuse: None,
                    retain: false,
                    retire_orphans: false,
                    json: true,
                    yes: true,
                    allow_worktree: false,
                    role_model: vec![],
                    role_harness: vec![],
                };
                if let Err(e) = cmd_workflow(cfg, &run) {
                    eprintln!("flywheel: [LoudMiss] invoke {corr} ({workflow}): {e:#}");
                }
            }
            workspace::Cmd::Prompt { .. }
            | workspace::Cmd::CheckAckSla
            | workspace::Cmd::RestartWatch => {}
            workspace::Cmd::Audit { kind, pin, detail } => {
                eprintln!("flywheel: [Audit {kind}] {pin}: {detail}");
            }
            workspace::Cmd::Digest { entries } => {
                let holds = workspace::sorted_holds(workspace_model);
                let now = chrono::Utc::now().timestamp() as u64;
                eprintln!(
                    "flywheel: [Digest]\n{}",
                    workspace::render_digest_with_holds(entries, &holds, now)
                );
            }
            workspace::Cmd::Send { .. }
            | workspace::Cmd::PinStamp { .. }
            | workspace::Cmd::DispatchImmediate { .. }
            | workspace::Cmd::Blocked { .. }
            | workspace::Cmd::Nudge { .. }
            | workspace::Cmd::Diagnose { .. }
            | workspace::Cmd::Respawn { .. }
            | workspace::Cmd::Reclaim { .. }
            | workspace::Cmd::Wait => {}
            workspace::Cmd::RebindBinding {
                pin,
                profile_name,
                model,
                harness,
                effect_key,
                note,
            } => {
                eprintln!(
                    "flywheel: [RebindBinding] effect_key={effect_key} pin={pin} note={note}"
                );
                let _ = cmd_binding_rebind(
                    cfg,
                    project,
                    pin,
                    profile_name.as_deref(),
                    model.as_deref(),
                    harness.as_deref(),
                    Some(note.as_str()),
                );
            }
            workspace::Cmd::Reclassify {
                goal,
                pin,
                explicit_topology,
                reason,
                effect_key,
            } => {
                eprintln!(
                    "flywheel: [Reclassify] effect_key={effect_key} reason={reason} goal_len={} sticky_explicit={:?}",
                    goal.len(),
                    explicit_topology
                );
                let snap = live_decide_snapshot(cfg, project);
                let more = workspace::expand_reclassify_cmds(
                    workspace_model,
                    vec![workspace::Cmd::Reclassify {
                        goal: goal.clone(),
                        pin: pin.clone(),
                        explicit_topology: explicit_topology.clone(),
                        reason: reason.clone(),
                        effect_key: effect_key.clone(),
                    }],
                    project,
                    &crate::authority::RuntimeAuthority::resolve(),
                    &|g| {
                        let d = crate::decide::decide(g, &snap);
                        (
                            d.primitive.to_string(),
                            d.topology.to_string(),
                            d.argv.clone(),
                            d.reason.clone(),
                        )
                    },
                );
                if let Some(topo) = workspace_model.session_topology.as_deref() {
                    let _ = state::save_session_topology(project, topo);
                }
                let _ = handle_workspace_cmds(workspace_model, &more, project, cfg, false);
            }
        }
    }
    if drain_waits {
        drain_pending_wait_corrs(workspace_model);
    }
    fetch_inbox
}

fn drain_pending_wait_corrs(workspace_model: &mut workspace::WorkspaceModel) {
    let pending: Vec<String> = workspace_model.wake.waits.keys().cloned().collect();
    for corr in pending {
        if try_tick_receipt(workspace_model, &corr) {
            workspace_model.wake.waits.remove(&corr);
        }
    }
}

fn try_tick_receipt(workspace_model: &mut workspace::WorkspaceModel, corr: &str) -> bool {
    let Some(mb) = crate::mail::try_mailbox() else {
        return false;
    };
    let Ok(store) = mb.receipt_store() else {
        return false;
    };
    let got = crate::backend::run_mail_off_runtime_sync(|| crate::mail::block_on(store.get(corr)));
    let Ok(Some(r)) = got else {
        return false;
    };
    let rec = workspace::Receipt {
        state: r.state.as_str().to_string(),
        wake: r
            .wake
            .map(|w| w.as_str().to_string())
            .unwrap_or_else(|| "tasked".into()),
        due_at: r.due_at.map(|v| v as u64),
        result_path: r.result_path.clone(),
    };
    let msg = workspace::Msg::Receipt {
        corr: corr.to_string(),
        receipt: rec,
    };
    let (next, extra) = workspace::tick(workspace_model, &msg);
    *workspace_model = next;
    for c in extra {
        if let workspace::Cmd::SqliteUpsert { corr, receipt } = c {
            upsert_workspace_receipt(&corr, &receipt);
        }
    }
    true
}
#[cfg(test)]
mod waitcorr_doorbell_only_tests {
    #[test]
    fn waitcorr_arm_names_cli_equivalent_and_sweep_wake() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn handle_workspace_cmds(").expect("handler");
        let end = src[pos..]
            .find("\nfn drain_pending_wait_corrs")
            .map(|i| pos + i)
            .unwrap_or(src.len());
        let slice = &src[pos..end];
        assert!(
            slice.contains("workspace::Cmd::WaitCorr"),
            "WaitCorr arm must be present in the handler"
        );
        assert!(
            slice.contains("drain_pending_wait_corrs(workspace_model)"),
            "drain fast-path must live in the handler"
        );
        assert!(
            slice.contains("drain_waits"),
            "E2: the peek is gated by drain_waits"
        );
        let host = include_str!("../doorbell_host.rs");
        assert!(
            host.contains("BrainMsg::Sweep { now } => (workspace::Msg::Sweep { now: *now }, true)"),
            "ADR-0011 D7: WaitCorr also drains on Sweep, not only WatchNdjson"
        );
    }

    #[test]
    fn invoke_arm_hands_off_to_durable_run_gate() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn handle_workspace_cmds(").expect("handler");
        let end = src[pos..]
            .find("\nfn drain_pending_wait_corrs")
            .map(|i| pos + i)
            .unwrap_or(src.len());
        let slice = &src[pos..end];
        assert!(
            slice.contains("Cmd::Invoke"),
            "Invoke arm must be applied in the handler"
        );
        assert!(
            slice.contains("cmd_workflow(cfg, &run)"),
            "Invoke must route through cmd_workflow (the durable Run gate)"
        );
        assert!(
            slice.contains("\"corr\".into(), serde_json::json!(corr)"),
            "Invoke inputs must carry the corr for attribution"
        );
        assert!(
            slice.contains("[LoudMiss] invoke"),
            "Invoke failure must be LoudMiss, never silent"
        );
    }

    #[test]
    fn waitcorr_never_drains_on_swarm_fold() {
        let durable = include_str!("../durable.rs");
        assert!(
            !durable.contains("WaitCorr"),
            "swarm fold must not produce/drain WaitCorr; doorbell owns pending_wait"
        );
        assert!(
            !durable.contains("pending_wait"),
            "pending_wait lives on the doorbell path only"
        );
    }
}

#[cfg(test)]
mod runtime_mail_tests {
    use super::*;

    /// The embedded host calls this from its Tokio runtime. Before the
    /// off-runtime wrapper, the receipt write panicked at startup and the
    /// live dispatcher never wrote a status wake.
    #[test]
    fn receipt_effects_do_not_nest_block_on_inside_a_runtime() {
        let _home = crate::config::test_state_home();
        let _mail = crate::mail::Mail::from_config(None)
            .expect("in-process mail")
            .expect("some");
        let receipt = workspace::Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: None,
            result_path: None,
        };
        let mut model = workspace::WorkspaceModel::default();
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async {
            let _ = handle_workspace_cmds(
                &mut model,
                &[workspace::Cmd::SqliteUpsert {
                    corr: "runtime-mail-receipt".into(),
                    receipt,
                }],
                "runtime-mail",
                &Config::default(),
                false,
            );
            assert!(!try_tick_receipt(&mut model, "missing-corr"));
        });
    }
}

#[cfg(test)]
mod s_proof_tests {
    #[test]
    fn s1_rebuild_reads_sqlite_into_assignment() {
        let src = include_str!("doorbell.rs");
        let pos = src
            .find("fn rebuild_assignment_from_receipts(")
            .expect("rebuild");
        let end = src[pos..]
            .find("\nfn unpinned_registry_names(")
            .map(|i| pos + i)
            .unwrap_or(src.len());
        let slice = &src[pos..end];
        assert!(
            slice.contains("receipt_store") && slice.contains(".list("),
            "S1: rebuild must read the receipts sqlite store (receipt_store list)"
        );
        assert!(
            slice.contains("assignment.insert"),
            "S1: every listed receipt must land in Model.assignment (no drop filter)"
        );
    }

    #[test]
    fn s1_receipt_tick_writes_assignment_and_upsert() {
        let (next, cmds) = crate::workspace::tick(
            &crate::workspace::WorkspaceModel::default(),
            &crate::workspace::Msg::Receipt {
                corr: "FM-S1".into(),
                receipt: crate::workspace::Receipt {
                    state: "owed".into(),
                    wake: "tasked".into(),
                    due_at: None,
                    result_path: None,
                },
            },
        );
        assert!(
            next.assignment.contains_key("FM-S1"),
            "S1: Receipt tick must record the corr in assignment"
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                crate::workspace::Cmd::SqliteUpsert { corr, .. } if corr == "FM-S1"
            )),
            "S1: Receipt tick must emit SqliteUpsert, got {cmds:?}"
        );
    }

    #[test]
    fn s2_standing_empty_silence() {
        let (next, cmds) = crate::workspace::tick(
            &crate::workspace::WorkspaceModel::default(),
            &crate::workspace::Msg::WatchNdjson {
                line: "not json, no ring".into(),
            },
        );
        assert!(
            cmds.is_empty(),
            "S2: unrung line must emit no Cmds, got {cmds:?}"
        );
        assert!(
            next.assignment.is_empty(),
            "S2: unrung line must not write assignment"
        );
    }

    #[test]
    fn why_fold_has_corr_seq_filters_from_default() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn cmd_doorbell_why(").expect("why");
        let end = src[pos..]
            .find("\nfn cmd_loop_why(")
            .map(|i| pos + i)
            .unwrap_or(src.len());
        let slice = &src[pos..end];
        assert!(
            slice.contains("WorkspaceModel::default()"),
            "why must fold tick from Default"
        );
        assert!(
            slice.contains("workspace::tick(&model, &msg)")
                || slice.contains("workspace::tick(&model, &msg,"),
            "why must reproduce Cmds by folding tick"
        );
        for needle in ["corr", "seq"] {
            assert!(
                slice.contains(needle),
                "why must carry a {needle} filter bound, not pin-substring grep"
            );
        }
    }

    #[test]
    fn why_shows_daemon_inbox_wake() {
        let entry_pins = vec!["PearlTern".to_string()];
        assert!(
            super::why_entry_mentions_pin("PearlTern", None, &[], &entry_pins),
            "daemon inbox row attributed via journal pins must match"
        );
        assert!(
            !super::why_entry_mentions_pin("BrightCoast", None, &[], &entry_pins),
            "other pins must not match"
        );
        assert!(
            !super::why_entry_mentions_pin("PearlTern", None, &[], &[]),
            "legacy rows without pins stay invisible, never invented"
        );
        let live = serde_json::json!({
            "rt_kind": "PlaneStatus", "pin": "PearlTern", "status": "idle"
        });
        let msg = super::journal_msg_to_workspace(&live)
            .expect("the live rt_kind row must replay as a status wake");
        assert!(
            super::why_entry_mentions_pin("PearlTern", Some(&msg), &[], &[]),
            "a status transition is a wake and must match its pin"
        );
        assert!(
            super::journal_msg_to_workspace(&serde_json::json!({
                "rt_kind": "PlaneStatus", "pin": "PearlTern"
            }))
            .is_none(),
            "a status row without its status stays unreplayable"
        );
    }
}

/// E4 live producer: the rumor carries corr (+ result_path) so the human
/// answer returns exactly that corr via --corr/--verdict (or body fallback).
fn publish_human_question(
    project: &str,
    kind: u64,
    content: &str,
    corr: &str,
    result_path: Option<&str>,
) {
    eprintln!(
        "flywheel: [Publish kind={kind} corr={corr}] {}",
        content.chars().take(120).collect::<String>()
    );
    if kind != 14 || corr.is_empty() {
        return;
    }
    let Some(mb) = crate::mail::try_mailbox() else {
        return;
    };
    let mut extra_tags = Vec::new();
    for pair in [("audience", "human"), ("corr", corr)] {
        match toron_core::nostr::Tag::parse([pair.0, pair.1]) {
            Ok(t) => extra_tags.push(t),
            Err(_) => return,
        }
    }
    if let Some(rp) = result_path.filter(|s| !s.is_empty()) {
        match toron_core::nostr::Tag::parse(["result_path", rp]) {
            Ok(t) => extra_tags.push(t),
            Err(_) => return,
        }
    }
    let args = toron_core::mailbox::messaging::SendMessage {
        project_slug: project.to_string(),
        sender_name: "captain".into(),
        to: vec!["captain".into()],
        subject: "[human] question".into(),
        body_md: content.to_string(),
        extra_tags,
        ..Default::default()
    };
    if let Err(e) = crate::mail::block_on(mb.send_message(&args)) {
        eprintln!("flywheel: [Publish kind=14] send failed: {e:#}");
    }
}

/// bd-adr0027-proven-leftovers-55nx.1: crash-resume presence seed for `why`.
/// Applies fresh presence.jsonl rows (parsed by timeout_presence_rows:
/// latest row per pin, <30s old) as Model.presence. Unparseable states
/// become Unknown via Presence::from_str — never defaulted idle.
fn seed_why_presence(
    workspace_model: &mut crate::workspace::WorkspaceModel,
    rows: Vec<(String, String)>,
) {
    for (pin, state) in rows {
        workspace_model
            .presence
            .insert(pin, crate::workspace::Presence::from_str(&state));
    }
}

pub(crate) fn rebuild_assignment_from_receipts(
    workspace_model: &mut crate::workspace::WorkspaceModel,
) {
    if let Some(mb) = crate::mail::try_mailbox()
        && let Ok(store) = mb.receipt_store()
        && let Ok(receipts) = crate::mail::block_on(store.list(None))
    {
        for r in receipts {
            let w = r
                .wake
                .map(|w| w.as_str().to_string())
                .unwrap_or_else(|| "tasked".to_string());
            let rec = crate::workspace::Receipt {
                state: r.state.as_str().to_string(),
                wake: w,
                due_at: r.due_at.map(|v| v as u64),
                result_path: r.result_path.clone(),
            };
            workspace_model.assignment.insert(r.corr.clone(), rec);
        }
    }
}
#[cfg(test)]
mod journal_effect_tests {
    use super::*;

    #[test]
    fn budget_effect_splits_pins() {
        let raw = vec![
            ("A".to_string(), "l1".to_string()),
            ("B".to_string(), "l2".to_string()),
        ];
        let allowed = vec![("A".to_string(), "l1".to_string())];
        let effect = budget_effect(&raw, &allowed);
        assert_eq!(
            effect
                .get("injected")
                .and_then(|v| v.as_array())
                .map(std::vec::Vec::len),
            Some(1),
            "one pin injected, got {effect}"
        );
        let dropped = effect
            .get("dropped")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            dropped.iter().any(|p| p.as_str() == Some("B")),
            "B was dropped by budget, got {effect}"
        );
        assert!(
            !dropped.iter().any(|p| p.as_str() == Some("A")),
            "A was injected, never dropped, got {effect}"
        );
    }

    #[test]
    fn journal_decided_lines_carry_effect() {
        let src = include_str!("doorbell.rs");
        for stage in ["decided", "drain", "fact_live", "sweep_inbox"] {
            let mut found_effect = false;
            let mut search = src;
            while let Some(pos) = search.find(stage) {
                let slice = &search[pos..std::cmp::min(search.len(), pos + 600)];
                if slice.contains("\"effect\"") || slice.contains("effect") {
                    found_effect = true;
                    break;
                }
                search = &search[pos + stage.len()..];
            }
            assert!(
                found_effect,
                "RED (55nx.9): journal stage {stage} carries no effect field — why overclaims dropped Prompts"
            );
        }
    }

    #[test]
    fn why_reads_effect_before_claiming_inject() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn cmd_doorbell_why(").expect("cmd_doorbell_why");
        let slice = &src[pos..std::cmp::min(src.len(), pos + 12000)];
        assert!(
            slice.contains("effect") && slice.contains("dropped"),
            "RED (55nx.9): why never reads the journal effect — dropped Prompts replay as injected"
        );
    }
}
#[cfg(test)]
mod why_resume_tests {
    use super::*;

    #[test]
    fn why_resume_seed_applies_presence_rows() {
        let mut model = workspace::WorkspaceModel::default();
        seed_why_presence(
            &mut model,
            vec![
                ("A".to_string(), "working".to_string()),
                ("B".to_string(), "weird".to_string()),
            ],
        );
        assert_eq!(
            model.presence.get("A"),
            Some(&workspace::Presence::from_str("working"))
        );
        assert_eq!(
            model.presence.get("B"),
            Some(&workspace::Presence::Unknown),
            "unparseable state seeds Unknown, never idle"
        );
    }

    #[test]
    fn why_reads_status_from_live_rt_kind_rows() {
        let live = serde_json::json!({
            "rt_kind": "PlaneStatus", "pin": "PearlTern", "status": "idle"
        });
        assert_eq!(why_plane_status(&live), Some("idle"));
        let legacy = serde_json::json!({
            "PlaneStatus": {"pin": "PearlTern", "status": "working"}
        });
        assert_eq!(why_plane_status(&legacy), Some("working"));
        assert_eq!(
            why_plane_status(&serde_json::json!({"rt_kind": "PlaneStatus"})),
            None
        );
    }

    #[test]
    fn why_limit_keeps_newest_rows_across_workspaces() {
        let row = |workspace: &str, ts: &str, seq: u64| super::WhyDisplayRow {
            workspace: workspace.to_string(),
            ts: ts.to_string(),
            ts_us: doorbell_core::parse_rfc3339_micros(ts).ok(),
            seq,
            kind: "status".into(),
            status: "idle".into(),
            actions: String::new(),
            dropped: Vec::new(),
            folded: true,
            console: false,
        };
        let rows = vec![
            row("elm-comms-kernel", "2026-11-08T16:33:21Z", 1),
            row("elm-comms-kernel", "2026-11-08T16:36:26Z", 2),
            row("flywheel-toron", "2026-11-08T18:19:39Z", 3),
            row("session:01", "2026-11-08T18:20:49Z", 4),
        ];
        let (shown, withheld) = super::why_newest(rows, 2);
        assert_eq!(withheld, 2, "the two older rows are withheld");
        assert_eq!(
            shown
                .iter()
                .map(|r| r.workspace.as_str())
                .collect::<Vec<_>>(),
            vec!["flywheel-toron", "session:01"],
            "limit must keep the newest rows, not the first workspace in BTreeSet order"
        );
    }

    #[test]
    fn why_seeds_assignment_before_fold() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn cmd_doorbell_why(").expect("cmd_doorbell_why");
        let slice = &src[pos..std::cmp::min(src.len(), pos + 9000)];
        assert!(
            slice.contains("rebuild_assignment_from_receipts"),
            "RED (55nx.1): why folds from Default without the receipts seed — tasked corrs vanish from replay"
        );
    }

    #[test]
    fn why_seeds_presence_snapshot() {
        let src = include_str!("doorbell.rs");
        let pos = src.find("fn cmd_doorbell_why(").expect("cmd_doorbell_why");
        let end = src[pos..]
            .find("\nfn cmd_loop_why(")
            .map(|i| pos + i)
            .unwrap_or(src.len());
        let slice = &src[pos..end];
        assert!(
            slice.contains("seed_why_presence"),
            "RED (55nx.1): why folds without a presence seed — working pins replay Prompts live would suppress"
        );
        assert!(
            slice.contains("presence_path()"),
            "the why presence seed must read the toron-canonical root-level projection"
        );
    }
}

/// --- moved-in test mod (bd-wave5-main-eviction-d5hht) ---
#[cfg(test)]
mod host_key_tests {
    use super::doorbell_host_key;

    /// The live session workspace must reach `__doorbell` by its plane ID;
    /// every other label (scratch workspaces) stays a label.
    #[test]
    fn maps_only_the_live_label_to_its_id() {
        assert_eq!(
            doorbell_host_key("flywheel-toron", "flywheel-toron", "session:abc"),
            "session:abc"
        );
        assert_eq!(
            doorbell_host_key("scratch", "flywheel-toron", "session:abc"),
            "scratch"
        );
        assert_eq!(
            doorbell_host_key("flywheel-toron", "", "session:abc"),
            "session:abc"
        );
        assert_eq!(
            doorbell_host_key("flywheel-toron", "flywheel-toron", ""),
            "flywheel-toron"
        );
    }

    /// re-inject must use the active plane backend (rex by default). The
    /// old hc client failed with `cannot connect to hc socket` on every
    /// rex-only host, so added panes never got the peer handler.
    #[test]
    fn re_inject_uses_the_plane_backend() {
        let src = include_str!("../cmd/doorbell.rs");
        let start = src
            .find("pub(crate) fn cmd_doorbell_re_inject(")
            .expect("re-inject");
        let rest = &src[start..];
        let end = rest
            .find("\npub(crate) fn cmd_internal_doorbell(")
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("multiplexer_backend"),
            "re-inject must resolve the active backend"
        );
        assert!(
            body.contains("backend.prompt"),
            "re-inject must prompt through the backend trait"
        );
        assert!(
            !body.contains("hc::Client::connect"),
            "re-inject must not hard-code hc"
        );
        assert!(
            body.contains("load_captain_alias"),
            "the operator/captain pane must be skipped, not abort the pass"
        );
    }
}

#[cfg(test)]
mod single_writer_tests {
    #[test]
    fn option_a_doorbell_seeds_swarm_resume() {
        let host = include_str!("../doorbell_host.rs");
        assert!(
            host.contains("seed_workspace_from_active_loop"),
            "Option A: doorbell must seed swarm from ActiveLoop onto the one Model"
        );
        assert!(
            host.contains("seed_roles_from_binds"),
            "Option A / B8: doorbell must seed roles from the RoleBound snapshot"
        );
        assert!(
            host.contains("WorkspaceModel::from_environment()"),
            "doorbell remains the one live WorkspaceModel with host-resolved policy"
        );
    }

    #[test]
    fn option_a_speech_rejects_dual_as_end_state() {
        let ws = include_str!("../workspace.rs");
        assert!(
            !ws.contains("This is the end-state, not a halfway merge"),
            "Option A: Dual-as-end-state speech must not remain"
        );
        assert!(
            !ws.contains("One RAM is impossible"),
            "Option A: 'One RAM is impossible' contradicts ADR-0027"
        );
        assert!(
            ws.contains("The end-state is one RAM owner"),
            "Option A: speech must name one RAM owner as the end-state"
        );
    }
}

#[cfg(test)]
mod doorbell_canonical_workspace_tests {
    use super::*;
    use crate::doorbell::{DoorbellState, state_file, write_state};

    const LABEL: &str = "flywheel-flywheel-rk-adr0019-3pi-1";
    const SESSION: &str = "session:01a1d257-9f7d-4c0a-9d1b-8c1e4f2a6b30";

    /// Dispatcher registry + fake process table: one `<ws>.state.json` per
    /// workspace and the pids that answer `alive`. The ids are the live rex
    /// session id and its project label, exactly as `ps` showed them.
    struct FakeDispatchRegistry {
        dir: std::path::PathBuf,
        alive: std::collections::BTreeSet<u32>,
    }

    impl FakeDispatchRegistry {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fw-dispatch-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("doorbell")).unwrap();
            Self {
                dir,
                alive: std::collections::BTreeSet::new(),
            }
        }

        fn spawn_dispatcher(&mut self, ws: &str, pid: u32) {
            write_state(
                &state_file(&self.dir, ws),
                &DoorbellState {
                    pid: Some(pid),
                    project: Some("flywheel".to_string()),
                    generation: None,
                },
            )
            .expect("write dispatcher record");
            self.alive.insert(pid);
        }

        /// Live dispatchers for `keys`, in registry order.
        fn live(&self, keys: &[String]) -> Vec<(String, u32)> {
            keys.iter()
                .filter_map(|ws| {
                    let pid = crate::doorbell::dispatcher_pid(&self.dir, ws)?;
                    self.alive.contains(&pid).then(|| (ws.clone(), pid))
                })
                .collect()
        }

        fn stop(&mut self, ws: &str) {
            if let Some(pid) = crate::doorbell::dispatcher_pid(&self.dir, ws) {
                self.alive.remove(&pid);
            }
            let _ = std::fs::remove_file(state_file(&self.dir, ws));
        }
    }

    impl Drop for FakeDispatchRegistry {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// bd-g9hqu: a start by session id followed by a start by the old label
    /// must leave ONE dispatcher. The raw (uncanonicalised) pair is the
    /// mutation witness: it reproduces the second door.
    #[test]
    fn start_by_session_id_then_by_label_leaves_one_dispatcher() {
        let mut reg = FakeDispatchRegistry::new("start");
        let requested = vec![SESSION.to_string(), LABEL.to_string()];
        let keys = canonical_workspaces(&requested, LABEL, SESSION);
        assert_eq!(
            keys,
            vec![SESSION.to_string()],
            "one workspace, one key: the label folds into the session id"
        );
        for (idx, ws) in keys.iter().enumerate() {
            reg.spawn_dispatcher(ws, 700 + idx as u32);
        }
        assert_eq!(
            reg.live(&keys).len(),
            1,
            "second start is a no-op on the live key, never a second door"
        );
        assert_eq!(
            reg.live(&requested).len(),
            1,
            "the label has no door of its own"
        );
        assert_eq!(
            requested.len(),
            2,
            "mutation keying by raw label reproduces two dispatchers: {requested:?}"
        );
    }

    /// Stop by label must stop the one dispatcher, including a stale record
    /// left under the label by the pre-fix binary.
    #[test]
    fn stop_by_label_stops_the_one_dispatcher() {
        let mut reg = FakeDispatchRegistry::new("stop");
        reg.spawn_dispatcher(SESSION, 701);
        reg.spawn_dispatcher(LABEL, 76310);

        let label = vec![LABEL.to_string()];
        let key = canonical_workspaces(&label, LABEL, SESSION);
        for ws in &key {
            reg.stop(ws);
        }
        for ws in &label {
            if !key.contains(ws) {
                reg.stop(ws);
            }
        }
        assert!(
            reg.live(&[SESSION.to_string(), LABEL.to_string()])
                .is_empty(),
            "stop by label kills the canonical dispatcher AND the stale label record"
        );
    }

    /// Heal never spawns a second door for the same canonical workspace: the
    /// label's dead record is not a heal target when the session id is live.
    #[test]
    fn heal_never_spawns_a_second_for_the_same_canonical_key() {
        let mut reg = FakeDispatchRegistry::new("heal");
        reg.spawn_dispatcher(SESSION, 702);
        let listed = vec![LABEL.to_string(), SESSION.to_string()];

        let heal: Vec<String> = canonical_workspaces(&listed, LABEL, SESSION)
            .into_iter()
            .filter(|ws| reg.live(std::slice::from_ref(ws)).is_empty())
            .collect();
        assert!(heal.is_empty(), "one door stands: {heal:?}");

        let raw_heal: Vec<String> = listed
            .iter()
            .filter(|ws| reg.live(std::slice::from_ref(ws)).is_empty())
            .cloned()
            .collect();
        assert_eq!(
            raw_heal,
            vec![LABEL.to_string()],
            "mutation keying by raw label heals the dead label back beside the live one"
        );
    }

    /// Report names the canonical key and the label it folded, once.
    #[test]
    fn report_names_the_canonical_key_and_its_label() {
        let listed = vec![LABEL.to_string(), SESSION.to_string()];
        let keys = canonical_workspaces(&listed, LABEL, SESSION);
        assert_eq!(keys.len(), 1, "one row per workspace: {keys:?}");
        let alias = listed.iter().find(|raw| {
            raw.as_str() != keys[0] && doorbell_host_key(raw, LABEL, SESSION) == keys[0]
        });
        assert_eq!(alias.map(String::as_str), Some(LABEL));
    }
}

#[cfg(test)]
mod doorbell_canonical_project_tests {
    use super::*;

    const PATH: &str = "/Users/op/workspace/chiebukuro";
    const SLUG: &str = "chiebukuro";

    /// One project, one key: a path-valued project is the slug it names.
    #[test]
    fn a_path_and_its_slug_are_the_same_project_key() {
        assert_eq!(canonical_project_key(PATH, "test").unwrap(), SLUG);
        assert_eq!(canonical_project_key(SLUG, "test").unwrap(), SLUG);
        assert_eq!(
            canonical_project_key("/Users/op/workspace/chiebukuro/", "test").unwrap(),
            SLUG,
            "a trailing separator is still the same project"
        );
        assert_eq!(
            canonical_project_key("workspace/chiebukuro", "test").unwrap(),
            SLUG
        );
        assert!(
            canonical_project_key(".", "test").is_err(),
            "a cwd alias is still refused"
        );
    }

    /// The construction site canonicalises: the spawned host's argv can
    /// never carry a path-valued project.
    #[test]
    fn spawn_child_carries_the_slug() {
        let src = include_str!("../doorbell.rs");
        let at = src.find("fn spawn_child(").expect("spawn_child");
        let body = &src[at..];
        let end = body.find("\n}\n").expect("body end");
        assert!(
            body[..end].contains("let project = canonical_project(project);"),
            "spawn_child must canonicalise the project before building argv"
        );
    }

    /// Every door folds through the same key: a path cannot open a second
    /// dispatcher path for one project (chibuikuro live bug).
    #[test]
    fn every_door_folds_through_the_project_key() {
        let src = include_str!("doorbell.rs");
        for door in ["status", "start", "supervise", "stop"] {
            let needle = format!("canonical_project_key(project, \"doorbell {door}\")");
            assert!(
                src.contains(&needle),
                "doorbell {door} must canonicalise the project: missing {needle}"
            );
        }
    }
}

#[cfg(test)]
mod project_dispatchers_tests {
    use crate::doorbell::{DoorbellState, state_file, write_state};

    /// bd-bj9xk: status must see every dispatcher record for the project —
    /// including dead sessions and the legacy label — because that is the only
    /// way the reap and the operator can see duplicates.
    #[test]
    fn every_dispatcher_record_is_listed_with_its_pid() {
        let dir = std::env::temp_dir().join(format!("fw-projdisp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("doorbell")).unwrap();
        for (ws, project, pid) in [
            ("session:01a1d1e6-live", "chiebukuro", 49772u32),
            ("session:01a1d816-dead", "chiebukuro", 17093),
            ("flywheel-chiebukuro", "chiebukuro", 17301),
            ("session:other-project", "other", 999),
        ] {
            write_state(
                &state_file(&dir, ws),
                &DoorbellState {
                    pid: Some(pid),
                    project: Some(project.to_string()),
                    generation: None,
                },
            )
            .unwrap();
        }
        let rows = crate::doorbell::project_dispatchers(&dir, "chiebukuro");
        assert_eq!(
            rows,
            vec![
                ("flywheel-chiebukuro".to_string(), Some(17301)),
                ("session:01a1d1e6-live".to_string(), Some(49772)),
                ("session:01a1d816-dead".to_string(), Some(17093)),
            ],
            "every chiebukuro door, with its pid; other projects excluded"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod dispatcher_gone_tests {
    use super::*;

    /// Only a live session id survives; an absent one is dead, and the legacy
    /// label form is an alias of the session door (bd-bj9xk reap target).
    #[test]
    fn a_dispatcher_is_gone_when_its_session_is_absent_or_it_is_a_label() {
        let live = vec!["01a1d1e6-0b95-767c-861f-176fd88da6ea".to_string()];
        assert_eq!(
            dispatcher_gone("session:01a1d1e6-0b95-767c-861f-176fd88da6ea", &live),
            None,
            "the one live session is kept"
        );
        for ws in [
            "session:01a1d816-a391-7bb0-8b05-cb9f25cef836",
            "session:01a1d877-0d27-76a3-a2a1-dc08848f7105",
            "session:01a1d8f0-6600-7943-8c9c-b104b1457df9",
        ] {
            assert_eq!(
                dispatcher_gone(ws, &live),
                Some(GoneReason::SessionAbsent),
                "{ws} has no live rex session"
            );
        }
        assert_eq!(
            dispatcher_gone("flywheel-chiebukuro", &live),
            Some(GoneReason::LabelAlias),
            "the bare label is the legacy alias form"
        );
    }
}
