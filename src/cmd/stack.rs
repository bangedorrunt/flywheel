//! `flywheel stack` — one operator door for the installed flywheel/toron
//! stack. `status` is a native read; `install` delegates to the audited
//! installer script so there is still exactly one install authority.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum StackCmd {
    /// Report installed binaries, live host generations, and toron build state.
    Status {
        /// Machine-readable `flywheel-stack-status.v1`.
        #[arg(long)]
        json: bool,
        /// Exit 1 when any check fails (missing/skewed binary, stale host, down health, replaced toron).
        #[arg(long)]
        check: bool,
    },
    /// Probe the installed stack end to end; `--fix` re-registers jobs.
    Doctor {
        /// Machine-readable `flywheel-stack-doctor.v1`.
        #[arg(long)]
        json: bool,
        /// Repair what can be repaired, then re-run the probes.
        #[arg(long)]
        fix: bool,
    },
    /// Reinstall flywheel, doorbell, and toron from source, then verify them.
    Install {
        /// Print the install/restart plan without changing the host.
        #[arg(long)]
        dry_run: bool,
        /// Skip ad-hoc codesign on macOS.
        #[arg(long)]
        no_codesign: bool,
        /// Build/install but do not restart services.
        #[arg(long)]
        no_restart: bool,
        /// Leave the installed toron binary and daemon alone.
        #[arg(long)]
        skip_toron: bool,
        /// Install as the operator: bypass the captain-only fleet guard.
        #[arg(long)]
        operator: bool,
    },
}

pub(crate) fn cmd_stack(cfg: &Config, cmd: &StackCmd) -> Result<()> {
    match cmd {
        StackCmd::Status { json, check } => stack_status(cfg, *json, *check),
        StackCmd::Doctor { json, fix } => {
            crate::cmd::stack_doctor::cmd_stack_doctor(cfg, *json, *fix)
        }
        StackCmd::Install {
            dry_run,
            no_codesign,
            no_restart,
            skip_toron,
            operator,
        } => {
            if !dry_run {
                crate::authority::guard_fleet_mutation(cfg, None, *operator, "stack install")?;
            }
            stack_install(*dry_run, *no_codesign, *no_restart, *skip_toron)
        }
    }
}

struct BinaryProbe {
    name: &'static str,
    path: Option<PathBuf>,
    version: Option<String>,
    error: Option<String>,
}

pub(crate) fn bin_dir() -> PathBuf {
    if let Some(root) = std::env::var_os("CARGO_INSTALL_ROOT") {
        return PathBuf::from(root).join("bin");
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(home).join("bin");
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".cargo")
        .join("bin")
}

fn find_bin(name: &str) -> Option<PathBuf> {
    let installed = bin_dir().join(name);
    if installed.is_file() {
        return Some(installed);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn probe_bin(name: &'static str) -> BinaryProbe {
    let Some(path) = find_bin(name) else {
        return BinaryProbe {
            name,
            path: None,
            version: None,
            error: Some("not found in the cargo bin directory or PATH".into()),
        };
    };
    match Command::new(&path).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string);
            let error = version
                .is_none()
                .then(|| "version probe printed nothing".into());
            BinaryProbe {
                name,
                path: Some(path),
                version,
                error,
            }
        }
        Ok(out) => BinaryProbe {
            name,
            path: Some(path),
            version: None,
            error: Some(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("version probe failed")
                    .to_string(),
            ),
        },
        Err(err) => BinaryProbe {
            name,
            path: Some(path),
            version: None,
            error: Some(err.to_string()),
        },
    }
}

