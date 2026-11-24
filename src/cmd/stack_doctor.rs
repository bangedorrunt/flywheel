//! `flywheel stack doctor` (bd-rgs02): end-to-end probes of the installed
//! stack. Every PASS means the check actually looked at the host — no cached
//! build record is treated as health. `--fix` re-registers jobs from their
//! plists, the one repair that is safe without the operator's install door.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use super::stack::{bin_dir, toron_addr, toron_reachable};
use crate::config::Config;
use crate::output::print_json;

#[derive(Clone, Serialize)]
pub(crate) struct Probe {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub fix: String,
    /// Non-fatal observations (e.g. stale state dirs from dead sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn: Option<String>,
}

const CORE_JOBS: &[&str] = &[
    "dev.flywheel.daemon",
    "dev.flywheel.cron",
    "com.toron.serve",
    "com.toron.logrotate",
];

fn launch_agent_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/LaunchAgents")
}

fn job_labels() -> Vec<String> {
    let mut labels: Vec<String> = CORE_JOBS.iter().map(|s| (*s).to_string()).collect();
    if let Ok(out) = Command::new("launchctl").arg("list").output()
        && out.status.success()
    {
        for word in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            if word.starts_with("dev.flywheel.doorbell.") && !labels.iter().any(|l| l == word) {
                labels.push(word.to_string());
            }
        }
    }
    labels
}

/// (loaded, state, last exit code, interval job). An interval job is healthy
/// when loaded: launchd leaves it idle between runs, so "not running" is not
/// a failure. Only a spawn failure or a nonzero last exit is bad.
fn job_health(label: &str) -> (bool, String, i32, bool) {
    let uid = unsafe { libc_getuid() };
    let Ok(out) = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
    else {
        return (false, "not loaded".into(), 0, false);
    };
    if !out.status.success() {
        return (false, "not loaded".into(), 0, false);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let state = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .unwrap_or("unknown")
        .trim()
        .to_string();
    let last_exit = text
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("last exit code = ")
                .and_then(|v| v.trim().parse::<i32>().ok())
        })
        .unwrap_or(0);
    let plist = launch_agent_dir().join(format!("{label}.plist"));
    let interval = std::fs::read_to_string(&plist)
        .map(|body| body.contains("StartInterval") || body.contains("StartCalendarInterval"))
        .unwrap_or(false);
    (true, state, last_exit, interval)
}

