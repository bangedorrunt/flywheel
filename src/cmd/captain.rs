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
//! poll-exit contract is testable without a live inbox, hc, or repo.

use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[cfg(feature = "backend-superlogical")]
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
        /// act as this registered agent (adjective+noun or terminal short name)
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
    /// one mikasa-side pull-fresh tick (ADR-0019 D2 amendment 2026-11-01):
    /// read `toron mail inbox` as the hosted pin so the durable cursor
    /// advances. Does not install or load any launchd unit.
    #[command(name = "pull-fresh-tick")]
    PullFreshTick {
        project: String,
        /// hosted captain pin whose inbox cursor must advance
        #[arg(long = "as")]
        as_agent: String,
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
        CaptainCmd::PullFreshTick { project, as_agent } => cmd_pull_fresh_tick(project, as_agent),
    }
}

/// Exit code for a stall alarm. Distinct from 1 (usage/runtime error) so the
/// re-arming wrapper can tell a real stall from a failed poll.
const STALL_EXIT: i32 = 3;

/// Exit code for a preflight-arm failure. Its own code (not 3, not 1) so the
/// re-arming wrapper tells a broken environment from a stalled fleet.
const PREFLIGHT_EXIT: i32 = 4;

/// One poll's observation of the fleet, as data: the plane's verdict for a pin
/// and what that pin is holding unread.
///
/// This is the whole input to [`IdleWatch::step`]; the caller does the I/O
/// (plane read, inbox read, deferral read) at the edge.
pub(crate) struct IdleSample {
    /// The crew pin.
    pub pin: String,
    /// The pane is occupied right now (working/busy/blocked).
    pub working: bool,
    /// Subjects of unread captain mail plus pending deferrals. Empty means the
    /// pin holds nothing a page could name.
    pub unread: Vec<String>,
}

/// One page the watch owes the captain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdlePage {
    pub pin: String,
    pub subjects: Vec<String>,
}

/// The captain-watch idle fold (bd-iscat).
///
/// Fires when a pin transitions working -> idle while it holds unread captain
/// mail (or an unrung deferral): the case where a crew sits on a brief and the
/// operator notices before the captain does. Once per transition by
/// construction: the page needs the pin in the PREVIOUS sample's working set,
/// which the next sample consumes. An empty inbox never pages, and a pin idle
/// since before the watch armed did not transition.
#[derive(Default)]
pub(crate) struct IdleWatch {
    /// Pins working at the previous sample.
    working: std::collections::BTreeSet<String>,
}

impl IdleWatch {
    /// Fold one sample. Pure: no clock, no I/O, no policy knobs.
    pub(crate) fn step(&mut self, sample: &[IdleSample]) -> Vec<IdlePage> {
        let mut pages = Vec::new();
        let mut working_now = std::collections::BTreeSet::new();
        for row in sample {
            if row.working {
                working_now.insert(row.pin.clone());
                continue;
            }
            if self.working.contains(&row.pin) && !row.unread.is_empty() {
                pages.push(IdlePage {
                    pin: row.pin.clone(),
                    subjects: row.unread.clone(),
                });
            }
        }
        self.working = working_now;
        pages
    }
}

/// Is this inbox row a captain-side brief for `pin`?
///
/// Captain-side means a sender that is neither the pin itself nor another crew
/// pin: crew-to-crew handoffs are the crew's business, not a captain page. The
/// classifier is pure over the row so the wire rows `captain watch` already
/// holds are testable without an inbox.
fn is_captain_row(row: &Value, pin: &str, crew: &std::collections::BTreeSet<String>) -> bool {
    let Some(sender) = row.get("sender").and_then(Value::as_str) else {
        return false;
    };
    if sender == pin {
        return false;
    }
    !crew.contains(sender)
}

/// The subjects of the rows a pin holds from the captain side.
///
/// A system notice (empty subject from a 64-hex sender) is not a brief and is
/// dropped by the doorbell's own classifier, shared here so the page names only
/// what a person can act on; a real row with no subject still gets a label.
fn captain_subjects(
    rows: &[Value],
    pin: &str,
    crew: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    rows.iter()
        .filter(|row| is_captain_row(row, pin, crew))
        .filter(|row| !crate::mail_wake::is_system_notice_value(row))
        .map(
            |row| match row.get("subject").and_then(Value::as_str).map(str::trim) {
                Some(subject) if !subject.is_empty() => subject.to_string(),
                _ => "(no subject)".to_string(),
            },
        )
        .collect()
}

/// What one pin holds: captain-side inbox subjects, plus the deferral marker
/// when it holds a wake that never rang. Pure, so the decision is pinned
/// without an inbox, a journal or a plane.
const UNRUNG_DEFERRAL: &str = "unrung deferral (the doorbell never rang)";

fn pin_unread(
    rows: &[Value],
    pin: &str,
    crew: &std::collections::BTreeSet<String>,
    deferred: bool,
) -> Vec<String> {
    let mut unread = captain_subjects(rows, pin, crew);
    if deferred {
        unread.push(UNRUNG_DEFERRAL.to_string());
    }
    unread
}

