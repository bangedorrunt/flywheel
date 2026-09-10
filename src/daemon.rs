// Daemon lifecycle: `flywheel daemon start|stop|restart|status`.
//
// Mirrors toron's daemon.rs supervision strategy, adapted to `flywheel serve`:
// - macOS: launchd LaunchAgent (`~/Library/LaunchAgents/<label>.plist`).
// - Linux with systemd: user unit (`~/.config/systemd/user/<label>.service`).
// - Fallback: pidfile + detached `flywheel serve` spawn
//   (`~/.local/state/flywheel/flywheel.pid`).
//
// SAFETY: no `unsafe` — host supervision shells out to `id -u` / `kill`,
// matching toron's approach.
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Default state dir: `$XDG_STATE_HOME/flywheel` else `~/.local/state/flywheel`.
pub fn default_state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("flywheel");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("flywheel");
    }
    PathBuf::from("flywheel-state")
}

/// The LaunchAgent label / systemd unit name.
pub const DAEMON_LABEL: &str = "dev.flywheel.daemon";

pub fn read_pidfile(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u32>().ok()
}

/// Probe whether a TCP address accepts connections (2s budget).
pub fn addr_alive(addr: &str) -> bool {
    let Ok(sa): Result<SocketAddr, _> = addr.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&sa, Duration::from_millis(2000)).is_ok()
}

/// How the daemon is currently supervised / running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    /// Running under launchd (macOS).
    RunningLaunchd,
    /// Running under systemd (Linux).
    RunningSystemd,
    /// Running via pidfile spawn (fallback).
    RunningPidfile(u32),
    /// Address responds but nothing manages it (a stray manual `flywheel serve`).
    RunningUnmanaged,
    Stopped,
}

/// Detect state: launchd (darwin) / systemd (linux) / pidfile + addr probe.
pub fn detect_state(label: &str, pidfile: &Path, addr: &str) -> DaemonState {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let out = Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{label}")])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            return DaemonState::RunningLaunchd;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("systemctl")
            .args(["--user", "is-active", label])
            .output();
        if let Ok(o) = out
            && String::from_utf8_lossy(&o.stdout).trim() == "active"
        {
            return DaemonState::RunningSystemd;
        }
    }
    if let Some(pid) = read_pidfile(pidfile)
        && process_alive(pid)
    {
        return DaemonState::RunningPidfile(pid);
    }
    if addr_alive(addr) {
        return DaemonState::RunningUnmanaged;
    }
    DaemonState::Stopped
}

/// Whether a pid is a live process on this host (`kill -0` shell-out).
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let out = Command::new("kill").args(["-0", &pid.to_string()]).output();
    matches!(out, Ok(o) if o.status.success())
}

/// Parse the first pid from `lsof -t` output (pure — unit-tested).
pub fn first_pid_from_lsof(out: &str) -> Option<u32> {
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.parse::<u32>().ok())
}

/// The pid of the process LISTENING on `addr` (lsof). `None` when the port
/// is free or lsof is unavailable.
pub fn port_holder_pid(addr: &str) -> Option<u32> {
    let port = addr.rsplit(':').next()?;
    let out = Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    first_pid_from_lsof(&String::from_utf8_lossy(&out.stdout))
}

/// Wait up to `secs` for the address to accept connections (restart verify).
pub fn wait_addr(addr: &str, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if addr_alive(addr) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
}

/// Current uid via `id -u` (keeps the binary unsafe-free).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn current_uid() -> String {
    let out = Command::new("id").arg("-u").output();
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".to_string())
}

/// Render the launchd plist (pure — golden-tested). KeepAlive on
/// non-zero exit; logs under the state dir.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist_xml(label: &str, binary: &Path, port: u16, out_log: &Path, err_log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
		<string>serve</string>
		<string>--port</string>
		<string>{port}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>5</integer>
	<key>StandardOutPath</key>
	<string>{}</string>
	<key>StandardErrorPath</key>
	<string>{}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>RUST_LOG</key>
		<string>warn</string>
	</dict>
