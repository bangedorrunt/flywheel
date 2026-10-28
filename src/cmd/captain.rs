//! `flywheel captain` — the captain's own poll-exit door
//! (bd-captain-watch-poll-exit-bbwm).
//!
//! The captain has no push channel: routine mail raises no doorbell wake and a
//! busy pane cannot be injected. So the operator hand-rolled poll-exit shell
//! watchers that block until one line appears and then exit, letting the
//! harness completion callback resume the captain. This ships that as ONE
//! command: `captain watch <project>` blocks until captain mail lands (or, with
//! `--stall`, until the fleet stalls or every bead is done), prints a preview,
//! and exits. Re-arm after each resume — the watch is one shot by design;
//! looping forever would recreate the wake problem it exists to solve.
//!
//! The decision logic is pure (`newest_since`, `stall_verdict`) so the
//! poll-exit contract is testable without a live inbox, herdr, or repo.

use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};

use crate::backend::MultiplexerBackend as _;
use crate::config::Config;
use crate::mail::MailInboxFilter;
use crate::output::print_json;
use crate::preflight;

#[derive(Subcommand)]
pub(crate) enum CaptainCmd {
    /// block on the captain's own mail water (poll-exit): exit with a preview
    /// when new mail lands. `--stall` also exits on a stalled fleet or when
    /// every bead is done. One shot — re-arm after each resume.
    Watch {
        project: String,
        /// seconds between polls
        #[arg(long, default_value_t = 15)]
        interval: u64,
        /// emit one JSON object on exit (stdout) instead of a human preview
        #[arg(long)]
        json: bool,
        /// also exit when the fleet stalls: all beads done, OR no pane working
        /// and no commit landed for --stall-minutes with work remaining
        #[arg(long)]
        stall: bool,
        /// stall threshold in minutes (with --stall)
        #[arg(long, default_value_t = 45)]
        stall_minutes: u64,
        /// act as this registered agent (adjective+noun or herdr short name)
        #[arg(long = "as")]
        as_agent: Option<String>,
        /// also exit when the project's preflight fails (store skew, missing
        /// binary, unreachable relay, unallocated panes): one shot, names the
        /// failing check. Exit 4. Off by default.
        #[arg(long)]
        preflight: bool,
        /// preflight mode for the arm (steer|loop|workflow|cron)
        #[arg(long, default_value = "loop")]
        preflight_mode: String,
    },
    /// approve (or deny) a parked hosted steer (ADR-0019 D10): verify the
    /// operator, mail the asker the 4-line body, rehash the payload, and run
    /// it. A missing row, self-approval, or an unverified sender is
    /// approval_mismatch — never a dispatch.
    Approve {
        project: String,
        /// request key of the parked steer
        #[arg(long)]
        request_key: String,
        /// approve or deny
        #[arg(long)]
        decision: String,
        /// one-line note, cut at 500 chars with the cut marked
        #[arg(long, default_value = "")]
        note: String,
        /// act as this operator pin (must differ from the asker)
        #[arg(long = "as")]
        as_agent: Option<String>,
    },
    /// show the captain directory (ADR-0019 D2/D4): sessions, tiers, and
    /// addressability for this project's pins. Read-only: no dispatch,
    /// no journal.
    Directory {
        project: String,
        /// emit one JSON object instead of the human table
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn cmd_captain(cfg: &Config, cmd: &CaptainCmd) -> Result<()> {
    match cmd {
        CaptainCmd::Watch {
            project,
            interval,
            json,
            stall,
            stall_minutes,
            as_agent,
            preflight,
            preflight_mode,
        } => {
            // An unknown mode must refuse loudly, not silently arm nothing.
            let pf = if *preflight {
                Some(preflight::Mode::named(preflight_mode, true).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown preflight mode: {preflight_mode} (steer|loop|workflow|cron)"
                    )
                })?)
            } else {
                None
            };
            cmd_captain_watch(
                cfg,
                project,
                *interval,
                *json,
                *stall,
                *stall_minutes,
                as_agent.as_deref(),
                pf,
            )
        }
        CaptainCmd::Approve {
            project,
            request_key,
            decision,
            note,
            as_agent,
        } => cmd_captain_approve(
            cfg,
            project,
            request_key,
            decision,
            note,
            as_agent.as_deref(),
        ),
        CaptainCmd::Directory { project, json } => cmd_captain_directory(cfg, project, *json),
    }
}

/// Exit code for a stall alarm. Distinct from 1 (usage/runtime error) so the
/// re-arming wrapper can tell a real stall from a failed poll.
const STALL_EXIT: i32 = 3;

/// Exit code for a preflight-arm failure. Its own code (not 3, not 1) so the
/// re-arming wrapper tells a broken environment from a stalled fleet.
const PREFLIGHT_EXIT: i32 = 4;

/// The FIRST failing check as `(name, message)`. Skips are advisory (the
/// operation can proceed) and passes are silent; only a `Fail` arms the exit.
/// Pure, so the arm's rule is pinned without a live herdr/resonate/toron.
pub(crate) fn first_failing_check(checks: &[preflight::Check]) -> Option<(&'static str, String)> {
    checks.iter().find_map(|c| match &c.status {
        preflight::Status::Fail(msg) => Some((c.name, msg.clone())),
        _ => None,
    })
}