/// The generation hash is SHA-1 over the first 8 KiB of the flywheel binary
/// (`doorbell::binary_hash`); only live hosts with a different non-empty hash
/// need replacement.
fn stale_host_pids(health: &Value, installed_hash: &str) -> Vec<u64> {
    health
        .get("hosts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|host| host.get("state").and_then(Value::as_str) == Some("live"))
        .filter(|host| {
            host.get("generation")
                .and_then(|generation| generation.get("binary_hash"))
                .and_then(Value::as_str)
                .is_some_and(|hash| !hash.is_empty() && hash != installed_hash)
        })
        .filter_map(|host| host.get("pid").and_then(Value::as_u64))
        .filter(|pid| *pid > 1)
        .collect()
}

fn is_live_pid(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Doorbell health-check target (ADR-0020): explicit `DOORBELL_URL` only.
/// `None` skips the probe (embedded/supervise path).
fn doorbell_url() -> Option<String> {
    std::env::var("DOORBELL_URL").ok().filter(|s| !s.is_empty())
}

fn toron_status() -> (Option<Value>, Option<String>) {
    let mut command = match find_bin("toron") {
        Some(path) => Command::new(path),
        None => Command::new("toron"),
    };
    match command.args(["daemon", "status", "--json"]).output() {
        Ok(out) if out.status.success() => match serde_json::from_slice(&out.stdout) {
            Ok(value) => (Some(value), None),
            Err(err) => (None, Some(format!("toron status was not JSON: {err}"))),
        },
        Ok(out) => (
            None,
            Some(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("toron daemon status failed")
                    .to_string(),
            ),
        ),
        Err(err) => (None, Some(format!("toron not found: {err}"))),
    }
}

/// bd-4bxtl: the address flywheel actually routes to. `FLYWHEEL_TORON_URL`
/// wins (the documented override), then `[mail.toron] url`, then toron's
/// default. A build record is not health — probe the address itself.
fn toron_addr_from_url(url: &str) -> Option<String> {
    let (without_scheme, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443)
    } else {
        (url.strip_prefix("http://")?, 18080)
    };
    let authority = without_scheme.split('/').next().unwrap_or("");
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(authority);
    if hostport.is_empty() {
        return None;
    }
    if hostport.contains(':') {
        Some(hostport.to_string())
    } else {
        Some(format!("{hostport}:{default_port}"))
    }
}

pub(crate) fn toron_addr(cfg: &Config) -> String {
    std::env::var("FLYWHEEL_TORON_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.toron_mail_config().url.clone())
        .and_then(|url| toron_addr_from_url(&url))
        .unwrap_or_else(|| "127.0.0.1:18080".to_string())
}

pub(crate) fn toron_reachable(addr: &str) -> bool {
    let Ok(addrs) = addr.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(500)).is_ok())
}

/// bd-9zw9u: an invalid logrotate side plist survives an install that never
/// calls `toron daemon install`, so the native status door must report it too.
fn logrotate_plist_invalid_at(path: &Path) -> bool {
    if !cfg!(target_os = "macos") || !path.is_file() {
        return false;
    }
    Command::new("plutil")
        .args(["-lint", path.to_str().unwrap_or_default()])
        .output()
        .map(|out| !out.status.success())
        .unwrap_or(false)
}

fn logrotate_plist_invalid() -> bool {
    dirs::home_dir()
        .map(|home| home.join("Library/LaunchAgents/com.toron.logrotate.plist"))
        .is_some_and(|path| logrotate_plist_invalid_at(&path))
}

fn launchd_spawn_failed(print_output: &str) -> bool {
    print_output
        .lines()
        .any(|line| line.trim() == "job state = spawn failed")
}

fn flywheel_job_labels<'a>(plist_names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut labels: Vec<String> = plist_names
        .filter_map(|name| name.strip_suffix(".plist"))
        .filter(|label| label.starts_with("dev.flywheel."))
        .map(String::from)
        .collect();
    labels.sort();
    labels
}

/// bd-i4edn: flywheel launchd jobs whose binary launchd refuses to spawn.
/// Reads only jobs with a plist under HOME, so a sandboxed HOME never shells
/// out to launchctl.
fn flywheel_jobs_unable_to_spawn() -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    let Some(agents) = dirs::home_dir().map(|home| home.join("Library/LaunchAgents")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&agents) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    let labels = flywheel_job_labels(names.iter().map(String::as_str));
    if labels.is_empty() {
        return Vec::new();
    }
    let Some(uid) = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|uid| !uid.is_empty())
    else {
        return Vec::new();
    };
    labels
        .into_iter()
        .filter(|label| {
            Command::new("launchctl")
                .args(["print", &format!("gui/{uid}/{label}")])
                .output()
                .map(|out| launchd_spawn_failed(&String::from_utf8_lossy(&out.stdout)))
                .unwrap_or(false)
        })
        .collect()
}