/// The idle arm's edge: crew pins from the registry, status from the snapshot
/// this poll already read, and what each pin holds unread (captain mail plus
/// any unrung deferral). All I/O lives here; [`IdleWatch::step`] sees data.
fn idle_sample(
    mail: &crate::mail::Mail,
    project: &str,
    key: &str,
    snap: &crate::snapshot::Snapshot,
    registry: &[crate::state::SpawnEntry],
) -> Result<Vec<IdleSample>> {
    let crew: std::collections::BTreeSet<String> = registry
        .iter()
        .filter_map(|e| e.mail_identity.clone().or_else(|| Some(e.name.clone())))
        .filter(|pin| !pin.is_empty())
        .collect();
    // A wake that never rang is held in one of two places: the handoff file
    // (not yet taken) or the journal (taken, never drained). Both count.
    let state_dir = crate::config::state_dir();
    let mut deferred: std::collections::BTreeSet<String> = crate::doorbell::unrung_deferrals(
        &state_dir,
        crate::doorbell::project_workspaces(&state_dir, project),
    )
    .into_iter()
    .map(|(pin, _)| pin)
    .collect();
    deferred.extend(
        crate::doorbell::pending_deferred_wakes(&state_dir, project)
            .into_iter()
            .map(|(pin, _)| pin),
    );
    let mut sample = Vec::with_capacity(registry.len());
    for entry in registry {
        let pin = entry
            .mail_identity
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| entry.name.clone());
        let working = snap
            .pins
            .iter()
            .any(|row| row.pane_id == entry.pane_id && row.verdict.starts_with("Busy"));
        // Each crew pin has its own inbox: read AS the pin, not as the watch's
        // identity, or the mailbox answers for the wrong reader.
        let unread_rows = mail
            .clone()
            .with_name(pin.clone())
            .inbox_filtered(
                key,
                &pin,
                &MailInboxFilter {
                    unread_only: true,
                    include_bodies: false,
                    limit: 50,
                    ..Default::default()
                },
            )
            .map(|v| v.as_array().cloned().unwrap_or_default())
            .unwrap_or_default();
        let unread = pin_unread(&unread_rows, &pin, &crew, deferred.contains(&pin));
        sample.push(IdleSample {
            pin,
            working,
            unread,
        });
    }
    Ok(sample)
}

/// The FIRST failing check as `(name, message)`. Skips are advisory (the
/// operation can proceed) and passes are silent; only a `Fail` arms the exit.
/// Pure, so the arm's rule is pinned without a live hc/toron.
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
    // bd-271ws: the doorbell's system-notice classifier, shared — an empty-
    // subject reservation-release DM never wakes the captain.
    rows.iter()
        .filter(|row| created_us(row).is_some_and(|us| us > baseline_us))
        .filter(|row| !crate::mail_wake::is_system_notice_value(row))
        .max_by_key(|row| created_us(row).unwrap_or(i64::MIN))
}

/// One row's creation time in unix micros. Missing/unparseable → None (the row
/// is not evidence of new mail, never a false arrival).
fn created_us(row: &Value) -> Option<i64> {
    row.get("created_ts_us").and_then(Value::as_i64)
}

