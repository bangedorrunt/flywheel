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

use crate::config::Config;
use crate::mail_shim::MailInboxFilter;
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
pub(crate) fn first_failing_check(
    checks: &[preflight::Check],
) -> Option<(&'static str, String)> {
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
fn inbox_rows(mail: &crate::mail_shim::Mail, key: &str, since: Option<&str>) -> Result<Vec<Value>> {
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
            Check::pass("herdr-socket"),
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
            first_failing_check(&[Check::pass("a"), Check::fail("b", "boom")])
                .map(|(n, _)| n),
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
}
