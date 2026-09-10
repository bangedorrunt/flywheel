// governed-by: ADR-0012
//! Daemon lifecycle (D2): paths, pidfile, spawn, verify, stop, status.
//! Units ship `Restart=on-failure` (a clean exit over a port conflict must
//! STAY dead — the bd-serve-double-spawn lesson); no `always`.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 18980;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub port: u16,
    /// Flywheel state dir (journals, leases, registries live here —
    /// shared with embedded loops). Mirrors flywheel's own resolution:
    /// `DOORBELL_FLYWHEEL_STATE_DIR` > `XDG_STATE_HOME/flywheel` >
    /// platform state dir + `flywheel`.
    pub flywheel_state: PathBuf,
    /// Hosted loops as `(project, workspace)` pairs (`--host p/w`,
    /// repeatable). Empty = standby: bind + reads only.
    pub hosts: Vec<(String, String)>,
}

/// Parse one `--host project/workspace` value.
pub fn parse_host(s: &str) -> anyhow::Result<(String, String)> {
    match s.split_once('/') {
        Some((p, w)) if !p.is_empty() && !w.is_empty() => Ok((p.to_string(), w.to_string())),
        _ => anyhow::bail!("--host must be project/workspace, got {s:?}"),
    }
}

pub fn default_flywheel_state() -> PathBuf {
    std::env::var_os("DOORBELL_FLYWHEEL_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).map(|d| d.join("flywheel"))
        })
        .unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("~"))
                        .join(".local/state")
                })
                .join("flywheel")
        })
}

impl Config {
    pub fn resolve(state_dir: Option<PathBuf>, port: Option<u16>) -> Result<Self> {
        Self::resolve_full(state_dir, port, None, Vec::new())
    }

    pub fn resolve_full(
        state_dir: Option<PathBuf>,
        port: Option<u16>,
        flywheel_state: Option<PathBuf>,
        hosts: Vec<(String, String)>,
    ) -> Result<Self> {
        let state_dir = state_dir
            .or_else(|| std::env::var_os("DOORBELL_STATE_DIR").map(PathBuf::from))
            .or_else(|| dirs::data_dir().map(|d| d.join("doorbell")))
            .unwrap_or_else(|| PathBuf::from("/tmp/doorbell-state"));
        let port = port
            .or_else(|| {
                std::env::var("DOORBELL_PORT")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(DEFAULT_PORT);
        let flywheel_state = flywheel_state.unwrap_or_else(default_flywheel_state);
        Ok(Self {
            state_dir,
            port,
            flywheel_state,
            hosts,
        })
    }

    pub fn pid_file(&self) -> PathBuf {
        self.state_dir.join("doorbell.pid")
    }

    pub fn socket_path(&self) -> PathBuf {
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(rt).join("doorbell.sock")
        } else {
            self.state_dir.join("doorbell.sock")
        }
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("doorbell.log")
    }
}

fn read_pid(path: &PathBuf) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Our pidfile names a live process: managed by us.
fn managed(self_cfg: &Config) -> Option<u32> {
    read_pid(&self_cfg.pid_file()).filter(|p| pid_alive(*p))
}

fn health_get(cfg: &Config, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", cfg.port).parse().ok()?,
        Duration::from_secs(2),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;
    let idx = body.find("\r\n\r\n")?;
    Some(body[idx + 4..].to_string())
}

fn health_probe(cfg: &Config) -> Option<serde_json::Value> {
    let body = health_get(cfg, "/health")?;
    serde_json::from_str(&body).ok()
}
fn wait_for(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    cond()
}

/// Start: spawn the detached serve child, verify /health, exit 0 only
/// when the daemon answers. Stale socket files are removed after proving
/// nothing listens; a live TCP squatter fails loudly (bind fence is .3).
pub fn start(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.log_file())
        .with_context(|| format!("open {}", cfg.log_file().display()))?;
    let err_log = log.try_clone().context("clone log handle")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .arg("--state-dir")
        .arg(&cfg.state_dir)
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--flywheel-state-dir")
        .arg(&cfg.flywheel_state);
    for (p, w) in &cfg.hosts {
        cmd.arg("--host").arg(format!("{p}/{w}"));
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err_log)
        .spawn()
        .context("spawn serve child")?;
    // Detached by disowning: never wait on it (reparented on unix).
    let pid = child.id();
    std::mem::forget(child);
    std::fs::write(cfg.pid_file(), format!("{pid}\n")).context("write pidfile")?;
    if wait_for(|| health_probe(cfg).is_some(), START_TIMEOUT) {
        // The probe answering is not enough: a stale orphan on this port
        // answers too. The owner must be the child we just spawned.
        if let Some(owner) = health_probe(cfg).and_then(|h| h.get("owner")?.get("pid")?.as_u64())
            && owner == u64::from(pid)
        {
            println!("doorbell: started (pid {pid}, port {})", cfg.port);
            return Ok(());
        }
        anyhow::bail!(
            "doorbell: port {} answered by foreign owner, not child {pid}",
            cfg.port
        );
    }
    anyhow::bail!("doorbell: serve child (pid {pid}) never answered /health");
}