/// What one stall probe observed. Shared with `loop-mode supervise` (rweq),
/// so the captain watch and the supervisor judge the same fleet the same way.
#[derive(Debug, Clone)]
pub(crate) struct StallProbe {
    /// any bead not closed/done
    pub work_remaining: bool,
    /// a pin reads Busy (working/busy/blocked — an occupied pane)
    pub any_pane_working: bool,
    /// plane pins were readable. False → the fleet is UNKNOWABLE, so we never
    /// alarm blind (a degraded read must not be read as calm AND must not be
    /// read as a stall).
    pub fleet_knowable: bool,
    /// seconds since the newest commit in the project, None when git is absent
    pub last_commit_age_secs: Option<u64>,
    /// a clean-proof/build slot is held (bd-0uzrq.5): a 466s suite is work,
    /// not a stall, even though no pane output and no commit arrives.
    pub proof_slot_held: bool,
    /// seconds since the newest pane-output delta the watcher observed. None
    /// when no pane tail was readable — then the commit clock is all we have.
    pub activity_age_secs: Option<u64>,
    /// longest run of identical tails on one busy pane (same error, again and
    /// again). 0 when nothing is readable or every sample moved.
    pub loop_repeats: u32,
    /// the pane holding that run, named in the verdict
    pub loop_pin: Option<String>,
    /// how long the identical run has held. Three samples at a 15s poll is
    /// ~45s — an ordinary think, not a spin.
    pub loop_repeat_secs: u64,
    /// how long an identical run must hold before it can page (bd-0uzrq.25)
    pub loop_min_secs: u64,
    pub stall_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallVerdict {
    Working,
    AllComplete,
    Stalled,
    /// A busy pane repeats one tail (bd-0uzrq.5): a crew spinning on one error
    /// looks alive to a wall clock, so it gets its own verdict.
    Looping,
}

/// How many identical tails on one busy pane make a loop. Three samples is
/// short enough to catch a spin and long enough that a two-line flicker does
/// not trip it.
pub(crate) const LOOP_REPEATS: u32 = 3;

/// How long the identical run must hold before it is a loop. Three samples at
/// the 15s poll is ~45s, which is an ordinary LLM think or one long tool call;
/// the true positive this verdict exists for ran 13 minutes. Five minutes is
/// well past a turn. `CAPTAIN_LOOP_MIN_SECS` overrides it.
pub(crate) const LOOP_MIN_SECS: u64 = 300;

/// The loop window from a raw env value: unset or unparsable keeps
/// [`LOOP_MIN_SECS`], so a typo can never turn the alarm off.
#[must_use]
pub(crate) fn loop_min_secs_from_env(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(LOOP_MIN_SECS)
}

/// The stall decision. `AllComplete` wins first (nothing left to do), then an
/// unknowable fleet (never alarm blind), then the activity facts: a held proof
/// slot and a moving pane are work, and only a silent fleet falls through to
/// the commit clock. A busy pane repeating one tail is `Looping` even though
/// it reads Busy — that is the case a wall clock cannot see.
fn stall_verdict(p: &StallProbe) -> StallVerdict {
    if !p.work_remaining {
        return StallVerdict::AllComplete;
    }
    if !p.fleet_knowable {
        return StallVerdict::Working;
    }
    if p.proof_slot_held {
        return StallVerdict::Working;
    }
    if p.loop_repeats >= LOOP_REPEATS && p.loop_repeat_secs >= p.loop_min_secs {
        return StallVerdict::Looping;
    }
    if p.any_pane_working {
        return StallVerdict::Working;
    }
    if p.activity_age_secs.is_some_and(|age| age < p.stall_secs) {
        return StallVerdict::Working;
    }
    match p.last_commit_age_secs {
        Some(age) if age >= p.stall_secs => StallVerdict::Stalled,
        _ => StallVerdict::Working,
    }
}

#[allow(clippy::too_many_arguments)]
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
    if let Some(mode) = preflight
        && let Some(code) = preflight_arm_exit(cfg, project, mode, json_out)
    {
        std::process::exit(code);
    }
    let interval = interval.max(1);
    let key = crate::project_dir_str(cfg, project)?;
    let mail = crate::mail_as(cfg, &key, as_agent)?;
    mail.ensure_project(&key)?;

    let armed = inbox_rows(&mail, &key, None)?;
    let baseline_us = armed.iter().filter_map(created_us).max().unwrap_or(0);
    let baseline_rfc = doorbell_core::inbox_high_water_from_values(armed.iter().collect());

    eprintln!(
        "flywheel: captain watch {project} armed (mail water {}, stall {}, idle-page on); Ctrl-C to abort",
        baseline_rfc.as_deref().unwrap_or("<empty>"),
        if stall {
            format!("on, {} min", stall_minutes.max(1))
        } else {
            "off".to_string()
        }
    );