</dict>
</plist>
"#,
        binary.display(),
        out_log.display(),
        err_log.display(),
    )
}

/// The CLI `serve` verb string the unit runs.
pub fn serve_verb() -> &'static str {
    "serve"
}

#[cfg(target_os = "macos")]
fn home_launch_agents() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME unset; cannot locate LaunchAgents"))?;
    Ok(PathBuf::from(home).join("Library").join("LaunchAgents"))
}

#[cfg(target_os = "linux")]
fn home_systemd_user() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("systemd").join("user"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME unset; cannot locate systemd user units"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("systemd")
        .join("user"))
}

/// `start`: ensure running (install-if-absent on supervised hosts; pidfile
/// spawn on fallback hosts). A start that starts NOTHING must not exit 0 —
/// an unmanaged holder is a hard error naming the pid (toron 2026-08-17
/// lesson: the silent Ok made restart a no-op while a stray serve kept
/// serving the old build).
pub fn start(state: &Path, addr: &str) -> anyhow::Result<()> {
    let pidfile = state.join("flywheel.pid");
    let port: u16 = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("cannot parse port from {addr:?}"))?;

    match detect_state(DAEMON_LABEL, &pidfile, addr) {
        DaemonState::RunningLaunchd | DaemonState::RunningSystemd => {
            println!("{DAEMON_LABEL} already running (supervised)");
            return Ok(());
        }
        DaemonState::RunningPidfile(pid) => {
            println!("{DAEMON_LABEL} already running (pid {pid})");
            return Ok(());
        }
        DaemonState::RunningUnmanaged => {
            let pid = port_holder_pid(addr);
            anyhow::bail!(
                "already serving {addr} (unmanaged pid {}) — stop it first, or \
                 `flywheel daemon restart --force` to replace it",
                pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            );
        }
        DaemonState::Stopped => {}
    }

    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        // kickstart -k restarts a loaded-but-crashed job.
        let out = Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{DAEMON_LABEL}")])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            if !wait_addr(addr, 10) {
                anyhow::bail!("started {DAEMON_LABEL} but {} never answered", addr);
            }
            println!("started {DAEMON_LABEL} (launchd)");
            return Ok(());
        }
        // Not loaded yet — install (idempotent) then kickstart.
        install(state, port)?;
        let out = Command::new("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{DAEMON_LABEL}")])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl kickstart failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        if !wait_addr(addr, 10) {
            anyhow::bail!("started {DAEMON_LABEL} but {addr} never answered");
        }
        println!("started {DAEMON_LABEL} (launchd)");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("systemctl")
            .args(["--user", "start", DAEMON_LABEL])
            .output()?;
        if !out.status.success() {
            install(state, port)?;
        }
        let out = Command::new("systemctl")
            .args(["--user", "start", DAEMON_LABEL])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl start failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        if !wait_addr(addr, 10) {
            anyhow::bail!("started {DAEMON_LABEL} but {addr} never answered");
        }
        println!("started {DAEMON_LABEL} (systemd)");
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        spawn_pidfile(state, port)?;
        if !wait_addr(addr, 10) {
            anyhow::bail!("spawned serve but {addr} never answered");
        }
        println!("started (pidfile {})", state.join("flywheel.pid").display());
        Ok(())
    }
}