/// Stop: SIGTERM, 5s grace, assert port+socket dead. Refuses unmanaged
/// socket holders without --force (never clobbers a stranger's bind).
pub fn stop(cfg: &Config, force: bool) -> Result<()> {
    match managed(cfg) {
        Some(pid) => {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
            if wait_for(|| !pid_alive(pid), STOP_GRACE) {
                let _ = std::fs::remove_file(cfg.pid_file());
                println!("doorbell: stopped (pid {pid})");
                return Ok(());
            }
            anyhow::bail!("doorbell: pid {pid} ignored SIGTERM within 5s");
        }
        None => {
            if health_probe(cfg).is_some() {
                if !force {
                    anyhow::bail!(
                        "doorbell: port {} answers but no live pidfile — unmanaged holder, refusing (use --force)",
                        cfg.port
                    );
                }
                anyhow::bail!(
                    "doorbell: port {} answers but owner unknown — --force cannot kill what it cannot identify; stop the owner instead",
                    cfg.port
                );
            }
            let _ = std::fs::remove_file(cfg.pid_file());
            println!("doorbell: already stopped");
            Ok(())
        }
    }
}

pub fn restart(cfg: &Config) -> Result<()> {
    let _ = stop(cfg, false);
    start(cfg)
}

pub fn status(cfg: &Config) -> Result<()> {
    let pid = managed(cfg);
    let health = health_probe(cfg);
    let out = serde_json::json!({
        "alive": pid.is_some() && health.is_some(),
        "pid": pid,
        "port": cfg.port,
        "socket": cfg.socket_path().display().to_string(),
        "health": health,
    });
    println!("{out}");
    Ok(())
}

/// Install the autostart unit (idempotent) and load it: macOS launchd
/// agent, Linux systemd user unit. Template placeholders take the live
/// binary path + state dir; `serve` reads both back from argv/env so the
/// unit and the pidfile path never diverge. Other platforms: loud error
/// naming manual `doorbell start`.
pub fn install(cfg: &Config) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let body = render_autostart(
        &exe.display().to_string(),
        &cfg.state_dir.display().to_string(),
    );
    install_rendered(cfg, &body)
}

/// Render the platform autostart unit with live paths (pure — golden
/// tested). Placeholders take the binary path + state dir; `serve` reads
/// both back from argv/env so the unit and the pidfile path never diverge.
pub fn render_autostart(exe: &str, state_dir: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        return include_str!("../units/com.doorbell.serve.plist")
            .replace("DOORBELL_BIN", exe)
            .replace("DOORBELL_STATE_DIR", state_dir);
    }
    #[cfg(not(target_os = "macos"))]
    {
        return include_str!("../units/doorbell.service")
            .replace("DOORBELL_BIN", exe)
            .replace("DOORBELL_STATE_DIR", state_dir);
    }
}

fn install_rendered(_cfg: &Config, body: &str) -> Result<()> {
    {
        let home = std::env::var("HOME").context("HOME unset")?;
        let dir = PathBuf::from(home).join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("com.doorbell.serve.plist");
        std::fs::write(&path, body)?;
        let uid = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "501".to_string());
        let domain = format!("gui/{uid}/com.doorbell.serve");
        let boot = std::process::Command::new("launchctl")
            .args(["bootstrap", &format!("gui/{uid}"), &path.display().to_string()])
            .output();
        match boot {
            Ok(o) if o.status.success() => {}
            _ => {
                // Already loaded (or kicked): kickstart -k restarts into the
                // current binary instead of failing install.
                let kick = std::process::Command::new("launchctl")
                    .args(["kickstart", "-k", &domain])
                    .output();
                if !kick.map(|o| o.status.success()).unwrap_or(false) {
                    anyhow::bail!("launchctl bootstrap + kickstart both failed for {domain}");
                }
            }
        }
        println!("doorbell: autostart installed ({})", path.display());
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME set")?;
        let dir = base.join("systemd").join("user");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("doorbell.service");
        std::fs::write(&path, body)?;
        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "doorbell.service"],
        ] {
            let st = std::process::Command::new("systemctl")
                .args(&args)
                .status()
                .context("systemctl failed (no user systemd?)")?;
            if !st.success() {
                anyhow::bail!("systemctl {} failed", args.join(" "));
            }
        }
        println!("doorbell: autostart installed ({})", path.display());
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        anyhow::bail!(
            "no autostart unit for this platform; run `doorbell start` (pidfile) instead"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_live_paths() {
        let body = render_autostart("/opt/doorbell", "/var/dbll");
        assert!(body.contains("/opt/doorbell"), "binary path rendered");
        assert!(body.contains("/var/dbll"), "state dir rendered");
        assert!(!body.contains("DOORBELL_BIN"), "no placeholder survives");
        assert!(!body.contains("DOORBELL_STATE_DIR"), "no placeholder survives");
    }
}
