//! `flywheel stack` — one operator door for the installed flywheel/toron
//! stack. `status` is a native read; `install` delegates to the audited
//! installer script so there is still exactly one install authority.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

const DEFAULT_DOORBELL_URL: &str = "http://127.0.0.1:18980";

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
    },
}

pub(crate) fn cmd_stack(_cfg: &Config, cmd: &StackCmd) -> Result<()> {
    match cmd {
        StackCmd::Status { json, check } => stack_status(*json, *check),
        StackCmd::Install {
            dry_run,
            no_codesign,
            no_restart,
            skip_toron,
        } => stack_install(*dry_run, *no_codesign, *no_restart, *skip_toron),
    }
}

struct BinaryProbe {
    name: &'static str,
    path: Option<PathBuf>,
    version: Option<String>,
    error: Option<String>,
}

fn bin_dir() -> PathBuf {
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

fn doorbell_url() -> String {
    std::env::var("DOORBELL_URL").unwrap_or_else(|_| DEFAULT_DOORBELL_URL.into())
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

fn binary_json(binary: &BinaryProbe) -> Value {
    json!({
        "name": binary.name,
        "path": binary.path.as_ref().map(|path| path.display().to_string()),
        "version": binary.version,
        "error": binary.error,
    })
}

fn stack_status(json_output: bool, check: bool) -> Result<()> {
    let binaries = [
        probe_bin("flywheel"),
        probe_bin("doorbell"),
        probe_bin("toron"),
    ];
    let installed_hash = binaries[0]
        .path
        .as_deref()
        .and_then(crate::doorbell::binary_hash);
    let health_result = crate::doorbell::daemon_health_probe(&doorbell_url());
    let health = health_result.as_ref().ok().cloned();
    let health_error = health_result
        .as_ref()
        .err()
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
    let stale_pids = health
        .as_ref()
        .zip(installed_hash.as_deref())
        .map(|(health, hash)| stale_host_pids(health, hash))
        .unwrap_or_default();
    let (toron, toron_error) = toron_status();
    let toron_restart_required = toron
        .as_ref()
        .and_then(|value| value.pointer("/build/restart_required"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let bad = binaries.iter().any(|binary| binary.error.is_some())
        || health.is_none()
        || version_skew
        || flywheel_version_skew
        || !stale_pids.is_empty()
        || toron.is_none()
        || toron_restart_required;

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
                "status": toron,
                "error": toron_error,
                "restart_required": toron_restart_required,
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
                "health unavailable at {}: {}",
                doorbell_url(),
                health_error.as_deref().unwrap_or("unknown failure")
            ),
        }

        println!("\n== toron ==");
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

        println!("\nstack: {}", if bad { "FAILED" } else { "ok" });
    }

    if check && bad {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