    let stall_secs = stall_minutes.max(1) * 60;
    let project_dir = crate::project_dir(cfg, project);
    let stall_backend = if stall {
        crate::backend::factory::multiplexer_backend(cfg).ok()
    } else {
        None
    };
    let mut activity = ActivityWatch::default();
    let registry = crate::state::load_spawn_registry(project);
    let mut idle = IdleWatch::default();
    // Seed the fold with the fleet as it stands at arm time: a pin already
    // working must page if it goes idle before the first poll.
    let armed_now = chrono::Utc::now().timestamp().max(0) as u64;
    let armed_snap =
        crate::snapshot::assemble_with_label(cfg, project, &swarm_label_for(project), armed_now);
    let _ = idle.step(&idle_sample(&mail, project, &key, &armed_snap, &registry)?);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));

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

        if let Some(mode) = preflight
            && let Some(code) = preflight_arm_exit(cfg, project, mode, json_out)
        {
            std::process::exit(code);
        }

        let now = chrono::Utc::now().timestamp().max(0) as u64;
        let snap =
            crate::snapshot::assemble_with_label(cfg, project, &swarm_label_for(project), now);

        if let Some(page) = idle
            .step(&idle_sample(&mail, project, &key, &snap, &registry)?)
            .into_iter()
            .next()
        {
            if json_out {
                print_json(&json!({
                    "reason": "idle",
                    "project": project,
                    "pin": page.pin,
                    "subjects": page.subjects,
                }));
            } else {
                println!(
                    "{} went idle holding {} unread: {}",
                    page.pin,
                    page.subjects.len(),
                    page.subjects.join(" | ")
                );
                println!(
                    "read it: flywheel mail inbox {project} --as {} --unread",
                    page.pin
                );
            }
            return Ok(());
        }

        if !stall {
            continue;
        }
        let probe = stall_probe_from(&snap, &project_dir, now, stall_secs, {
            match stall_backend.as_deref() {
                Some(backend) => {
                    activity.observe(now, &read_pane_tails(backend, &snap), &busy_panes(&snap))
                }
                None => ActivityFacts::default(),
            }
        });
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
            StallVerdict::Looping => {
                let repeats = probe.loop_repeats;
                let pin = probe.loop_pin.clone().unwrap_or_default();
                let held = probe.loop_repeat_secs;
                if json_out {
                    print_json(&json!({
                        "reason": "looping",
                        "project": project,
                        "pin": pin,
                        "stall_secs": stall_secs,
                        "loop_repeats": repeats,
                        "loop_repeat_secs": held,
                        "no_commit_secs": probe.last_commit_age_secs.unwrap_or(0),
                        "work_remaining": true,
                    }));
                } else {
                    println!(
                        "LOOPING in {project}: {pin} produced the same tail {repeats}x \
                         over {held}s with no new output — spinning on one error, not progressing"
                    );
                }
                std::process::exit(STALL_EXIT);
            }
            StallVerdict::Stalled => {
                let age = probe.last_commit_age_secs.unwrap_or(0);
                if json_out {
                    print_json(&json!({
                        "reason": "stall",
                        "project": project,
                        "stall_secs": stall_secs,
                        "no_commit_secs": age,
                        "activity_age_secs": probe.activity_age_secs,
                        "proof_slot_held": probe.proof_slot_held,
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
    let snap = crate::snapshot::assemble_with_label(cfg, project, &swarm_label_for(project), now);
    stall_probe_from(
        &snap,
        project_dir,
        now,
        stall_secs,
        ActivityFacts::default(),
    )
}

/// The workspace label one project's plane session carries.
fn swarm_label_for(project: &str) -> String {
    crate::state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project))
}

/// [`probe_stall`] over a snapshot the caller already read, so a watch that
/// needs the same fleet for two arms reads the plane once per poll.
pub(crate) fn stall_probe_from(
    snap: &crate::snapshot::Snapshot,
    project_dir: &std::path::Path,
    now: u64,
    stall_secs: u64,
    activity: ActivityFacts,
) -> StallProbe {
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
        proof_slot_held: proof_slot_held(),
        activity_age_secs: activity.activity_age_secs,
        loop_repeats: activity.loop_repeats,
        loop_pin: activity.loop_pin,
        loop_repeat_secs: activity.loop_repeat_secs,
        loop_min_secs: loop_min_secs_from_env(
            std::env::var("CAPTAIN_LOOP_MIN_SECS").ok().as_deref(),
        ),
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

/// What the watcher observed since the previous poll (bd-0uzrq.5). Data, not a
/// decision: the verdict stays in [`stall_verdict`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ActivityFacts {
    /// seconds since the newest pane-output delta, None when no tail was read
    pub activity_age_secs: Option<u64>,
    /// longest run of identical tails on one busy pane
    pub loop_repeats: u32,
    /// the pane holding the longest run
    pub loop_pin: Option<String>,
    /// how long that run has held
    pub loop_repeat_secs: u64,
}

/// Consecutive-sample state for the stall arm. Pure: [`observe`](Self::observe)
/// takes the pane tails as data, so a fake clock and fake digests are the whole
/// test surface.
#[derive(Debug, Default)]
pub(crate) struct ActivityWatch {
    tails: BTreeMap<String, String>,
    repeats: BTreeMap<String, u32>,
    /// when the current identical run on a pane started
    loop_since: BTreeMap<String, u64>,
    last_delta_secs: Option<u64>,
}

impl ActivityWatch {
    /// Fold one poll. `busy` names the panes the plane says are mid-turn; only
    /// those can be looping, because an idle pane repeats its tail by
    /// definition. A first sighting of a tail is a delta — a sample that names
    /// no pane has no clock, which is honest: nothing was read.
    pub(crate) fn observe(
        &mut self,
        now_secs: u64,
        tails: &BTreeMap<String, String>,
        busy: &BTreeSet<String>,
    ) -> ActivityFacts {
        let mut delta = false;
        for (pane, tail) in tails {
            let same = self.tails.get(pane).is_some_and(|prev| prev == tail);
            if !same {
                delta = true;
            }
            let repeats = if same {
                self.repeats.get(pane).copied().unwrap_or(1) + 1
            } else {
                1
            };
            if same && repeats == 2 {
                self.loop_since.insert(pane.clone(), now_secs);
            } else if !same {
                self.loop_since.remove(pane);
            }
            self.repeats.insert(pane.clone(), repeats);
            self.tails.insert(pane.clone(), tail.clone());
        }
        if delta {
            self.last_delta_secs = Some(now_secs);
        }
        let longest = tails
            .keys()
            .filter(|pane| busy.contains(*pane))
            .max_by_key(|pane| self.repeats.get(*pane).copied().unwrap_or(0));
        ActivityFacts {
            activity_age_secs: self.last_delta_secs.map(|at| now_secs.saturating_sub(at)),
            loop_repeats: longest
                .and_then(|pane| self.repeats.get(pane).copied())
                .unwrap_or(0),
            loop_pin: longest.cloned(),
            loop_repeat_secs: longest
                .and_then(|pane| self.loop_since.get(pane))
                .map(|since| now_secs.saturating_sub(*since))
                .unwrap_or(0),
        }
    }
}

/// The machine-wide clean-proof slot file, the same path `clean-proof.sh`
/// flocks (`CLEAN_PROOF_LOCK_FILE`, else `clean-proof.lock` under
/// `XDG_RUNTIME_DIR` or `TMPDIR`).
fn proof_slot_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CLEAN_PROOF_LOCK_FILE") {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR").or_else(|| std::env::var_os("TMPDIR"))?;
    Some(PathBuf::from(base).join("clean-proof.lock"))
}