fn binary_json(binary: &BinaryProbe) -> Value {
    json!({
        "name": binary.name,
        "path": binary.path.as_ref().map(|path| path.display().to_string()),
        "version": binary.version,
        "error": binary.error,
    })
}

/// bd-rrlo2: seats with an events door but no token file, as
/// (project label, pin, harness). Pure over rows + the tokened pin set so
/// the scan stays testable; the live wiring below feeds both halves.
/// Rows for harnesses without an events door never count as tokenless —
/// they ride the doorbell by design.
fn tokenless_events_seats(
    rows: &[(String, String, String)],
    token_pins: &std::collections::HashSet<String>,
) -> Vec<(String, String, String)> {
    rows.iter()
        .filter(|(_, pin, harness)| {
            !token_pins.contains(pin) && crate::cmd::spawn::has_events_door(harness)
        })
        .cloned()
        .collect()
}

/// bd-rrlo2: every registered events-door seat (project label, pin,
/// harness) across all project registries, plus every pin with a token
/// file on disk. A seat is tokenless when its pin names no token file —
/// matched by pin, never by project slug, so registry-key spellings
/// cannot manufacture a false missing.
fn events_seats_and_tokens() -> (
    Vec<(String, String, String)>,
    std::collections::HashSet<String>,
) {
    let mut rows = Vec::new();
    let dir = crate::state::agents_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(seats) = serde_json::from_str::<Vec<crate::state::SpawnEntry>>(&raw) else {
                continue;
            };
            let project = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for seat in seats {
                let (Some(pin), Some(harness)) = (seat.mail_identity.clone(), seat.harness.clone())
                else {
                    continue;
                };
                rows.push((project.clone(), pin, harness));
            }
        }
    }
    let mut token_pins = std::collections::HashSet::new();
    let token_dir = crate::config::state_dir().join("events-tokens");
    if let Ok(entries) = std::fs::read_dir(&token_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".token") else {
                continue;
            };
            if let Some((_, pin)) = stem.rsplit_once('-') {
                token_pins.insert(pin.to_string());
            }
        }
    }
    (rows, token_pins)
}