/// Write + bootstrap the LaunchAgent / user unit. Idempotent.
pub fn install(state: &Path, port: u16) -> anyhow::Result<()> {
    let binary = std::env::current_exe()?;
    std::fs::create_dir_all(state)?;
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let agents = home_launch_agents()?;
        std::fs::create_dir_all(&agents)?;
        let plist = agents.join(format!("{DAEMON_LABEL}.plist"));
        std::fs::write(
            &plist,
            plist_xml(
                DAEMON_LABEL,
                &binary,
                port,
                &state.join("serve.out.log"),
                &state.join("serve.err.log"),
            ),
        )?;
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{DAEMON_LABEL}")])
            .output();
        let out = Command::new("launchctl")
            .args([
                "bootstrap",
                &format!("gui/{uid}"),
                plist.to_str().unwrap_or_default(),
            ])
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.contains("already") && !stderr.contains("Bootstrap failed: 17") {
                anyhow::bail!("launchctl bootstrap failed: {stderr}");
            }
        }
        println!(
            "installed + started {DAEMON_LABEL} (plist {})",
            plist.display()
        );
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let units = home_systemd_user()?;
        std::fs::create_dir_all(&units)?;
        let unit = units.join(format!("{DAEMON_LABEL}.service"));
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=flywheel serve daemon\n\n[Service]\nExecStart={} serve --port {port}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
                binary.display()
            ),
        )?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let out = Command::new("systemctl")
            .args(["--user", "enable", "--now", DAEMON_LABEL])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl enable --now failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!(
            "installed + started {DAEMON_LABEL} (unit {})",
            unit.display()
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (state, port, binary);
        anyhow::bail!(
            "daemon install: unsupported host — use `flywheel daemon start` (pidfile mode)"
        )
    }
}

/// Detached pidfile spawn (fallback hosts): logs to the state dir.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn spawn_pidfile(state: &Path, port: u16) -> anyhow::Result<()> {
    let binary = std::env::current_exe()?;
    let out = std::fs::File::create(state.join("serve.out.log"))?;
    let err = std::fs::File::create(state.join("serve.err.log"))?;
    let child = Command::new(binary)
        .args(["serve", "--port", &port.to_string()])
        .stdout(out)
        .stderr(err)
        .spawn()?;
    std::fs::write(state.join("flywheel.pid"), child.id().to_string())?;
    Ok(())
}

/// `stop`: bootout/systemctl stop/pidfile SIGTERM. Idempotent. An unmanaged
/// port holder is refused unless `force`.
pub fn stop(state: &Path, addr: &str, force: bool) -> anyhow::Result<()> {
    let pidfile = state.join("flywheel.pid");
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let out = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{DAEMON_LABEL}")])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            // Grace window: a fresh-bootout socket still accepting is a
            // teardown race, not a holder.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while addr_alive(addr) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            if !addr_alive(addr) {
                println!("stopped {DAEMON_LABEL} (launchd)");
                return Ok(());
            }
        }
        if let Some(pid) = read_pidfile(&pidfile)
            && process_alive(pid)
        {
            let _ = Command::new("kill").arg(pid.to_string()).output();
            println!("stopped (pid {pid})");
            return Ok(());
        }
        if addr_alive(addr) {
            let pid = port_holder_pid(addr);
            if force && let Some(p) = pid {
                let _ = Command::new("kill").arg(p.to_string()).output();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while addr_alive(addr) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                if addr_alive(addr) {
                    anyhow::bail!(
                        "unmanaged pid {p} still holds {addr} after SIGTERM — kill it manually"
                    );
                }
                println!("stopped unmanaged instance (pid {p})");
                return Ok(());
            }
            anyhow::bail!(
                "not managed (launchd/pidfile) but {addr} responds (unmanaged pid {}) — \
                 refusing to kill it; `flywheel daemon stop --force` to replace it",
                pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            );
        }
        println!("not running");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("systemctl")
            .args(["--user", "stop", DAEMON_LABEL])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl stop failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("stopped {DAEMON_LABEL} (systemd)");
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if let Some(pid) = read_pidfile(&pidfile)
            && process_alive(pid)
        {
            let _ = Command::new("kill").arg(pid.to_string()).output();
            println!("stopped (pid {pid})");
            return Ok(());
        }
        println!("not running");
        Ok(())
    }
}

/// `status`: print the detection result.
pub fn status(state: &Path, addr: &str) -> anyhow::Result<()> {
    let pidfile = state.join("flywheel.pid");
    match detect_state(DAEMON_LABEL, &pidfile, addr) {
        DaemonState::RunningLaunchd => println!("running (launchd {DAEMON_LABEL})"),
        DaemonState::RunningSystemd => println!("running (systemd {DAEMON_LABEL})"),
        DaemonState::RunningPidfile(pid) => println!("running (pidfile {pid})"),
        DaemonState::RunningUnmanaged => {
            let pid = port_holder_pid(addr);
            println!(
                "running (unmanaged instance on {addr}, pid {})",
                pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            );
        }
        DaemonState::Stopped => println!("stopped"),
    }
    Ok(())
}