/// Print the preflight-arm failure once (JSON or human) and return the exit
/// code. `None` when every check passed or skipped — the watch keeps polling.
fn preflight_arm_exit(
    cfg: &Config,
    project: &str,
    mode: preflight::Mode,
    json_out: bool,
) -> Option<i32> {
    let checks = preflight::run(cfg, mode, Some(project));
    let (name, msg) = first_failing_check(&checks)?;
    if json_out {
        print_json(&json!({
            "reason": "preflight",
            "project": project,
            "check": name,
            "message": msg,
        }));
    } else {
        println!("PREFLIGHT FAILURE in {project}: {name} — {msg}");
    }
    Some(PREFLIGHT_EXIT)
}

/// The newest row strictly newer than `baseline_us`, or None. Strict-newer
/// matches the toron `since` contract (equal timestamps are not new), so a row
/// already in the inbox at arm time is never re-reported.
fn newest_since(rows: &[Value], baseline_us: i64) -> Option<&Value> {
    rows.iter()
        .filter(|row| created_us(row).is_some_and(|us| us > baseline_us))
        .max_by_key(|row| created_us(row).unwrap_or(i64::MIN))
}

/// One row's creation time in unix micros. Missing/unparseable → None (the row
/// is not evidence of new mail, never a false arrival).
fn created_us(row: &Value) -> Option<i64> {
    row.get("created_ts_us").and_then(Value::as_i64)
}

/// What one stall probe observed. Shared with `loop-mode supervise` (rweq),
/// so the captain watch and the supervisor judge the same fleet the same way.
pub(crate) struct StallProbe {
    /// any bead not closed/done
    pub work_remaining: bool,
    /// a pin reads Busy (working/busy/blocked — an occupied pane)
    pub any_pane_working: bool,
    /// herdr pins were readable. False → the fleet is UNKNOWABLE, so we never
    /// alarm blind (a degraded read must not be read as calm AND must not be
    /// read as a stall).
    pub fleet_knowable: bool,
    /// seconds since the newest commit in the project, None when git is absent
    pub last_commit_age_secs: Option<u64>,
    pub stall_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallVerdict {
    Working,
    AllComplete,
    Stalled,
}

/// The stall decision. `AllComplete` wins first (nothing left to do), then any
/// working pane, then an unknowable fleet (never alarm blind), and only then
/// the idle-plus-silent clock.
fn stall_verdict(p: &StallProbe) -> StallVerdict {
    if !p.work_remaining {
        return StallVerdict::AllComplete;
    }
    if p.any_pane_working || !p.fleet_knowable {
        return StallVerdict::Working;
    }
    match p.last_commit_age_secs {
        Some(age) if age >= p.stall_secs => StallVerdict::Stalled,
        _ => StallVerdict::Working,
    }
}

#[allow(clippy::too_many_arguments)] // one param per CLI flag (clap handler).
fn cmd_captain_watch(
    cfg: &Config,
    project: &str,
    interval: u64,
    json_out: bool,
    stall: bool,
    stall_minutes: u64,
    as_agent: Option<&str>,
    preflight: Option<preflight::Mode>,
) -> Result<()> {
    // The preflight arm runs FIRST: a project whose environment is already
    // broken (skewed store, missing binary, unreachable relay) must not arm a
    // watch that cannot do anything with the wake it would receive. Re-checked
    // each poll so a fault that develops after arming still exits once.
    if let Some(mode) = preflight
        && let Some(code) = preflight_arm_exit(cfg, project, mode, json_out)
    {
        std::process::exit(code);
    }
    let interval = interval.max(1);
    let key = crate::project_dir_str(cfg, project)?;
    let mail = crate::mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;

    // Our own water: capture the inbox high-water ONCE at arm time. Mail that
    // was already here is not new (same contract as `mail watch`); only what
    // lands after arming resumes the captain.
    let armed = inbox_rows(&mail, &key, None)?;
    let baseline_us = armed.iter().filter_map(created_us).max().unwrap_or(0);
    let baseline_rfc = doorbell_core::inbox_high_water_from_values(armed.iter().collect());

    eprintln!(
        "flywheel: captain watch {project} armed (mail water {}, stall {}); Ctrl-C to abort",
        baseline_rfc.as_deref().unwrap_or("<empty>"),
        if stall {
            format!("on, {} min", stall_minutes.max(1))
        } else {
            "off".to_string()
        }
    );

    let stall_secs = stall_minutes.max(1) * 60;
    let project_dir = crate::project_dir(cfg, project);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));

        // 1) Mail — the primary poll-exit.
        let rows = inbox_rows(&mail, &key, baseline_rfc.as_deref())?;
        if let Some(row) = newest_since(&rows, baseline_us) {
            if json_out {
                print_json(&json!({
                    "reason": "mail",
                    "project": project,
                    "water": baseline_rfc,
                    "count": rows.len(),
                    "newest": row,
                }));
            } else {
                let sender = row.get("sender").and_then(Value::as_str).unwrap_or("?");
                let subject = row
                    .get("subject")
                    .and_then(Value::as_str)
                    .unwrap_or("(no subject)");
                let thread = row
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or("(none)");
                println!("new captain mail from {sender}: {subject}");
                println!("thread {thread} — read it: flywheel mail inbox {project}");
            }
            return Ok(());
        }

        // 2) Preflight — opt-in, re-checked each poll.
        if let Some(mode) = preflight
            && let Some(code) = preflight_arm_exit(cfg, project, mode, json_out)
        {
            std::process::exit(code);
        }

        // 3) Stall — only when asked; the mail exit above is the common case.
        if !stall {
            continue;
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let probe = probe_stall(cfg, project, &project_dir, now, stall_secs);
        match stall_verdict(&probe) {
            StallVerdict::Working => continue,
            StallVerdict::AllComplete => {
                if json_out {
                    print_json(&json!({
                        "reason": "complete",
                        "project": project,
                        "work_remaining": false,
                    }));
                } else {
                    println!("all beads complete — nothing left to watch in {project}");
                }
                return Ok(());
            }
            StallVerdict::Stalled => {
                let age = probe.last_commit_age_secs.unwrap_or(0);
                if json_out {
                    print_json(&json!({
                        "reason": "stall",
                        "project": project,
                        "stall_secs": stall_secs,
                        "no_commit_secs": age,
                        "work_remaining": true,
                        "panes_working": 0,
                    }));
                } else {
                    println!(
                        "STALL in {project}: no pane working and no commit for {age}s \
                         (threshold {stall_secs}s) with work remaining — page the fleet"
                    );
                }
                std::process::exit(STALL_EXIT);
            }
        }
    }
}