/// True when another process holds the proof slot. The flock is the truth: a
/// stale `.holder` file beside a free lock cannot fake a running proof.
pub(crate) fn proof_slot_held() -> bool {
    match proof_slot_path() {
        Some(path) => proof_slot_held_at(&path),
        None => false,
    }
}

fn proof_slot_held_at(path: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(path) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// One readable tail per pane, keyed by pane id and prefixed with the plane's
/// verdict so a status transition counts as a delta even when the text repeats.
fn read_pane_tails(
    backend: &dyn crate::backend::MultiplexerBackend,
    snap: &crate::snapshot::Snapshot,
) -> BTreeMap<String, String> {
    let mut tails = BTreeMap::new();
    for pin in &snap.pins {
        if pin.pane_id.is_empty() {
            continue;
        }
        let Ok(read) = backend.agent_read(&pin.pane_id, "screen", 120) else {
            continue;
        };
        let Some(text) = pane_tail_text(&read) else {
            continue;
        };
        tails.insert(pin.pane_id.clone(), format!("{}\u{1f}{text}", pin.verdict));
    }
    tails
}

/// The text of an `agent_read` reply, whatever shape the backend used. A shape
/// this does not know is not evidence, so the pane drops out of the sample
/// instead of contributing a fabricated digest.
fn pane_tail_text(read: &Value) -> Option<String> {
    match read {
        Value::String(text) => Some(text.clone()),
        Value::Array(rows) => Some(
            rows.iter()
                .map(|row| row.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Object(map) => ["content", "text", "screen", "lines"]
            .iter()
            .find_map(|key| map.get(*key).and_then(pane_tail_text)),
        _ => None,
    }
}

fn busy_panes(snap: &crate::snapshot::Snapshot) -> BTreeSet<String> {
    snap.pins
        .iter()
        .filter(|pin| pin.verdict.starts_with("Busy") && !pin.pane_id.is_empty())
        .map(|pin| pin.pane_id.clone())
        .collect()
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
    /// ADR-0019 D6 harness session id from the spawn registry, if any.
    harness_session: Option<String>,
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
                    p.harness_session.as_deref(),
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

/// ADR-0019 D2 amendment: one mikasa-side pull-fresh tick.
fn cmd_pull_fresh_tick(project: &str, pin: &str) -> Result<()> {
    crate::check_project_spelling(project, "captain pull-fresh-tick")?;
    if pin.trim().is_empty() {
        anyhow::bail!("captain pull-fresh-tick: --as <pin> is required");
    }
    let code = crate::pull_fresh_poller::run_tick(project, pin, |bin, argv| {
        let status = std::process::Command::new(bin).args(argv).status()?;
        Ok(status.code().unwrap_or(1))
    })
    .map_err(|e| anyhow::anyhow!("captain pull-fresh-tick: failed to run toron: {e}"))?;
    if code != 0 {
        anyhow::bail!("captain pull-fresh-tick: toron exited {code}");
    }
    println!("pull-fresh-tick ok project={project} pin={pin}");
    Ok(())
}

/// ADR-0019 D2/D4 read-only directory.
///
/// `flywheel captain directory <project> [--json]`
///
/// bd-nmmf4: this path is rex-only by design, so it needs the rex backend's
/// cargo feature. A lean hc-only build used to fail to COMPILE here
/// (the function named a cfg-gated module unconditionally); it now answers
/// the same question with a loud, actionable refusal.
#[cfg(feature = "backend-superlogical")]
fn cmd_captain_directory(cfg: &Config, project: &str, json_out: bool) -> Result<()> {
    let backend = toron_plane::backend::superlogical::SuperlogicalBackend;
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
            harness_session: e.harness_session.clone(),
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
            harness_session: None,
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

/// ADR-0019 D2/D4 read-only directory, in a build WITHOUT the rex backend.
/// bd-nmmf4: the directory projects rex blocks, so a hc-only build cannot
/// answer it — refuse loudly instead of failing to compile.
#[cfg(not(feature = "backend-superlogical"))]
fn cmd_captain_directory(_cfg: &Config, _project: &str, _json_out: bool) -> Result<()> {
    anyhow::bail!(
        "captain directory needs the rex backend (build with --features backend-superlogical); \
         a hc-only build has no blocks to project"
    )
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
    let approver_npub =
        crate::mail::block_on(crate::mail::comms_mailbox()?.whois(project, approver))
            .map_err(|_| {
                anyhow::anyhow!(
                    "captain approve: unknown approver {approver:?} — approval_mismatch"
                )
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
        let clean = vec![
            Check::pass("engine_store"),
            Check::skip("doorbell", "embedded (no [doorbell] url configured)"),
        ];
        assert!(
            first_failing_check(&clean).is_none(),
            "a clean preflight must not exit the watch"
        );
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
        assert!(
            first_failing_check(&[Check::skip("x", "preflight failed")]).is_none(),
            "skip is advisory, never an exit"
        );
        assert_eq!(
            first_failing_check(&[Check::pass("a"), Check::fail("b", "boom")]).map(|(n, _)| n),
            Some("b")
        );
        assert!(matches!(skewed[1].status, Status::Fail(_)));
    }

    /// bd-iscat: a crew that goes working -> idle holding an unread captain
    /// brief pages ONCE, naming the pin and the subjects — and never twice for
    /// the same idle stretch. A new working stretch re-arms it.
    #[test]
    fn idle_page_fires_once_per_transition() {
        let sample = |working: bool| {
            vec![IdleSample {
                pin: "CyanCove".into(),
                working,
                unread: vec!["[bd-x] brief".into(), "revise the plan".into()],
            }]
        };
        let mut watch = IdleWatch::default();

        assert!(
            watch.step(&sample(false)).is_empty(),
            "a pin idle since before the watch armed did not transition"
        );
        assert!(watch.step(&sample(true)).is_empty(), "working never pages");
        let pages = watch.step(&sample(false));
        assert_eq!(pages.len(), 1, "one page per transition: {pages:?}");
        assert_eq!(pages[0].pin, "CyanCove");
        assert_eq!(
            pages[0].subjects,
            vec!["[bd-x] brief".to_string(), "revise the plan".to_string()],
            "the page names the unread subjects"
        );
        assert!(
            watch.step(&sample(false)).is_empty(),
            "the same idle stretch never pages twice"
        );
        assert!(watch.step(&sample(true)).is_empty());
        assert_eq!(
            watch.step(&sample(false)).len(),
            1,
            "a new idle stretch pages again"
        );
    }

    /// An empty inbox never pages, however the pane moves.
    #[test]
    fn idle_page_never_fires_for_an_empty_inbox() {
        let sample = |working: bool| {
            vec![IdleSample {
                pin: "CyanCove".into(),
                working,
                unread: Vec::new(),
            }]
        };
        let mut watch = IdleWatch::default();
        assert!(watch.step(&sample(true)).is_empty());
        assert!(
            watch.step(&sample(false)).is_empty(),
            "idle with nothing unread is not a page"
        );
        assert!(watch.step(&sample(false)).is_empty());
    }

    /// A wake the host took and never rang is a brief the captain must hear
    /// about, whether it is still in the handoff file or already journaled —
    /// `pin_unread` is the ONE decision both sources feed.
    #[test]
    fn a_queued_deferral_is_an_unread_brief() {
        let crew = std::collections::BTreeSet::from(["CyanCove".to_string()]);
        assert_eq!(
            pin_unread(&[], "CyanCove", &crew, true),
            vec![UNRUNG_DEFERRAL.to_string()],
            "a deferral counts with an otherwise empty inbox"
        );
        assert!(
            pin_unread(&[], "CyanCove", &crew, false).is_empty(),
            "no deferral, nothing unread"
        );
        let rows = vec![json!({"sender": "SlateHarbor", "subject": "[bd-x] brief"})];
        assert_eq!(
            pin_unread(&rows, "CyanCove", &crew, true),
            vec!["[bd-x] brief".to_string(), UNRUNG_DEFERRAL.to_string()],
            "mail and the deferral both surface"
        );
    }

    /// Only captain-side mail counts: a crew peer's unread is not a captain
    /// page, and a pin's own rows are never counted against it.
    #[test]
    fn captain_subjects_skip_crew_senders() {
        let crew =
            std::collections::BTreeSet::from(["CyanCove".to_string(), "MaroonLynx".to_string()]);
        let rows = vec![
            json!({"sender": "MaroonLynx", "subject": "crew handoff"}),
            json!({"sender": "CyanCove", "subject": "to myself"}),
            json!({"sender": "SlateHarbor", "subject": "[bd-x] brief"}),
            json!({"subject": "(no sender)"}),
        ];
        assert_eq!(
            captain_subjects(&rows, "CyanCove", &crew),
            vec!["[bd-x] brief".to_string()],
            "only the captain-side row is named"
        );
        let noise = vec![
            json!({"sender": "a".repeat(64), "subject": ""}),
            json!({"sender": "SlateHarbor", "subject": "  "}),
        ];
        assert_eq!(
            captain_subjects(&noise, "CyanCove", &crew),
            vec!["(no subject)".to_string()],
            "a system notice is not a brief; a blank human subject still gets a label"
        );
        assert!(is_captain_row(&rows[2], "CyanCove", &crew));
        assert!(!is_captain_row(&rows[0], "CyanCove", &crew));
    }

    /// The poll-exit contract: the mail door reports EXACTLY the newest row
    /// strictly newer than the water, and the stall door fires only on
    /// idle-plus-silent with work left.
    #[test]
    fn captain_watch_poll_exit() {
        let armed = vec![row(1_000, "old"), row(2_000, "also old")];
        assert!(
            newest_since(&armed, 2_000).is_none(),
            "a row at the water is not new mail"
        );
        let mut rows = armed.clone();
        rows.push(row(3_000, "new"));
        rows.push(row(4_000, "newer"));
        let hit = newest_since(&rows, 2_000).expect("new mail");
        assert_eq!(hit.get("subject").and_then(Value::as_str), Some("newer"));
        let junk = vec![json!({ "subject": "no ts" }), row(2_000, "at water")];
        assert!(newest_since(&junk, 2_000).is_none());

        // bd-271ws: a system notice (empty subject from a 64-hex sender) is
        // not captain mail; a [bd-x] done row behind it still wakes.
        let key_sender = "a".repeat(64);
        let notice = json!({
            "sender": key_sender,
            "subject": "",
            "created_ts_us": 5_000,
        });
        assert!(
            newest_since(std::slice::from_ref(&notice), 2_000).is_none(),
            "a reservation-release notice never ends the watch"
        );
        let done = row(4_000, "[bd-x] done");
        let mixed = vec![notice, done];
        let hit = newest_since(&mixed, 2_000).expect("the done row wakes");
        assert_eq!(
            hit.get("subject").and_then(Value::as_str),
            Some("[bd-x] done")
        );

        let base = StallProbe {
            work_remaining: false,
            any_pane_working: false,
            fleet_knowable: true,
            last_commit_age_secs: Some(99_999),
            proof_slot_held: false,
            activity_age_secs: None,
            loop_repeats: 0,
            loop_pin: None,
            loop_repeat_secs: 0,
            loop_min_secs: LOOP_MIN_SECS,
            stall_secs: 2_700,
        };
        assert_eq!(stall_verdict(&base), StallVerdict::AllComplete);
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                ..base.clone()
            }),
            StallVerdict::Stalled
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: true,
                ..base.clone()
            }),
            StallVerdict::Working
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: false,
                fleet_knowable: false,
                ..base.clone()
            }),
            StallVerdict::Working
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                work_remaining: true,
                any_pane_working: false,
                fleet_knowable: true,
                last_commit_age_secs: Some(60),
                proof_slot_held: false,
                activity_age_secs: None,
                loop_repeats: 0,
                loop_pin: None,
                loop_repeat_secs: 0,
                loop_min_secs: LOOP_MIN_SECS,
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
                proof_slot_held: false,
                activity_age_secs: None,
                loop_repeats: 0,
                loop_pin: None,
                loop_repeat_secs: 0,
                loop_min_secs: LOOP_MIN_SECS,
                stall_secs: 2_700,
            }),
            StallVerdict::Working
        );
    }

    /// bd-0uzrq.5: the live incident. A 2713s silence while a 466s suite held
    /// the machine-wide proof slot paged the captain. The slot is the activity
    /// fact; wall time alone is not.
    #[test]
    fn a_long_build_holding_the_proof_slot_is_not_a_stall() {
        let base = StallProbe {
            work_remaining: true,
            any_pane_working: false,
            fleet_knowable: true,
            last_commit_age_secs: Some(2_713),
            proof_slot_held: true,
            activity_age_secs: None,
            loop_repeats: 0,
            loop_pin: None,
            loop_repeat_secs: 0,
            loop_min_secs: LOOP_MIN_SECS,
            stall_secs: 2_700,
        };
        assert_eq!(stall_verdict(&base), StallVerdict::Working);
        assert_eq!(
            stall_verdict(&StallProbe {
                proof_slot_held: false,
                ..base.clone()
            }),
            StallVerdict::Stalled,
            "the same silence with the slot free is the stall the clock was right about"
        );
    }

    /// An unchanged screen past the threshold is a stall; a delta inside it is
    /// not, and an unreadable fleet never alarms.
    #[test]
    fn an_unchanged_screen_is_a_stall_and_a_moving_one_is_not() {
        let base = StallProbe {
            work_remaining: true,
            any_pane_working: false,
            fleet_knowable: true,
            last_commit_age_secs: Some(9_999),
            proof_slot_held: false,
            activity_age_secs: Some(2_700),
            loop_repeats: 0,
            loop_pin: None,
            loop_repeat_secs: 0,
            loop_min_secs: LOOP_MIN_SECS,
            stall_secs: 2_700,
        };
        assert_eq!(stall_verdict(&base), StallVerdict::Stalled);
        assert_eq!(
            stall_verdict(&StallProbe {
                activity_age_secs: Some(60),
                ..base.clone()
            }),
            StallVerdict::Working
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                fleet_knowable: false,
                ..base.clone()
            }),
            StallVerdict::Working,
            "a degraded pane read is not a stall"
        );
    }

    /// A busy pane repeating one tail is a loop, even though the pane reads
    /// Busy. Two identical samples are not enough, and a held slot outranks it.
    #[test]
    fn a_busy_pane_repeating_one_tail_is_looping() {
        let base = StallProbe {
            work_remaining: true,
            any_pane_working: true,
            fleet_knowable: true,
            last_commit_age_secs: Some(60),
            proof_slot_held: false,
            activity_age_secs: Some(3_000),
            loop_repeats: LOOP_REPEATS,
            loop_pin: Some("SlateHarbor".into()),
            loop_repeat_secs: LOOP_MIN_SECS,
            loop_min_secs: LOOP_MIN_SECS,
            stall_secs: 2_700,
        };
        assert_eq!(stall_verdict(&base), StallVerdict::Looping);
        assert_eq!(
            base.loop_pin.as_deref(),
            Some("SlateHarbor"),
            "the verdict names the pane it accuses"
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                loop_repeats: LOOP_REPEATS - 1,
                ..base.clone()
            }),
            StallVerdict::Working
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                proof_slot_held: true,
                ..base.clone()
            }),
            StallVerdict::Working,
            "the captain's own long suite is not a loop"
        );
    }

    /// bd-0uzrq.25: three identical tails ~45s apart is a think, not a spin.
    /// Only a run that outlives an ordinary turn may page, and it names the
    /// pane — the 13-minute hung child still pages.
    #[test]
    fn a_thinking_pane_is_not_looping_until_the_run_outlives_a_turn() {
        let base = StallProbe {
            work_remaining: true,
            any_pane_working: true,
            fleet_knowable: true,
            last_commit_age_secs: Some(60),
            proof_slot_held: false,
            activity_age_secs: Some(3_000),
            loop_repeats: LOOP_REPEATS,
            loop_pin: Some("SlateHarbor".into()),
            loop_repeat_secs: 45,
            loop_min_secs: LOOP_MIN_SECS,
            stall_secs: 2_700,
        };
        assert_eq!(
            stall_verdict(&base),
            StallVerdict::Working,
            "45s of one tail is an ordinary think"
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                loop_repeat_secs: LOOP_MIN_SECS - 1,
                ..base.clone()
            }),
            StallVerdict::Working,
            "just under the window is still a think"
        );
        assert_eq!(
            stall_verdict(&StallProbe {
                loop_repeat_secs: 780,
                ..base.clone()
            }),
            StallVerdict::Looping,
            "the 13-minute hung child still pages"
        );
    }

    /// The window is configurable but a typo can never disable the alarm.
    #[test]
    fn loop_min_secs_ignores_junk_and_zero() {
        assert_eq!(loop_min_secs_from_env(None), LOOP_MIN_SECS);
        assert_eq!(loop_min_secs_from_env(Some("nope")), LOOP_MIN_SECS);
        assert_eq!(loop_min_secs_from_env(Some("0")), LOOP_MIN_SECS);
        assert_eq!(loop_min_secs_from_env(Some(" 600 ")), 600);
    }

    /// The sampler: the first poll seeds the clock, a repeated tail ages it, a
    /// moved tail resets it, and only a BUSY pane's repeats count as looping.
    #[test]
    fn activity_watch_seeds_the_clock_and_counts_only_busy_tails() {
        let mut watch = ActivityWatch::default();
        let mut tails = BTreeMap::new();
        tails.insert("pane-a".to_string(), "Busy\u{1f}building".to_string());
        tails.insert("pane-b".to_string(), "Idle\u{1f}prompt".to_string());
        let busy: BTreeSet<String> = ["pane-a".to_string()].into_iter().collect();

        let first = watch.observe(1_000, &tails, &busy);
        assert_eq!(
            first.activity_age_secs,
            Some(0),
            "the first poll seeds the clock"
        );
        assert_eq!(first.loop_repeats, 1);
        assert_eq!(first.loop_pin.as_deref(), Some("pane-a"));

        let same = watch.observe(1_600, &tails, &busy);
        assert_eq!(same.activity_age_secs, Some(600), "an unchanged fleet ages");
        let looped = watch.observe(2_200, &tails, &busy);
        assert_eq!(
            looped.loop_repeats, LOOP_REPEATS,
            "three identical busy tails is a loop"
        );
        assert_eq!(looped.activity_age_secs, Some(1_200));
        assert_eq!(
            looped.loop_repeat_secs, 600,
            "the run is aged from its first repeat, not from the first sighting"
        );

        let mut moved = tails.clone();
        moved.insert("pane-a".to_string(), "Busy\u{1f}compiling".to_string());
        let fresh = watch.observe(2_300, &moved, &busy);
        assert_eq!(fresh.activity_age_secs, Some(0), "a moved tail is a delta");
        assert_eq!(fresh.loop_repeats, 1, "the loop counter resets on movement");

        let idle_only: BTreeSet<String> = BTreeSet::new();
        watch.observe(3_000, &tails, &idle_only);
        assert_eq!(
            watch.observe(3_100, &tails, &idle_only).loop_repeats,
            0,
            "an idle pane repeats its tail by definition; it is not looping"
        );
    }

    /// The slot probe reads the flock clean-proof.sh takes, not the `.holder`
    /// file: a stale holder beside a free lock must not fake a running proof.
    #[test]
    fn proof_slot_probe_reads_the_flock_not_the_holder_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = dir.path().join("clean-proof.lock");
        std::fs::write(&lock, "").expect("create lock");
        assert!(!proof_slot_held_at(&lock), "an unlocked slot file is free");

        let held = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock)
            .expect("open lock");
        held.lock().expect("take the slot");
        assert!(proof_slot_held_at(&lock), "a held flock is a running proof");
        held.unlock().expect("release");
        assert!(!proof_slot_held_at(&lock), "released is free again");
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
                harness_session: None,
                pane_id: Some("block:abc".to_string()),
                location_hosted: false,
                live: true,
                registered: true,
            },
            DirPin {
                pin: "HostedPin".to_string(),
                harness: "grok".to_string(),
                harness_session: Some("sess-9".to_string()),
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