fn stack_status(cfg: &Config, json_output: bool, check: bool) -> Result<()> {
    let toron_daemon_addr = toron_addr(cfg);
    let toron_reachable_now = toron_reachable(&toron_daemon_addr);
    let binaries = [
        probe_bin("flywheel"),
        probe_bin("doorbell"),
        probe_bin("toron"),
        // bd-t2z0b: report the pinned omp (pi-fork) build. Seats launch from the
        // project dir, whose mise.toml pins it, so the shim resolves it here.
        probe_bin("omp"),
    ];
    let installed_hash = binaries[0]
        .path
        .as_deref()
        .and_then(crate::doorbell::binary_hash);
    let health_result = doorbell_url()
        .as_deref()
        .map(crate::doorbell::daemon_health_probe);
    let health = health_result
        .as_ref()
        .and_then(|r| r.as_ref().ok().cloned());
    let health_error = health_result
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|failure| failure.reason().to_string());
    let health_version = health
        .as_ref()
        .and_then(|value| value.pointer("/build/version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let installed_doorbell_version = binaries[1]
        .version
        .as_deref()
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or("");
    let installed_flywheel_version = binaries[0]
        .version
        .as_deref()
        .and_then(|line| line.split_whitespace().last())
        .unwrap_or("");
    let version_skew = !health_version.is_empty()
        && !installed_doorbell_version.is_empty()
        && health_version != installed_doorbell_version;
    let flywheel_version_skew = !installed_flywheel_version.is_empty()
        && installed_flywheel_version != env!("CARGO_PKG_VERSION");
    let stale_pids: Vec<u64> = health
        .as_ref()
        .zip(installed_hash.as_deref())
        .map(|(health, hash)| {
            stale_host_pids(health, hash)
                .into_iter()
                .filter(|pid| is_live_pid(*pid))
                .collect()
        })
        .unwrap_or_default();
    let (toron, toron_error) = toron_status();
    // bd-rrlo2: report events-door seats without a token file. Display
    // only: a missing token degrades a seat to the doorbell fallback, it
    // never fails the stack.
    let (seat_rows, token_pins) = events_seats_and_tokens();
    let tokenless = tokenless_events_seats(&seat_rows, &token_pins);
    let toron_restart_required = toron
        .as_ref()
        .and_then(|value| value.pointer("/build/restart_required"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let unspawnable_jobs = flywheel_jobs_unable_to_spawn();
    let required_binaries_bad = binaries[0].error.is_some() || binaries[2].error.is_some();
    let bad = required_binaries_bad
        || !unspawnable_jobs.is_empty()
        || version_skew
        || flywheel_version_skew
        || !stale_pids.is_empty()
        || toron.is_none()
        || toron_restart_required
        || !toron_reachable_now
        || logrotate_plist_invalid();

    if json_output {
        print_json(&json!({
            "schema": "flywheel-stack-status.v1",
            "ok": !bad,
            "binaries": binaries.iter().map(binary_json).collect::<Vec<_>>(),
            "build": {
                "source_version": env!("CARGO_PKG_VERSION"),
                "installed_version": installed_flywheel_version,
                "version_skew": flywheel_version_skew,
            },
            "doorbell": {
                "url": doorbell_url(),
                "health": health,
                "error": health_error,
                "version_skew": version_skew.then_some({
                    json!({"health": health_version, "installed": installed_doorbell_version})
                }),
                "stale_live_pids": stale_pids,
            },
            "toron": {
                "addr": toron_daemon_addr,
                "reachable": toron_reachable_now,
                "status": toron,
                "error": toron_error,
                "restart_required": toron_restart_required,
            },
            "launchd": { "unable_to_spawn": unspawnable_jobs },
            "events_tokens": {
                "missing": tokenless
                    .iter()
                    .map(|(project, pin, harness)| {
                        json!({"project": project, "pin": pin, "harness": harness})
                    })
                    .collect::<Vec<_>>(),
            },
        }));
    } else {
        println!("== binaries ==");
        for binary in &binaries {
            match (&binary.path, &binary.version, &binary.error) {
                (Some(path), Some(version), _) => {
                    println!("{:<9} {version}", binary.name);
                    println!("          {}", path.display());
                }
                (path, _, error) => {
                    println!(
                        "{:<9} unavailable{}",
                        binary.name,
                        path.as_ref()
                            .map(|p| format!(" at {}", p.display()))
                            .unwrap_or_default()
                    );
                    if let Some(error) = error {
                        println!("          {error}");
                    }
                }
            }
        }
        if flywheel_version_skew {
            println!(
                "flywheel version skew: installed {installed_flywheel_version}, source {}",
                env!("CARGO_PKG_VERSION")
            );
        }

        println!("\n== doorbell ==");
        match &health {
            Some(health) => {
                println!(
                    "version {}, mode {}",
                    health
                        .pointer("/build/version")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    health.get("mode").and_then(Value::as_str).unwrap_or("?")
                );
                if version_skew {
                    println!(
                        "version skew: /health {health_version}, installed {installed_doorbell_version}"
                    );
                }
                if let Some(hosts) = health.get("hosts").and_then(Value::as_array)
                    && !hosts.is_empty()
                {
                    println!("hosts:");
                    for host in hosts {
                        println!(
                            "  {}\t{}\tpid={}\tgen={}\tage={}s",
                            host.get("workspace").and_then(Value::as_str).unwrap_or("?"),
                            host.get("state").and_then(Value::as_str).unwrap_or("?"),
                            host.get("pid").and_then(Value::as_u64).unwrap_or(0),
                            host.pointer("/generation/binary_hash")
                                .and_then(Value::as_str)
                                .map(|hash| &hash[..hash.len().min(12)])
                                .unwrap_or("?"),
                            host.get("last_seen_age_s")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        );
                    }
                } else {
                    println!("hosts: none");
                }
                if stale_pids.is_empty() {
                    println!("stale live hosts: none");
                } else {
                    println!(
                        "stale live hosts: {} (pids {})",
                        stale_pids.len(),
                        stale_pids
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            None => println!(
                "consolidated into toron daemon on :18080 (port :18980 retired per ADR-0020)"
            ),
        }

        println!("\n== toron ==");
        if !toron_reachable_now {
            println!("daemon down: {toron_daemon_addr} (connection refused)");
        }
        match &toron {
            Some(status) => {
                println!(
                    "daemon: {}",
                    status
                        .pointer("/build/verdict")
                        .and_then(Value::as_str)
                        .unwrap_or(if toron_restart_required {
                            "replaced"
                        } else {
                            "current"
                        })
                );
                if toron_restart_required {
                    println!("restart required: true (a replaced build is still running)");
                }
            }
            None => println!(
                "status unavailable: {}",
                toron_error.as_deref().unwrap_or("unknown failure")
            ),
        }

        println!("\n== events tokens ==");
        if tokenless.is_empty() {
            println!("all events-door seats have a token");
        } else {
            println!("seats without a token (doorbell fallback):");
            for (project, pin, harness) in &tokenless {
                println!("  {project} {pin} ({harness})");
            }
        }

        if !unspawnable_jobs.is_empty() {
            println!("\n== launchd ==");
            for label in &unspawnable_jobs {
                println!("{label}: spawn failed (re-register: launchctl bootout + bootstrap)");
            }
        }

        println!("\nstack: {}", if bad { "FAILED" } else { "ok" });
    }

    if check && bad {
        if let Some(label) = unspawnable_jobs.first() {
            bail!("stack is not current: launchd cannot spawn {label}");
        }
        if !toron_reachable_now {
            bail!("stack is not current: toron daemon not listening at {toron_daemon_addr}");
        }
        bail!("stack is not current");
    }
    Ok(())
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn stack_install(
    dry_run: bool,
    no_codesign: bool,
    no_restart: bool,
    skip_toron: bool,
) -> Result<()> {
    let source = std::env::var("FLYWHEEL_SOURCE").unwrap_or_else(|_| "~/workspace/flywheel".into());
    let source = expand_tilde(&source);
    let script = source.join("scripts").join("stack.sh");
    if !script.is_file() {
        bail!(
            "stack installer not found at {} — set FLYWHEEL_SOURCE to the flywheel repo",
            script.display()
        );
    }
    let mut args = vec!["install".to_string()];
    // The native door already passed the guard, so the script's own check (for
    // a direct `scripts/stack.sh install` call) is told the authority is proven.
    args.push("--operator".into());
    if dry_run {
        args.push("--dry-run".into());
    }
    if no_codesign {
        args.push("--no-codesign".into());
    }
    if no_restart {
        args.push("--no-restart".into());
    }
    if skip_toron {
        args.push("--skip-toron".into());
    }
    let status = Command::new("bash")
        .arg(&script)
        .args(&args)
        .status()
        .map_err(|err| anyhow!("could not run {}: {err}", script.display()))?;
    if !status.success() {
        bail!("stack install failed");
    }
    // A fleet-wide binary change is a digest fact: every other pane is now
    // running different code than it booted with (bd-0uzrq.6).
    broadcast_install();
    Ok(())
}

/// Queue the install notice where the digest reads it. Best effort: a missing
/// project must not fail an install that already replaced the binaries.
fn broadcast_install() {
    let cfg = crate::config::Config::default();
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Some(project) = crate::cron::detect_project_in(&cfg, &cwd, None) else {
        return;
    };
    let state_dir = crate::config::state_dir();
    let digest = format!(
        "stack install: {} replaced at {}",
        crate::cmd::stack::installed_flywheel_version(),
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    crate::doorbell::service_emit_wake_in(&state_dir, &project, "stack-install", "captain");
    let _ = crate::append_pending_fact(&state_dir, &project, &digest);
}

/// The installed flywheel version, or "unknown" when the binary is not found.
pub(crate) fn installed_flywheel_version() -> String {
    find_bin("flywheel")
        .and_then(|path| {
            std::process::Command::new(path)
                .arg("--version")
                .output()
                .ok()
        })
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// bd-i4edn: a job launchd cannot spawn (new binary signature after an
    /// install) must fail the check, not read as ok.
    #[test]
    fn a_job_launchd_cannot_spawn_is_a_failure() {
        let stuck = "\tstate = spawn scheduled\n\tprogram = /x/flywheel\n\tlast exit code = 78: EX_CONFIG\n\tjob state = spawn failed\n\tproperties = runatload | needs LWCR update\n";
        let running = "\tstate = running\n\tpid = 50358\n\tjob state = running\n";
        assert!(super::launchd_spawn_failed(stuck));
        assert!(!super::launchd_spawn_failed(running));
    }

    #[test]
    fn flywheel_job_labels_come_from_their_plists() {
        let names = [
            "dev.flywheel.daemon.plist",
            "dev.flywheel.doorbell.flywheel.plist",
            "com.toron.serve.plist",
            "dev.flywheel.cron.plist.bak",
        ];
        assert_eq!(
            super::flywheel_job_labels(names.iter().copied()),
            vec!["dev.flywheel.daemon", "dev.flywheel.doorbell.flywheel"]
        );
    }

    #[test]
    fn invalid_logrotate_plist_is_detected() {
        let root = std::env::temp_dir().join(format!("fw-logrotate-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("com.toron.logrotate.plist");
        std::fs::write(&path, "<string>cp a && b</string>").unwrap();
        assert!(super::logrotate_plist_invalid_at(&path));
        std::fs::write(
            &path,
            r#"<?xml version="1.0"?><plist version="1.0"><dict/></plist>"#,
        )
        .unwrap();
        assert!(!super::logrotate_plist_invalid_at(&path));
        std::fs::remove_dir_all(&root).ok();
    }

    /// bd-4bxtl: health is the address the daemon answers on, not a build
    /// record. A stale build record alone must not make `stack: ok`.
    #[test]
    fn toron_addr_comes_from_the_url() {
        assert_eq!(
            super::toron_addr_from_url("http://127.0.0.1:18080/mcp"),
            Some("127.0.0.1:18080".to_string())
        );
        assert_eq!(
            super::toron_addr_from_url("http://user:pw@127.0.0.1:9/"),
            Some("127.0.0.1:9".to_string())
        );
        assert_eq!(
            super::toron_addr_from_url("http://toron.local"),
            Some("toron.local:18080".to_string())
        );
        assert_eq!(super::toron_addr_from_url("not a url/"), None);
    }

    #[test]
    fn toron_reachability_fails_closed_on_a_refusing_address() {
        assert!(
            !super::toron_reachable("127.0.0.1:1"),
            "closed port is down"
        );
        assert!(
            !super::toron_reachable("not-an-address"),
            "bad address is down"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(super::toron_reachable(&addr), "a listening daemon is up");
    }

    #[test]
    fn stale_hosts_are_only_live_mismatched_pids() {
        let health = json!({
            "hosts": [
                {"workspace": "current", "state": "live", "pid": 101, "generation": {"binary_hash": "new"}},
                {"workspace": "old-live", "state": "live", "pid": 102, "generation": {"binary_hash": "old"}},
                {"workspace": "old-exited", "state": "exited", "pid": 103, "generation": {"binary_hash": "old"}},
                {"workspace": "no-gen", "state": "live", "pid": 104, "generation": null},
                {"workspace": "no-pid", "state": "live", "pid": null, "generation": {"binary_hash": "old"}}
            ]
        });
        assert_eq!(super::stale_host_pids(&health, "new"), vec![102]);
    }

    #[test]
    fn stale_hosts_ignore_empty_generation() {
        let health = json!({
            "hosts": [
                {"state": "live", "pid": 7, "generation": {"binary_hash": ""}},
                {"state": "live", "pid": 8, "generation": {"binary_hash": null}}
            ]
        });
        assert!(super::stale_host_pids(&health, "new").is_empty());
    }

    /// bd-rrlo2: tokenless reporting is a pin-set difference — a seat
    /// counts as tokened when its pin names any token file, whatever the
    /// registry-key spelling of its project.
    #[test]
    fn tokenless_seats_match_by_pin_not_project() {
        let rows = vec![
            (
                "flywheel".to_string(),
                "PiPin".to_string(),
                "pi".to_string(),
            ),
            (
                "other".to_string(),
                "GemPin".to_string(),
                "gemini".to_string(),
            ),
            (
                "flywheel".to_string(),
                "ClaudePin".to_string(),
                "claude".to_string(),
            ),
        ];
        let tokened: std::collections::HashSet<String> =
            ["PiPin".to_string()].into_iter().collect();
        assert_eq!(
            super::tokenless_events_seats(&rows, &tokened),
            vec![(
                "other".to_string(),
                "GemPin".to_string(),
                "gemini".to_string()
            )],
            "only the tokenless events-door seat is reported"
        );
        assert!(
            super::tokenless_events_seats(&[], &tokened).is_empty(),
            "no seats, no report"
        );
    }
}