/// The live stall probe: fleet + lanes from the snapshot, commit age from git.
pub(crate) fn probe_stall(
    cfg: &Config,
    project: &str,
    project_dir: &std::path::Path,
    now: u64,
    stall_secs: u64,
) -> StallProbe {
    let ws_label =
        crate::state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
    let snap = crate::snapshot::assemble(cfg, project, &ws_label, now);
    let fleet_knowable = !snap.degraded.iter().any(|d| d == "pins");
    let any_pane_working = snap.pins.iter().any(|p| p.verdict.starts_with("Busy"));
    let work_remaining = snap
        .lanes
        .iter()
        .any(|l| l.status != "closed" && l.status != "done");
    StallProbe {
        work_remaining,
        any_pane_working,
        fleet_knowable,
        last_commit_age_secs: last_commit_age_secs(project_dir, now),
        stall_secs,
    }
}

/// Seconds since the newest commit in the project. None when git is absent or
/// the repo has no commit — never a fabricated "very old" age, which would
/// alarm on a repo that simply has no history.
pub(crate) fn last_commit_age_secs(project_dir: &std::path::Path, now: u64) -> Option<u64> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(project_dir)
        .args(["log", "-1", "--format=%ct"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(now.saturating_sub(secs))
}

/// One inbox read for the watch. `since` scopes the read server-side so the
/// store's limit (which keeps the OLDEST rows of an ascending page) cannot hide
/// a brand-new message — the same trap `mail watch` guards against.
fn inbox_rows(mail: &crate::mail::Mail, key: &str, since: Option<&str>) -> Result<Vec<Value>> {
    let filter = MailInboxFilter {
        unread_only: false,
        include_bodies: true,
        limit: 50,
        since_ts: since.map(str::to_owned),
        ..Default::default()
    };
    let v = mail.inbox_filtered(key, &mail.name, &filter)?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// ADR-0019 D2/D4: one pin's directory inputs, loaded by the CLI.
struct DirPin {
    pin: String,
    harness: String,
    pane_id: Option<String>,
    location_hosted: bool,
    live: bool,
    registered: bool,
}

/// ADR-0019 D2/D4: pure directory assembly over caller-loaded rows. No I/O,
/// no clock: the CLI loads, this projects with `tier_of`,
/// `project_directory`, `session_key`, and `resolve_address`.
fn assemble_directory(
    pins: &[DirPin],
    beads: &[(String, Option<String>)],
    file_rows: &[(String, String, String)],
) -> (
    Vec<crate::captain_directory::Session>,
    bool,
    Vec<(String, String)>,
) {
    use crate::captain_directory as dir;
    let bead_ids: std::collections::BTreeSet<&str> =
        beads.iter().map(|(id, _)| id.as_str()).collect();
    let mut sessions: Vec<dir::Session> = pins
        .iter()
        .map(|p| {
            let block_id = p.pane_id.as_deref().filter(|id| id.starts_with("block:"));
            let caps = crate::harness_caps::row(p.harness.as_str());
            let tier = dir::tier_of(&dir::TierSnapshot {
                live_block: p.live,
                prompt_supported: p.live,
                hook_some: caps.is_some_and(|c| c.hook.is_some()),
                verified_some: caps.is_some_and(|c| c.verified_by.is_some()),
                pin_registered: p.registered,
                location_hosted: p.location_hosted,
                has_block: p.live,
                inbox_enabled: true,
                rex_degraded: false,
                positive_no_receiver: false,
            });
            let mut s = dir::Session::new(
                p.pin.clone(),
                dir::session_key(
                    p.location_hosted,
                    &p.pin,
                    (!p.harness.is_empty()).then_some(p.harness.as_str()),
                    None,
                    block_id,
                    p.pane_id.as_deref(),
                    false,
                ),
                tier,
            );
            s.location_hosted = p.location_hosted;
            s.pane_id = p.pane_id.clone();
            s
        })
        .collect();
    sessions.sort_by(|a, b| {
        (a.pin.clone(), a.session_key.clone()).cmp(&(b.pin.clone(), b.session_key.clone()))
    });
    let (directory, truncated) = dir::project_directory(sessions.clone());
    let _ = directory;
    // Addressability per pin. Peer holds are viewer-relative, so the
    // directory reports file holds globally; the sender is the pin itself
    // when arrived, else the first arrived pin (else the lookup itself is
    // unarrived, which is the honest answer for a stranger).
    let bead_assignee: std::collections::BTreeMap<String, String> = beads
        .iter()
        .filter_map(|(id, a)| a.clone().map(|a| (id.clone(), a)))
        .collect();
    let file_held: std::collections::BTreeSet<String> = file_rows
        .iter()
        .filter(|(_, reason, holder)| !holder.is_empty() && bead_ids.contains(reason.as_str()))
        .map(|(_, reason, _)| reason.clone())
        .collect();
    let arrived: std::collections::BTreeSet<String> = pins
        .iter()
        .filter(|p| p.registered || p.live)
        .map(|p| p.pin.clone())
        .collect();
    let fallback_sender = arrived.iter().next().cloned().unwrap_or_default();
    let snap = dir::DirSnapshot {
        sessions: sessions.clone(),
        bead_assignee,
        peer_held_beads: std::collections::BTreeSet::new(),
        file_held_beads: file_held,
        arrived_pins: arrived,
    };
    let address = pins
        .iter()
        .map(|p| {
            let sender = if snap.arrived_pins.contains(&p.pin) {
                p.pin.clone()
            } else {
                fallback_sender.clone()
            };
            let word = match dir::resolve_address(&snap, &p.pin, &sender) {
                Ok(s) => format!("ok {}", s.session_key),
                Err(e) => format!("err {}", e.code()),
            };
            (p.pin.clone(), word)
        })
        .collect();
    (sessions, truncated, address)
}

/// ADR-0019 D2/D4 read-only directory.
///
/// `flywheel captain directory <project> [--json]`
fn cmd_captain_directory(cfg: &Config, project: &str, json_out: bool) -> Result<()> {
    // Explicit rex backend on this path: `auto` would return herdr when
    // REX_SESSION is unset, and herdr has no blocks to project.
    let backend = crate::backend::superlogical::SuperlogicalBackend;
    let ws_id = crate::find_workspace_dyn(&backend, project)?;
    let agents = backend
        .agent_list()
        .map_err(|e| anyhow::anyhow!("captain directory: rex backend unavailable: {e:#}"))?;
    let registry = crate::state::load_spawn_registry(project);
    let mut pins: Vec<DirPin> = Vec::new();
    for e in &registry {
        let pin = e.mail_identity.clone().unwrap_or_else(|| e.name.clone());
        if pins.iter().any(|p: &DirPin| p.pin == pin) {
            continue;
        }
        let pane_id = if e.pane_id.is_empty() {
            None
        } else {
            Some(e.pane_id.clone())
        };
        let live = pane_id.as_deref().is_some_and(|pane| {
            agents
                .iter()
                .any(|a| a.workspace_id == ws_id && a.pane_id == *pane)
        });
        pins.push(DirPin {
            location_hosted: crate::agent_ref::binding_location_for(project, &pin)
                == crate::agent_ref::BindingLocation::Hosted,
            pin,
            harness: e.harness.clone().unwrap_or_default(),
            pane_id,
            live,
            registered: true,
        });
    }
    for a in agents.iter().filter(|a| a.workspace_id == ws_id) {
        let Some(name) = a.name.clone() else { continue };
        if pins.iter().any(|p: &DirPin| p.pin == name) {
            continue;
        }
        pins.push(DirPin {
            location_hosted: crate::agent_ref::binding_location_for(project, &name)
                == crate::agent_ref::BindingLocation::Hosted,
            pin: name,
            harness: String::new(),
            pane_id: Some(a.pane_id.clone()),
            live: true,
            registered: false,
        });
    }
    let mut beads: Vec<(String, Option<String>)> = Vec::new();
    for bs in [
        crate::load_inprogress_beads(cfg, project),
        crate::load_ready_beads(cfg, project),
    ]
    .into_iter()
    .flatten()
    {
        for b in bs {
            if !beads.iter().any(|(id, _)| id == &b.id) {
                beads.push((b.id, b.assignee));
            }
        }
    }
    let file_rows = crate::live_file_rows(cfg, project);
    let (sessions, truncated, address) = assemble_directory(&pins, &beads, &file_rows);
    if json_out {
        print_json(&json!({
            "project": project,
            "sessions": sessions,
            "truncated": truncated,
            "address": address.iter().cloned().collect::<std::collections::BTreeMap<String, String>>(),
        }));
    } else {
        for (pin, word) in &address {
            let tier = sessions
                .iter()
                .find(|s| &s.pin == pin)
                .map(|s| format!("{:?}", s.tier))
                .unwrap_or_else(|| "?".to_string());
            println!("{pin} {tier} {word}");
        }
        if truncated {
            eprintln!("flywheel: directory truncated at 100 sessions");
        }
    }
    Ok(())
}
/// The clock is an argument; whois, mail, journal, and dispatch follow in
/// the caller. `payload_ok` is the re-derived-digest check (deny skips it:
/// a deny ignores any objective).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApproveGate {
    Missing,
    InFlight,
    Replay,
    DeniedReplay,
    NoPayload,
    NoAsker,
    Expired,
    Conflict,
    Ready,
}

fn approve_row_gate(
    row: Option<&crate::request_receipts::DoorReceipt>,
    payload_ok: bool,
    now_ms: u64,
) -> ApproveGate {
    use crate::agent_ref::EffectState;
    let Some(row) = row else {
        return ApproveGate::Missing;
    };
    match row.state {
        EffectState::Settled if row.evidence.as_deref() == Some("denied") => {
            ApproveGate::DeniedReplay
        }
        EffectState::Settled => ApproveGate::Replay,
        EffectState::Parked => {
            if row.payload.is_none() {
                return ApproveGate::NoPayload;
            }
            if row.asker.is_none() {
                return ApproveGate::NoAsker;
            }
            let Some(ts) = row.parked_at_ms else {
                return ApproveGate::NoPayload;
            };
            if crate::request_receipts::approval_expired(ts, now_ms) {
                return ApproveGate::Expired;
            }
            if payload_ok {
                ApproveGate::Ready
            } else {
                ApproveGate::Conflict
            }
        }
        _ => ApproveGate::InFlight,
    }
}

/// ADR-0019 D10 operator approval.
///
/// `flywheel captain approve <project> --request-key <key> --decision
/// approve|deny --note <text> --as <operator-pin>`
fn cmd_captain_approve(
    cfg: &Config,
    project: &str,
    request_key: &str,
    decision: &str,
    note: &str,
    as_agent: Option<&str>,
) -> Result<()> {
    let approver = as_agent.map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| {
        anyhow::anyhow!(
            "captain approve: --as <operator-pin> is required — approval_required, not a silent allow"
        )
    })?;
    if decision != "approve" && decision != "deny" {
        anyhow::bail!("captain approve: --decision must be approve|deny, got {decision:?}");
    }
    let slot = crate::request_receipts::door_slot(project, "steer", request_key);
    let row = crate::request_receipts::load_door_receipt(project, "steer", &slot);
    // Deny ignores any objective; approve requires the payload to re-derive.
    let payload_ok = decision == "deny"
        || row
            .as_ref()
            .and_then(|r| r.payload.as_ref())
            .is_some_and(|p| {
                crate::request_receipts::payload_rederives(
                    p,
                    row.as_ref().and_then(|r| r.digest.as_deref()).unwrap_or(""),
                )
            });
    let now_ms = crate::request_receipts::now_ms();
    match approve_row_gate(row.as_ref(), payload_ok, now_ms) {
        ApproveGate::Missing => {
            print_json(&json!({
                "approval": "mismatch",
                "request_key": request_key,
                "slot": slot,
                "reason": "no parked steer for this key",
            }));
            anyhow::bail!(
                "captain approve: no parked steer for key {request_key:?} — approval_mismatch"
            );
        }
        ApproveGate::InFlight => {
            print_json(&json!({
                "approval": "in_flight",
                "request_key": request_key,
                "slot": slot,
            }));
            anyhow::bail!("captain approve: slot {slot} is not parked — in_flight");
        }
        ApproveGate::Replay => {
            print_json(&json!({
                "approval": "replay",
                "request_key": request_key,
                "slot": slot,
                "replay": true,
            }));
            eprintln!("flywheel: approve: slot {slot} already settled — replay, not dispatching");
            return Ok(());
        }
        ApproveGate::DeniedReplay => {
            print_json(&json!({
                "approval": "approval_denied",
                "request_key": request_key,
                "slot": slot,
                "replay": true,
            }));
            eprintln!(
                "flywheel: approve: slot {slot} was denied — approval_denied, not dispatching"
            );
            return Ok(());
        }
        ApproveGate::NoPayload | ApproveGate::NoAsker => {
            print_json(&json!({
                "approval": "mismatch",
                "request_key": request_key,
                "slot": slot,
                "reason": "parked row has no payload or no asker",
            }));
            anyhow::bail!(
                "captain approve: parked row for slot {slot} is incomplete — approval_mismatch, never a guess"
            );
        }
        ApproveGate::Conflict => {
            print_json(&json!({
                "approval": "mismatch",
                "request_key": request_key,
                "slot": slot,
                "reason": "payload does not re-derive to the row digest",
            }));
            anyhow::bail!("captain approve: payload conflict for slot {slot} — row stays Parked");
        }
        ApproveGate::Expired => {
            let mut row = row.expect("gate saw a row");
            row.state = crate::agent_ref::EffectState::Unknown;
            row.evidence = Some("approval_expired".to_string());
            crate::request_receipts::journal_door_row(project, &row)?;
            let asker = row.asker.clone().unwrap_or_default();
            if !asker.is_empty() {
                let body = crate::request_receipts::approval_body(
                    request_key,
                    "expired",
                    "approval wait bound elapsed",
                    None,
                );
                let args = toron_core::mailbox::messaging::SendMessage {
                    project_slug: project.to_string(),
                    sender_name: asker.clone(),
                    to: vec![asker],
                    subject: "[approval] expired".to_string(),
                    body_md: body,
                    ..Default::default()
                };
                crate::mail::block_on(crate::mail::comms_mailbox()?.send_message(&args))?;
            }
            print_json(&json!({
                "approval": "expired",
                "request_key": request_key,
                "slot": slot,
            }));
            return Ok(());
        }
        ApproveGate::Ready => {}
    }
    let mut row = row.expect("gate Ready means a row");
    let payload = row.payload.clone().expect("gate Ready means a payload");
    let asker = row.asker.clone().expect("gate Ready means an asker");
    // The operator must be a known identity, and must not be the asker.
    // npub compare is alias-proof; the pin compare covers unarrived askers.
    let approver_npub = crate::mail::block_on(crate::mail::comms_mailbox()?.whois(project, approver))
        .map_err(|_| {
            anyhow::anyhow!("captain approve: unknown approver {approver:?} — approval_mismatch")
        })?
        .npub;
    let asker_npub = crate::mail::block_on(crate::mail::comms_mailbox()?.whois(project, &asker))
        .ok()
        .map(|w| w.npub);
    if asker == approver || asker_npub.as_deref().is_some_and(|a| a == approver_npub) {
        print_json(&json!({
            "approval": "mismatch",
            "request_key": request_key,
            "slot": slot,
            "reason": "asker cannot approve its own steer",
        }));
        anyhow::bail!("captain approve: asker cannot approve its own steer — approval_mismatch");
    }
    // D12: an asker who has not arrived in this project parks cross-project;
    // the approval body gains the project line, bound to the steer project.
    let cross_project = asker_npub.is_none();
    let body = crate::request_receipts::approval_body(
        request_key,
        decision,
        note,
        cross_project.then_some(payload.project.as_str()),
    );
    let args = toron_core::mailbox::messaging::SendMessage {
        project_slug: project.to_string(),
        sender_name: approver.to_string(),
        to: vec![asker.clone()],
        subject: format!("[approval] {decision} {request_key}"),
        body_md: body.clone(),
        ..Default::default()
    };
    let sent = crate::mail::block_on(crate::mail::mailbox().send_message(&args))
        .map_err(|e| anyhow::anyhow!("captain approve: approval mail not accepted: {e:#}"))?;
    // The body must parse back: never claim a mail our own verifier rejects.
    // (Notices skip this — `decision=expired` is not an approval.)
    if crate::request_receipts::parse_approval_body(&body)
        .filter(|p| p.decision == decision)
        .is_none()
    {
        anyhow::bail!("captain approve: generated body does not parse — approval_mismatch");
    }
    if !sent.verified_sender {
        print_json(&json!({
            "approval": "mismatch",
            "request_key": request_key,
            "slot": slot,
            "reason": "surface would not vouch for the sender",
        }));
        anyhow::bail!("captain approve: unverified sender — approval_mismatch");
    }
    // Fetch-back: the asker's inbox must hold the exact lines authorized.
    // Delivery that cannot be read back is not claimed.
    let inbox = crate::mail::block_on(crate::mail::comms_mailbox()?.fetch_inbox(
        &toron_core::mailbox::messaging::FetchInbox {
            project_slug: project.to_string(),
            agent_name: asker.clone(),
            unread_only: false,
            limit: Some(10),
            include_bodies: true,
            ack_overdue_only: false,
            urgent_only: false,
            since_ts: None,
            thread_id: None,
        },
    ))
    .map_err(|e| anyhow::anyhow!("captain approve: inbox read-back failed: {e:#}"))?;
    if !inbox
        .iter()
        .any(|m| m.sender == approver && m.body_md == body)
    {
        print_json(&json!({
            "approval": "mismatch",
            "request_key": request_key,
            "slot": slot,
            "reason": "approval mail not found in asker inbox",
        }));
        anyhow::bail!(
            "captain approve: approval mail not found in asker inbox — approval_mismatch"
        );
    }
    if decision == "deny" {
        row.state = crate::agent_ref::EffectState::Settled;
        row.evidence = Some(
            crate::captain_directory::Delivery::Denied
                .as_str()
                .to_string(),
        );
        crate::request_receipts::journal_door_row(project, &row)?;
        print_json(&json!({
            "approval": "approval_denied",
            "request_key": request_key,
            "slot": slot,
        }));
        return Ok(());
    }
    // ADR-0019 D9: the approved payload targets an existing bead — check the
    // hold before the first pane write. Held settles file_held (the mail was
    // accepted); an untellable holder settles Unknown, never a dispatch.
    match crate::check_bead_file_hold(cfg, project, &payload.objective)? {
        crate::HoldVerdict::Clear => {}
        crate::HoldVerdict::Held => {
            row.state = crate::agent_ref::EffectState::Settled;
            row.evidence = Some("file_held".to_string());
            crate::request_receipts::journal_door_row(project, &row)?;
            print_json(&json!({
                "approval": "file_held",
                "request_key": request_key,
                "slot": slot,
            }));
            eprintln!("flywheel: approve: payload target is file-held — not dispatching");
            return Ok(());
        }
        crate::HoldVerdict::Unknown(msg) => {
            row.state = crate::agent_ref::EffectState::Unknown;
            row.evidence = Some(msg.clone());
            crate::request_receipts::journal_door_row(project, &row)?;
            anyhow::bail!("captain approve: file-hold check could not notify the holder: {msg}");
        }
    }
    // Approve: Parked -> InFlight, run the payload, settle by outcome.
    row.state = crate::agent_ref::EffectState::InFlight;
    crate::request_receipts::journal_door_row(project, &row)?;
    let out = crate::run_approved_steer(cfg, &payload);
    let delivery = match &out {
        Ok(()) => crate::captain_directory::Delivery::Queued,
        Err(_) => crate::captain_directory::Delivery::Unknown,
    };
    match &out {
        Ok(()) => {
            row.state = crate::agent_ref::EffectState::Settled;
        }
        Err(e) => {
            row.state = crate::agent_ref::EffectState::Unknown;
            eprintln!("flywheel: approve: payload dispatch failed: {e:#}");
        }
    }
    row.evidence = Some(delivery.as_str().to_string());
    crate::request_receipts::journal_door_row(project, &row)?;
    print_json(&json!({
        "approval": "approved",
        "request_key": request_key,
        "slot": slot,
        "delivery": delivery.as_str(),
    }));
    out.map_err(|e| anyhow::anyhow!("captain approve: payload dispatch failed: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(us: i64, subject: &str) -> Value {
        json!({ "created_ts_us": us, "subject": subject, "sender": "S" })
    }

    /// bd-captain-watch-preflight-arm-snak: the watch's preflight arm names
    /// EXACTLY the first failing check and fires only on a `Fail`. A skip is
    /// advisory, a pass is silent, and the message carries the remedy evidence
    /// (a store-skew refusal names the ceiling and its marker).
    #[test]
    fn captain_watch_exits_on_preflight_failure() {
        use crate::preflight::{Check, Status};
        // Path clear: passes and skips only → the arm never fires.
        let clean = vec![
            Check::pass("resonate"),
            Check::skip("doorbell", "embedded (no [doorbell] url configured)"),
        ];
        assert!(
            first_failing_check(&clean).is_none(),
            "a clean preflight must not exit the watch"
        );
        // The skew the bead observed: the toron store is ahead of the binary.
        let skewed = vec![
            Check::pass("pane-plane"),
            Check::fail(
                "toron-mail",
                "migration 17 is applied in the store but this toron build resolves at most 16 \
                 [toron-error=store_skew] — reinstall toron from the source that wrote the store",
            ),
            Check::fail("project-dir", "proj does not exist"),
        ];
        let (name, msg) = first_failing_check(&skewed).expect("a failing check arms the exit");
        assert_eq!(name, "toron-mail", "the FIRST failure is the one named");
        assert!(
            msg.contains("store_skew") && msg.contains("reinstall toron"),
            "the message carries the failing check's remedy line: {msg}"
        );
        // Status is the only discriminator — a Skip whose reason reads like a
        // failure is not a failure.
        assert!(
            first_failing_check(&[Check::skip("x", "preflight failed")]).is_none(),
            "skip is advisory, never an exit"
        );
        // A fail-only set is still an exit even with every other check green.
        assert_eq!(
            first_failing_check(&[Check::pass("a"), Check::fail("b", "boom")]).map(|(n, _)| n),
            Some("b")
        );
        // Sanity: the discriminants the arm reads are the ones the renderer uses.
        assert!(matches!(skewed[1].status, Status::Fail(_)));
    }

    /// The poll-exit contract: the mail door reports EXACTLY the newest row
    /// strictly newer than the water, and the stall door fires only on
    /// idle-plus-silent with work left.
    #[test]
    fn captain_watch_poll_exit() {
        // Mail: nothing newer than the water is not an exit.
        let armed = vec![row(1_000, "old"), row(2_000, "also old")];
        assert!(
            newest_since(&armed, 2_000).is_none(),
            "a row at the water is not new mail"
        );
        // A strictly-newer row is the exit, and the NEWEST such row wins.
        let mut rows = armed.clone();
        rows.push(row(3_000, "new"));
        rows.push(row(4_000, "newer"));
        let hit = newest_since(&rows, 2_000).expect("new mail");
        assert_eq!(hit.get("subject").and_then(Value::as_str), Some("newer"));
        // A malformed row is not an arrival.
        let junk = vec![json!({ "subject": "no ts" }), row(2_000, "at water")];
        assert!(newest_since(&junk, 2_000).is_none());

        // Stall: no work left is complete, regardless of the clock.
        let base = StallProbe {
            work_remaining: false,
            any_pane_working: false,
            fleet_knowable: true,
            last_commit_age_secs: Some(99_999),
            stall_secs: 2_700,
        };
        assert_eq!(stall_verdict(&base), StallVerdict::AllComplete);
        // Idle, knowable fleet, old commit, work left → the one stall shape.
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                ..base
            }),
            StallVerdict::Stalled
        );
        // A working pane is never a stall.
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: true,
                ..base
            }),
            StallVerdict::Working
        );
        // An unknowable fleet is never alarmed on.
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: false,
                fleet_knowable: false,
                ..base
            }),
            StallVerdict::Working
        );
        // Under threshold, or with no commit history, it is still working.
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: false,
                fleet_knowable: true,
                last_commit_age_secs: Some(60),
                stall_secs: 2_700,
            }),
            StallVerdict::Working
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: false,
                fleet_knowable: true,
                last_commit_age_secs: None,
                stall_secs: 2_700,
            }),
            StallVerdict::Working
        );
    }

    /// ADR-0019 D10: approve classifies the loaded row before any I/O.
    /// Missing is mismatch; settled replays (denied replays as
    /// approval_denied); parked needs payload + asker + timestamp, then
    /// expiry, then the rehash; anything unresolved is in_flight.
    #[test]
    fn approve_row_gate_classifies_before_io() {
        use crate::agent_ref::EffectState;
        use crate::request_receipts::{DoorReceipt, SteerPayload};

        fn parked() -> DoorReceipt {
            let mut r = DoorReceipt::prepare("steer", "s", "d");
            r.state = EffectState::Parked;
            r.payload = Some(SteerPayload {
                objective: "o".to_string(),
                crew: String::new(),
                direct: false,
                reuse: String::new(),
                retain: false,
                retire_orphans: false,
                project: "p".to_string(),
                bead_id: None,
            });
            r.asker = Some("asker".to_string());
            r.parked_at_ms = Some(1_000);
            r
        }
        let fresh = 1_000 + 60_000;
        let stale = 1_000 + crate::captain_directory::APPROVAL_WAIT_MS as u64 + 1;

        assert_eq!(approve_row_gate(None, true, fresh), ApproveGate::Missing);
        let mut settled = parked();
        settled.state = EffectState::Settled;
        settled.evidence = Some("queued".to_string());
        assert_eq!(
            approve_row_gate(Some(&settled), true, fresh),
            ApproveGate::Replay
        );
        settled.evidence = Some("denied".to_string());
        assert_eq!(
            approve_row_gate(Some(&settled), true, fresh),
            ApproveGate::DeniedReplay
        );
        let mut flying = parked();
        flying.state = EffectState::InFlight;
        assert_eq!(
            approve_row_gate(Some(&flying), true, fresh),
            ApproveGate::InFlight
        );
        let mut nopay = parked();
        nopay.payload = None;
        assert_eq!(
            approve_row_gate(Some(&nopay), true, fresh),
            ApproveGate::NoPayload
        );
        let mut noask = parked();
        noask.asker = None;
        assert_eq!(
            approve_row_gate(Some(&noask), true, fresh),
            ApproveGate::NoAsker
        );
        assert_eq!(
            approve_row_gate(Some(&parked()), true, stale),
            ApproveGate::Expired
        );
        assert_eq!(
            approve_row_gate(Some(&parked()), false, fresh),
            ApproveGate::Conflict
        );
        assert_eq!(
            approve_row_gate(Some(&parked()), true, fresh),
            ApproveGate::Ready
        );
    }

    /// ADR-0019 D2/D4: the directory projects tiers, session keys, and
    /// addressability from loaded rows — no I/O, no clock.
    #[test]
    fn directory_assembles_tiers_keys_and_address() {
        let pins = vec![
            DirPin {
                pin: "LivePin".to_string(),
                harness: "pi".to_string(),
                pane_id: Some("block:abc".to_string()),
                location_hosted: false,
                live: true,
                registered: true,
            },
            DirPin {
                pin: "HostedPin".to_string(),
                harness: "grok".to_string(),
                pane_id: None,
                location_hosted: true,
                live: false,
                registered: true,
            },
        ];
        let (sessions, truncated, address) = assemble_directory(&pins, &[], &[]);
        assert!(!truncated);
        let live = sessions.iter().find(|s| s.pin == "LivePin").expect("live");
        assert_eq!(live.tier, crate::captain_directory::Tier::Push);
        assert_eq!(live.session_key, "block:block:abc");
        assert_eq!(
            address
                .iter()
                .find(|(p, _)| p == "LivePin")
                .map(|(_, w)| w.as_str()),
            Some("ok block:block:abc")
        );
        let hosted = sessions
            .iter()
            .find(|s| s.pin == "HostedPin")
            .expect("hosted");
        assert_eq!(hosted.tier, crate::captain_directory::Tier::Pull);
        assert_eq!(hosted.session_key, "hosted:HostedPin");
        // A file-held bead resolves to its code through the same snapshot.
        let snap = crate::captain_directory::DirSnapshot {
            sessions,
            bead_assignee: [("bd-1".to_string(), "LivePin".to_string())]
                .into_iter()
                .collect(),
            peer_held_beads: std::collections::BTreeSet::new(),
            file_held_beads: ["bd-1".to_string()].into_iter().collect(),
            arrived_pins: ["LivePin".to_string(), "HostedPin".to_string()]
                .into_iter()
                .collect(),
        };
        assert!(matches!(
            crate::captain_directory::resolve_address(&snap, "bead:bd-1", "LivePin"),
            Err(e) if e.code() == "file_held"
        ));
    }
}