/// Workspace ids rex reports as live now. Dead sessions leave doorbell state
/// dirs and seat registry rows behind; they must not fail doctor.
fn live_workspaces(cfg: &Config) -> std::collections::BTreeSet<String> {
    crate::backend::multiplexer_backend(cfg)
        .ok()
        .and_then(|b| b.workspace_list().ok())
        .map(|rows| rows.into_iter().map(|w| w.workspace_id).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
unsafe fn libc_getuid() -> u32 {
    // No libc dependency: `/usr/bin/id -u` is available on every host we
    // support and is what scripts/stack.sh uses too.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(not(unix))]
unsafe fn libc_getuid() -> u32 {
    0
}

fn re_register(label: &str) -> std::result::Result<(), String> {
    let uid = unsafe { libc_getuid() };
    let plist = launch_agent_dir().join(format!("{label}.plist"));
    if plist.is_file() {
        let domain = format!("gui/{uid}/{label}");
        let _ = Command::new("launchctl")
            .args(["bootout", &domain])
            .status();
        // Same settle window as scripts/stack.sh: bootstrap immediately after
        // bootout returns launchd I/O error 5. Wait for the job to leave the
        // plane, bounded so --fix can never hang.
        for _ in 0..10 {
            let present = Command::new("launchctl")
                .args(["print", &domain])
                .output()
                .is_ok_and(|o| o.status.success());
            if !present {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let out = Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                plist.to_str().unwrap_or(""),
            ])
            .status()
            .map_err(|e| e.to_string())?;
        if !out.success() {
            return Err(format!("bootstrap {label} failed"));
        }
        Ok(())
    } else {
        Err(format!("no plist for {label}; cannot repair safely"))
    }
}

fn binaries_probe() -> Probe {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for name in ["flywheel", "toron"] {
        let path = bin_dir().join(name);
        let out = if path.is_file() {
            Command::new(&path).arg("--version").output().ok()
        } else {
            None
        };
        match out.filter(|o| o.status.success()) {
            Some(o) => found.push(format!(
                "{name}: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            )),
            None => missing.push(name.to_string()),
        }
    }
    let ok = missing.is_empty();
    Probe {
        name: "binaries",
        ok,
        detail: if ok {
            found.join("; ")
        } else {
            format!("missing or unversioned: {}", missing.join(", "))
        },
        fix: "flywheel stack install --operator".into(),
        warn: None,
    }
}

fn jobs_probe() -> (Probe, Vec<String>) {
    let mut failed = Vec::new();
    let mut detail = Vec::new();
    for label in job_labels() {
        let (loaded, state, last_exit, interval) = job_health(&label);
        let healthy =
            loaded && state != "spawn failed" && last_exit == 0 && (interval || state == "running");
        detail.push(format!(
            "{label}={state}{}{}",
            if last_exit == 0 {
                String::new()
            } else {
                format!(" exit={last_exit}")
            },
            if interval { " (interval)" } else { "" }
        ));
        if !healthy {
            failed.push(label.clone());
        }
    }
    let ok = failed.is_empty();
    let probe = Probe {
        name: "jobs",
        ok,
        detail: detail.join("; "),
        fix: "flywheel stack doctor --fix (re-register from plist)".into(),
        warn: None,
    };
    (probe, failed)
}

fn doorbell_probe(cfg: &Config) -> Probe {
    let state_dir = crate::config::state_dir();
    let live = live_workspaces(cfg);
    let mut detail = Vec::new();
    let mut stale = Vec::new();
    let mut down = Vec::new();
    if let Ok(entries) = std::fs::read_dir(state_dir.join("doorbell")) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
            let Some(workspace) = name.strip_suffix(".state.json").map(str::to_string) else {
                continue;
            };
            if !live.contains(&workspace) {
                stale.push(workspace);
                continue;
            }
            let state = crate::doorbell::dispatcher_token(&state_dir, &workspace);
            detail.push(format!("{workspace}={state}"));
            if state != "up" {
                down.push(workspace);
            }
        }
    }
    if detail.is_empty() {
        detail.push(format!(
            "{} live session(s) without a dispatcher",
            live.len()
        ));
    }
    let warn = (!stale.is_empty()).then(|| {
        format!(
            "{} stale doorbell state dirs ({}); clean with: remove <state>/doorbell/<session>",
            stale.len(),
            stale.join(", ")
        )
    });
    Probe {
        name: "doorbell",
        ok: down.is_empty(),
        detail: detail.join("; "),
        fix: "flywheel doorbell start <project> (or stack doctor --fix)".into(),
        warn,
    }
}

fn daemon_port_probe() -> Probe {
    let port = std::env::var("FLYWHEEL_SERVE_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(crate::daemon::SERVE_PORT_DEFAULT);
    let addr = format!("127.0.0.1:{port}");
    let listening = toron_reachable(&addr);
    let holder = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
        });
    let command = holder.as_deref().and_then(|pid| {
        Command::new("ps")
            .args(["-p", pid, "-o", "comm="])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
    });
    let ours = command.as_deref().is_some_and(|c| c.contains("flywheel"));
    let ok = listening && ours;
    let detail = match (listening, &holder, &command) {
        (false, _, _) => format!("{addr}: no listener"),
        (true, Some(pid), Some(comm)) => format!("{addr} held by {comm} (pid {pid})"),
        (true, _, _) => format!("{addr}: listening, holder unknown"),
    };
    Probe {
        name: "daemon_port",
        ok,
        detail: detail.clone(),
        fix: if ok {
            String::new()
        } else {
            format!(
                "stop the conflicting process ({}) and run flywheel stack install --operator",
                command.as_deref().unwrap_or("unknown")
            )
        },
        warn: None,
    }
}

fn wake_probe() -> Probe {
    let state_dir = crate::config::state_dir();
    let project = "flywheel-doctor";
    let pin = "DoctorProbe";
    let summary = format!("emit stack-doctor -> {pin}");
    // Use the same wake path the stack itself uses, then verify a wake row
    // was actually queued for the probe pin — not that a status flag was set.
    crate::doorbell::service_emit_wake_in(&state_dir, project, "stack-doctor", pin);
    let path = state_dir.join("facts").join(format!(
        "{}-pending.jsonl",
        crate::state::state_key(project)
    ));
    let ok = std::fs::read_to_string(&path)
        .map(|body| body.lines().any(|line| line.contains(&summary)))
        .unwrap_or(false);
    Probe {
        name: "wake",
        ok,
        detail: format!(
            "{}: {}",
            path.display(),
            if ok { "wake queued" } else { "no wake row" }
        ),
        fix: "flywheel stack install --operator; check the doorbell dispatcher".into(),
        warn: None,
    }
}

fn events_probe(cfg: &Config) -> Probe {
    let live = live_workspaces(cfg);
    let (all_seats, tokens) = super::stack::events_seats_and_tokens();
    // Dead sessions leave seat rows behind; only live workspaces can carry a
    // working events door.
    let seats: Vec<_> = all_seats
        .iter()
        .filter(|(project, _, _)| live.contains(project))
        .cloned()
        .collect();
    let stale: Vec<String> = all_seats
        .iter()
        .filter(|(project, _, _)| !live.contains(project))
        .map(|(project, pin, _)| format!("{project}/{pin}"))
        .collect();
    let tokenless = super::stack::tokenless_events_seats(&seats, &tokens);
    let ok = tokenless.is_empty();
    Probe {
        name: "events",
        ok,
        detail: if ok {
            format!("{} live events-door seats armed", seats.len())
        } else {
            format!(
                "tokenless live seats: {}",
                tokenless
                    .iter()
                    .map(|(p, pin, _)| format!("{p}/{pin}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
        fix: "flywheel stack install --operator (mints seat event tokens)".into(),
        warn: (!stale.is_empty()).then(|| {
            format!(
                "{} seat rows from dead sessions ignored ({}); clean with: remove <state>/doorbell/<session>",
                stale.len(),
                stale.join(", ")
            )
        }),
    }
}

fn toron_probe(cfg: &Config) -> Probe {
    let addr = toron_addr(cfg);
    let ok = toron_reachable(&addr);
    Probe {
        name: "toron",
        ok,
        detail: format!("{addr}: {}", if ok { "reachable" } else { "refused" }),
        fix: "flywheel stack install --operator; then check toron daemon logs".into(),
        warn: None,
    }
}

/// A job is not healthy the instant launchd accepts its bootstrap: the daemon
/// still has to bind its port. Poll with a deadline so --fix never hangs and
/// never reports a FAIL purely because it probed too early.
fn wait_for_port(addr: &str, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if toron_reachable(addr) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub(crate) fn cmd_stack_doctor(cfg: &Config, json: bool, fix: bool) -> Result<()> {
    let mut probes = Vec::new();
    probes.push(binaries_probe());
    let (jobs, failed_jobs) = jobs_probe();
    probes.push(jobs);
    probes.push(toron_probe(cfg));
    probes.push(doorbell_probe(cfg));
    probes.push(events_probe(cfg));
    probes.push(wake_probe());
    probes.push(daemon_port_probe());

    if fix {
        for label in &failed_jobs {
            match re_register(label) {
                Ok(()) => eprintln!("doctor: re-registered {label}"),
                Err(e) => eprintln!("doctor: could not repair {label}: {e}"),
            }
        }
        if !failed_jobs.is_empty() {
            // Only now, after bootout+bootstrap, give the daemons a bounded
            // window to bind their ports before re-probing.
            let serve_port = std::env::var("FLYWHEEL_SERVE_PORT")
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(crate::daemon::SERVE_PORT_DEFAULT);
            wait_for_port(&toron_addr(cfg), 10);
            wait_for_port(&format!("127.0.0.1:{serve_port}"), 10);
            probes.clear();
            probes.push(binaries_probe());
            let (jobs, _) = jobs_probe();
            probes.push(jobs);
            probes.push(toron_probe(cfg));
            probes.push(doorbell_probe(cfg));
            probes.push(events_probe(cfg));
            probes.push(wake_probe());
            probes.push(daemon_port_probe());
        }
    }

    if json {
        print_json(&serde_json::json!({
            "kind": "flywheel-stack-doctor.v1",
            "ok": probes.iter().all(|p| p.ok),
            "probes": probes,
        }));
    } else {
        for probe in &probes {
            println!(
                "{} {} — {}{}",
                if probe.ok { "PASS" } else { "FAIL" },
                probe.name,
                probe.detail,
                if probe.ok {
                    String::new()
                } else {
                    format!(" (fix: {})", probe.fix)
                }
            );
            if let Some(warn) = &probe.warn {
                println!("WARN {} — {}", probe.name, warn);
            }
        }
    }
    if probes.iter().any(|p| !p.ok) {
        anyhow::bail!("stack doctor found a failing probe");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_port_polls_until_a_listener_binds() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        drop(listener);
        let held = addr.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _late = std::net::TcpListener::bind(&held).expect("late bind");
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        assert!(wait_for_port(&addr, 3), "a late listener must be awaited");
        thread.join().expect("late listener thread");
        assert!(
            !wait_for_port("127.0.0.1:1", 1),
            "a refused port must time out false"
        );
    }
}
