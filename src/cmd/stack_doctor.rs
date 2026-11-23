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

fn job_state(label: &str) -> Option<String> {
    let uid = unsafe { libc_getuid() };
    let out = Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| line.trim().strip_prefix("state = "))
        .map(str::to_string)
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
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{label}")])
            .status();
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
    }
}

fn jobs_probe() -> (Probe, Vec<String>) {
    let mut failed = Vec::new();
    let mut detail = Vec::new();
    for label in job_labels() {
        match job_state(&label) {
            Some(state) if state == "running" => detail.push(format!("{label}=running")),
            Some(state) => {
                failed.push(label.clone());
                detail.push(format!("{label}={state}"))
            }
            None => {
                failed.push(label.clone());
                detail.push(format!("{label}=not loaded"))
            }
        }
    }
    let ok = failed.is_empty();
    let probe = Probe {
        name: "jobs",
        ok,
        detail: detail.join("; "),
        fix: "flywheel stack doctor --fix (re-register from plist)".into(),
    };
    (probe, failed)
}

fn toron_probe(cfg: &Config) -> Probe {
    let addr = toron_addr(cfg);
    let ok = toron_reachable(&addr);
    Probe {
        name: "toron",
        ok,
        detail: format!("{addr}: {}", if ok { "reachable" } else { "refused" }),
        fix: "flywheel stack install --operator; then check toron daemon logs".into(),
    }
}

pub(crate) fn cmd_stack_doctor(cfg: &Config, json: bool, fix: bool) -> Result<()> {
    let mut probes = Vec::new();
    probes.push(binaries_probe());
    let (jobs, failed_jobs) = jobs_probe();
    probes.push(jobs);
    probes.push(toron_probe(cfg));

    if fix {
        for label in &failed_jobs {
            match re_register(label) {
                Ok(()) => eprintln!("doctor: re-registered {label}"),
                Err(e) => eprintln!("doctor: could not repair {label}: {e}"),
            }
        }
        if !failed_jobs.is_empty() {
            probes.clear();
            probes.push(binaries_probe());
            let (jobs, _) = jobs_probe();
            probes.push(jobs);
            probes.push(toron_probe(cfg));
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
        }
    }
    if probes.iter().any(|p| !p.ok) {
        anyhow::bail!("stack doctor found a failing probe");
    }
    Ok(())
}