/// `restart`: stop, then start, then VERIFY the new daemon actually serves
/// the address (stop+start alone can report success while a stray instance
/// keeps serving the old build — toron 2026-08-17 lesson).
pub fn restart(state: &Path, addr: &str, force: bool) -> anyhow::Result<()> {
    stop(state, addr, force)?;
    if addr_alive(addr) {
        let pid = port_holder_pid(addr);
        anyhow::bail!(
            "{addr} still responds after stop (unmanaged pid {}) — `flywheel daemon restart --force`",
            pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        );
    }
    start(state, addr)?;
    Ok(())
}

/// Plane-D ride-along for `flywheel daemon start`: ensure the doorbell
/// daemon is running by shelling its own lifecycle (`doorbell start`
/// verifies /health before exit 0 — same bar as ours). Binary lookup:
/// PATH first, then beside our own exe (dev checkouts). Absent binary
/// fails loudly naming the install (`cargo install --path
/// crates/doorbell-bin`) — never a silent skip, or the operator believes
/// wake is always-on while nothing hosts it.
pub fn ensure_doorbell() -> anyhow::Result<()> {
    let bin = which_doorbell().ok_or_else(|| {
        anyhow::anyhow!(
            "doorbell binary not found on PATH or beside {:?}; install it (`cargo install --path crates/doorbell-bin`) so `flywheel daemon start` can host plane D",
            std::env::current_exe().ok()
        )
    })?;
    // A unit-hosted daemon (launchd/systemd) answers health without a
    // pidfile, which `doorbell start` reads as a foreign squatter and
    // refuses. So: healthy port first means already running.
    let status = Command::new(&bin).arg("status").output();
    if let Ok(o) = status {
        let body = String::from_utf8_lossy(&o.stdout);
        if o.status.success() && (body.contains("\"alive\":true") || body.contains("\"health\":{")) {
            println!("doorbell: already running");
            return Ok(());
        }
    }
    let out = Command::new(&bin)
        .arg("start")
        .output()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e:#}", bin.display()))?;
    print!("{}", String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        anyhow::bail!(
            "`doorbell start` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn which_doorbell() -> Option<PathBuf> {
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("doorbell"))
            .find(|p| p.is_file())
    }) {
        return Some(found);
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("doorbell")))
        .filter(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfile_parses_trimmed_u32() {
        let dir = std::env::temp_dir().join(format!("fw-daemon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("test.pid");
        std::fs::write(&p, " 12345\n").unwrap();
        assert_eq!(read_pidfile(&p), Some(12345));
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(read_pidfile(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lsof_output_first_pid() {
        assert_eq!(first_pid_from_lsof("123\n456\n"), Some(123));
        assert_eq!(first_pid_from_lsof("\n\n789"), Some(789));
        assert_eq!(first_pid_from_lsof(""), None);
        assert_eq!(first_pid_from_lsof("abc"), None);
    }

    #[test]
    fn dead_port_is_not_alive() {
        // Bind without listening: deterministic RST, port stays reserved.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(!addr_alive(&format!("127.0.0.1:{port}")));
    }

    #[test]
    fn plist_contains_binary_and_port_and_keepalive() {
        let xml = plist_xml(
            "dev.flywheel.daemon",
            Path::new("/usr/local/bin/flywheel"),
            8787,
            Path::new("/tmp/out.log"),
            Path::new("/tmp/err.log"),
        );
        assert!(xml.contains("/usr/local/bin/flywheel"));
        assert!(xml.contains("<string>serve</string>"));
        assert!(xml.contains("<string>8787</string>"));
        assert!(xml.contains("SuccessfulExit"));
        assert!(xml.contains("RunAtLoad"));
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!process_alive(0));
    }
}
