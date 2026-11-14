// SAFETY: no `unsafe` — host supervision shells out to `id -u` / `kill`,
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

/// The one serve port: `flywheel serve` and every `flywheel daemon` verb
/// default to it, and the generated launchd/systemd unit runs it (bd-zdal).
pub const SERVE_PORT_DEFAULT: u16 = 8787;

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
    /// Running under systemd (Linux). Constructed only inside the
    /// `cfg(target_os = "linux")` branch of [`detect_state`], so on macOS the
    /// variant is reachable in `match` arms but never built — a platform
    /// difference the lint cannot see.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
/// non-zero exit; logs under the state dir. `working_dir` pins the unit's cwd
/// (cron resolves its project tier and `br` from there).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn plist_xml(
    label: &str,
    binary: &Path,
    args: &[String],
    working_dir: Option<&Path>,
    out_log: &Path,
    err_log: &Path,
    path_env: Option<&str>,
) -> String {
    let mut program = format!("\t\t<string>{}</string>\n", binary.display());
    for arg in args {
        program.push_str(&format!("\t\t<string>{arg}</string>\n"));
    }
    let working_dir = working_dir.map_or_else(String::new, |dir| {
        format!(
            "\t<key>WorkingDirectory</key>\n\t<string>{}</string>\n",
            dir.display()
        )
    });
    let path_env = path_env.map_or_else(String::new, |path| {
        format!("\t\t<key>PATH</key>\n\t\t<string>{path}</string>\n")
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{program}	</array>
	<key>RunAtLoad</key>
	<true/>
{working_dir}	<key>KeepAlive</key>
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
{path_env}	</dict>
</dict>
</plist>
"#,
        out_log.display(),
        err_log.display(),
    )
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
    install_unit(
        state,
        &UnitSpec {
            label: DAEMON_LABEL,
            args: vec!["serve".into(), "--port".into(), port.to_string()],
            working_dir: None,
            stem: "serve",
        },
    )
}

/// One supervised process: its launchd label / systemd unit name, argv, cwd,
/// and log stem under the state dir.
struct UnitSpec<'a> {
    label: &'a str,
    args: Vec<String>,
    working_dir: Option<&'a Path>,
    stem: &'a str,
}

/// Write + bootstrap one unit. Idempotent: the old unit is booted out first so
/// an edited argv/cwd takes effect.
fn install_unit(state: &Path, spec: &UnitSpec<'_>) -> anyhow::Result<()> {
    let label = spec.label;
    let binary = std::env::current_exe()?;
    std::fs::create_dir_all(state)?;
    let out_log = state.join(format!("{}.out.log", spec.stem));
    let err_log = state.join(format!("{}.err.log", spec.stem));
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let agents = home_launch_agents()?;
        std::fs::create_dir_all(&agents)?;
        let plist = agents.join(format!("{label}.plist"));
        std::fs::write(
            &plist,
            plist_xml(
                label,
                &binary,
                &spec.args,
                spec.working_dir,
                &out_log,
                &err_log,
                std::env::var("PATH").ok().as_deref(),
            ),
        )?;
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{label}")])
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
        println!("installed + started {label} (plist {})", plist.display());
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let units = home_systemd_user()?;
        std::fs::create_dir_all(&units)?;
        let unit = units.join(format!("{label}.service"));
        let working_dir = spec.working_dir.map_or_else(String::new, |dir| {
            format!("WorkingDirectory={}\n", dir.display())
        });
        let path_env = std::env::var("PATH")
            .map(|path| format!("Environment=PATH={path}\n"))
            .unwrap_or_default();
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=flywheel {} host\n\n[Service]\n{working_dir}{path_env}ExecStart={} {}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
                spec.stem,
                binary.display(),
                spec.args.join(" ")
            ),
        )?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        let out = Command::new("systemctl")
            .args(["--user", "enable", "--now", label])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl enable --now failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("installed + started {label} (unit {})", unit.display());
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (state, spec, binary);
        anyhow::bail!(
            "daemon install: unsupported host — use `flywheel daemon start` (pidfile mode)"
        )
    }
}

/// True when a launchd/systemd user unit is loaded/active.
pub fn unit_active(label: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{label}")])
            .output()
            .is_ok_and(|o| o.status.success())
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("systemctl")
            .args(["--user", "is-active", label])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = label;
        false
    }
}

fn unit_slug(project: &str) -> String {
    let s: String = project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .to_ascii_lowercase();
    if s.is_empty() { "default".into() } else { s }
}

/// Install + start the per-project doorbell supervisor. The unit runs
/// `flywheel doorbell supervise <project>` as one long-lived process:
/// `RunAtLoad` on boot, `KeepAlive` on failure, and the supervisor itself
/// exits when its own binary changes, so reinstall is picked up by the
/// service manager without a manual restart. The loop re-runs the idempotent
/// `doorbell start`, which resolves the current session workspace id.
/// The supervisor's one key: a path-valued project is the same project as its
/// slug (bd-bj9xk: chiebukuro ran TWO supervisors, one per spelling, because
/// the unit label was built from the raw argument).
pub(crate) fn doorbell_supervisor_key(project: &str) -> String {
    unit_slug(&crate::doorbell::canonical_project(project))
}

/// Units under `dir` that supervise the SAME canonical project under a
/// non-canonical key (bd-bj9xk: chiebukuro was installed as both
/// `dev.flywheel.doorbell.chiebukuro` and the path-splatted
/// `…-users-…-workspace-chiebukuro`). Pure over the plist bodies so a test
/// never needs a supervisor.
pub(crate) fn stale_supervisor_units(
    dir: &Path,
    canonical: &str,
) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(label) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !label.starts_with("dev.flywheel.doorbell.") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(project) = plist_project_argument(&body) else {
            continue;
        };
        if crate::doorbell::canonical_project(&project) != canonical {
            continue;
        }
        if label == format!("dev.flywheel.doorbell.{canonical}") {
            continue;
        }
        out.push((label.to_string(), path));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The project argument of a `doorbell supervise <project>` unit, read from
/// the plist's ProgramArguments (or its systemd ExecStart line).
pub(crate) fn plist_project_argument(body: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        let value = line
            .strip_prefix("<string>")
            .and_then(|rest| rest.strip_suffix("</string>"))
            .or_else(|| line.strip_prefix("ExecStart="));
        if let Some(value) = value {
            tokens.extend(value.split_whitespace().map(str::to_string));
        }
    }
    let at = tokens.iter().position(|t| t == "supervise")?;
    tokens.get(at + 1).cloned()
}

/// Typed outcome of retiring one stale supervisor unit. The plist is the only
/// index a later install has, so it is removed ONLY when the unit is provably
/// gone (bd-bj9xk review): a failed bootout that also deleted the plist would
/// leave a running supervisor invisible forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetireOutcome {
    Retired,
    NotLoaded,
    BootoutFailed(String),
}

/// The supervisor effect, behind a port so the outcome logic is unit-tested
/// with a fake (bd-bj9xk review ask 1). Production: launchctl / systemctl.
pub(crate) trait SupervisorPort {
    /// Boot one unit out. `Ok(Err(stderr))` = the tool ran and failed.
    fn bootout(&self, label: &str) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
struct LaunchctlPort;
#[cfg(target_os = "macos")]
impl SupervisorPort for LaunchctlPort {
    fn bootout(&self, label: &str) -> Result<(), String> {
        let uid = current_uid();
        let out = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{label}")])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

#[cfg(target_os = "linux")]
struct SystemdPort;
#[cfg(target_os = "linux")]
impl SupervisorPort for SystemdPort {
    fn bootout(&self, label: &str) -> Result<(), String> {
        let out = Command::new("systemctl")
            .args(["--user", "disable", "--now", &format!("{label}.service")])
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(target_os = "macos")]
fn supervisor_port() -> Box<dyn SupervisorPort> {
    Box::new(LaunchctlPort)
}
#[cfg(target_os = "linux")]
fn supervisor_port() -> Box<dyn SupervisorPort> {
    Box::new(SystemdPort)
}

/// Retire one unit. The plist goes only on a proven-gone outcome; a failed
/// bootout keeps it so the next install can still find and retry the unit.
pub(crate) fn retire_stale_unit(
    port: &dyn SupervisorPort,
    label: &str,
    plist: &Path,
) -> RetireOutcome {
    match port.bootout(label) {
        Ok(()) => {
            let _ = std::fs::remove_file(plist);
            RetireOutcome::Retired
        }
        Err(stderr) => {
            let lower = stderr.to_ascii_lowercase();
            let not_loaded = lower.contains("no such process")
                || lower.contains("could not find service")
                || lower.contains("not loaded")
                || lower.contains("not-found");
            if not_loaded {
                let _ = std::fs::remove_file(plist);
                RetireOutcome::NotLoaded
            } else {
                RetireOutcome::BootoutFailed(stderr)
            }
        }
    }
}

/// Find + retire every stale unit for this project: intent audit first, then
/// the attempt, then an outcome audit (loud on failure). Never fails the
/// install.
fn retire_stale_supervisor_units(project: &str, canonical: &str, state: &Path) -> Vec<String> {
    let Ok(agents) = home_launch_agents() else {
        return Vec::new();
    };
    let port = supervisor_port();
    let mut retired = Vec::new();
    for (label, plist) in stale_supervisor_units(&agents, canonical) {
        crate::doorbell::audit_append(
            state,
            &format!("supervisor-{canonical}"),
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "kind": "supervisor_stale_unit_retire_intent",
                "label": label,
                "project": project,
                "canonical": canonical,
            }),
        );
        let outcome = retire_stale_unit(&*port, &label, &plist);
        let (kind, detail) = match &outcome {
            RetireOutcome::Retired => ("supervisor_stale_unit_retired", None),
            RetireOutcome::NotLoaded => ("supervisor_stale_unit_not_loaded", None),
            RetireOutcome::BootoutFailed(stderr) => {
                eprintln!(
                    "flywheel: STALE SUPERVISOR SURVIVED bootout: {label} for project {project}: {stderr} — plist kept so the next install can retry"
                );
                ("supervisor_stale_unit_bootout_failed", Some(stderr.clone()))
            }
        };
        crate::doorbell::audit_append(
            state,
            &format!("supervisor-{canonical}"),
            serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "kind": kind,
                "label": label,
                "project": project,
                "canonical": canonical,
                "detail": detail,
            }),
        );
        if matches!(outcome, RetireOutcome::Retired | RetireOutcome::NotLoaded) {
            retired.push(label);
        }
    }
    retired
}

pub fn install_doorbell_supervisor(
    cfg: &crate::config::Config,
    project: &str,
) -> anyhow::Result<()> {
    let project = crate::doorbell::canonical_project(project);
    let project = project.as_str();
    let slug = doorbell_supervisor_key(project);
    let label = format!("dev.flywheel.doorbell.{slug}");
    let _ = retire_stale_supervisor_units(project, &slug, &crate::config::state_dir());
    if unit_active(&label) {
        return Ok(());
    }
    let state = crate::config::state_dir();
    let dir = crate::project_dir(cfg, project);
    let working_dir = dir.is_dir().then_some(dir.as_path());
    let stem = format!("doorbell-{slug}");
    install_unit(
        &state,
        &UnitSpec {
            label: &label,
            args: vec![
                "doorbell".to_string(),
                "supervise".to_string(),
                project.to_string(),
            ],
            working_dir,
            stem: &stem,
        },
    )
}

/// The cron host's unit: `flywheel cron start` as its own supervised process.///
/// Not folded into `serve` on purpose. `serve` is the HTTP/wake surface and
/// dies on a bind conflict or a handler panic; cron owns ticks and the
/// doorbell-supervisor minute schedule. Separate units keep one failure from
/// stopping the other, and each restarts at boot (`RunAtLoad`).
pub const CRON_LABEL: &str = "dev.flywheel.cron";

/// Install + start the cron host. `working_dir` becomes the unit's cwd, so the
/// project tier (`<project>/.flywheel/crons`) and `br create` resolve exactly
/// as an interactive `flywheel cron start` from that directory does.
pub fn start_cron(state: &Path, working_dir: &Path, project: Option<&str>) -> anyhow::Result<()> {
    if cron_running() {
        println!("{CRON_LABEL} already running (supervised)");
        return Ok(());
    }
    let mut args = vec!["cron".to_string(), "start".to_string()];
    if let Some(project) = project {
        args.push("--project".to_string());
        args.push(project.to_string());
    }
    install_unit(
        state,
        &UnitSpec {
            label: CRON_LABEL,
            args,
            working_dir: Some(working_dir),
            stem: "cron",
        },
    )
}

/// Stop the cron host. Idempotent.
pub fn stop_cron() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        let out = Command::new("launchctl")
            .args(["bootout", &format!("gui/{uid}/{CRON_LABEL}")])
            .output()?;
        if out.status.success() {
            println!("stopped {CRON_LABEL} (launchd)");
        } else {
            println!("{CRON_LABEL} not running");
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("systemctl")
            .args(["--user", "stop", CRON_LABEL])
            .output()?;
        if out.status.success() {
            println!("stopped {CRON_LABEL} (systemd)");
        } else {
            println!("{CRON_LABEL} not running");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("no supervisor on this host; run `flywheel cron start` in the foreground")
    }
}

/// Whether the cron unit is loaded and running.
pub fn cron_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid();
        Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/{CRON_LABEL}")])
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .any(|line| line.trim() == "state = running")
            })
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", CRON_LABEL])
            .output()
            .is_ok_and(|out| out.status.success())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Print the cron host's supervision state (a stopped host is why ticks stop).
pub fn status_cron() {
    if cron_running() {
        println!("cron: running (supervised {CRON_LABEL})");
    } else {
        println!("cron: stopped — `flywheel daemon start` installs {CRON_LABEL}");
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
pub fn status(state: &Path, addr: &str) {
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

/// Typed result of a doorbell bring-up attempt (bd-0gmw): call sites that
/// can degrade (doorbell::ensure_running → embedded loop) need the
/// distinction that `ensure_doorbell`'s hard bail hid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoorbellEnsure {
    /// A live daemon answered `doorbell status` — attach, don't spawn.
    Attached,
    /// `doorbell start` spawned a fresh daemon.
    Started,
    /// No binary / spawn failed / env opt-out — caller degrades with `hint`.
    Degraded { hint: String },
}

/// bd-0gmw: non-bailing variant of [`ensure_doorbell`] — same
/// status-then-`doorbell start` logic, but reports instead of aborting so
/// embedded-mode call sites can degrade. Env opt-out
/// `FLYWHEEL_NO_DOORBELL_ENSURE=1` skips entirely (hermetic tests).
/// Persistence stays opt-in: pidfile-mode start only, never launchctl
/// bootstrap.
pub fn ensure_doorbell_spawn() -> DoorbellEnsure {
    if std::env::var("FLYWHEEL_NO_DOORBELL_ENSURE").as_deref() == Ok("1") {
        return DoorbellEnsure::Degraded {
            hint: "skipped (FLYWHEEL_NO_DOORBELL_ENSURE=1)".into(),
        };
    }
    let Some(bin) = which_doorbell() else {
        return DoorbellEnsure::Degraded {
            hint: format!(
                "doorbell binary not found on PATH or beside {:?}; install it (`cargo install --path crates/doorbell-cli`)",
                std::env::current_exe().ok()
            ),
        };
    };
    if let Ok(o) = Command::new(&bin).arg("status").output() {
        let body = String::from_utf8_lossy(&o.stdout);
        if o.status.success() && (body.contains("\"alive\":true") || body.contains("\"health\":{"))
        {
            println!("doorbell: already running");
            return DoorbellEnsure::Attached;
        }
    }
    let out = match Command::new(&bin).arg("start").output() {
        Ok(out) => out,
        Err(e) => {
            return DoorbellEnsure::Degraded {
                hint: format!("spawn {}: {e:#}", bin.display()),
            };
        }
    };
    print!("{}", String::from_utf8_lossy(&out.stdout));
    if !out.status.success() {
        return DoorbellEnsure::Degraded {
            hint: format!(
                "`doorbell start` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
        };
    }
    DoorbellEnsure::Started
}

/// Plane-D ride-along for `flywheel daemon start`: ensure the doorbell
/// daemon is running by shelling its own lifecycle (`doorbell start`
/// verifies /health before exit 0 — same bar as ours). Fails loudly
/// naming the install — never a silent skip, or the operator believes
/// wake is always-on while nothing hosts it.
pub fn ensure_doorbell() -> anyhow::Result<()> {
    match ensure_doorbell_spawn() {
        DoorbellEnsure::Attached | DoorbellEnsure::Started => Ok(()),
        DoorbellEnsure::Degraded { hint } => anyhow::bail!("{hint}"),
    }
}

/// Outcome of a best-effort service ensure (bd-ykqd).
/// Parked-run capability probe (bd-flywheel-probe-qjay — step 1 of the
/// vendor removal): open a scratch run on the durable engine store, park
/// it, and verify the wake fires via `drain_due`. A run that can park and
/// wake is the health signal — not a vendor `/health` endpoint.
///
/// Opens the same sqlite the effect journal uses (`mail::mailbox_roots`),
/// failing closed: a missing/unwritable dir or DB reads as DOWN, never as
/// silent success. The scratch run is cancelled best-effort afterwards; its
/// journal rows stay within the scope's retention bound.
pub fn durable_capability_probe() -> anyhow::Result<()> {
    let (data_dir, db) = crate::mail::mailbox_roots();
    durable_capability_probe_at(&data_dir, &db)
}

pub(crate) fn durable_capability_probe_at(data_dir: &Path, db: &Path) -> anyhow::Result<()> {
    use toron_core::index::durable_engine::ToronEngineStore;
    use toron_core::index::schema;
    use toron_core::journal::{JournalDb, ToronJournal};
    use toron_core::kernel::{DurableEngine, EngineMsg, OurEngine};

    std::fs::create_dir_all(data_dir).map_err(|e| {
        anyhow::anyhow!(
            "probe: cannot create engine store dir {}: {e}",
            data_dir.display()
        )
    })?;
    let db_str = db.to_string_lossy().into_owned();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("probe runtime: {e}"))?;
    rt.block_on(async move {
        let pool = schema::connect(&db_str)
            .await
            .map_err(|e| anyhow::anyhow!("probe: cannot open engine store {db_str}: {e}"))?;
        schema::migrate(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("probe: cannot migrate engine store {db_str}: {e}"))?;
        let handle = tokio::runtime::Handle::current();
        let store = ToronEngineStore::spawn(pool.clone(), &handle);
        let journal = ToronJournal::spawn(JournalDb::new(pool, "probe", 64), &handle);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .map_err(|e| anyhow::anyhow!("probe: system clock: {e}"))?;
        let run_id = format!("probe-capability-{}-{now_ms}", std::process::id());
        tokio::task::spawn_blocking(move || {
            let mut engine = OurEngine::with_journal(store, journal);
            engine
                .start(&run_id, now_ms)
                .map_err(|e| anyhow::anyhow!("probe: engine start: {e}"))?;
            let until_ms = now_ms.saturating_add(5_000);
            engine
                .park_until(&run_id, until_ms, 0, now_ms, None)
                .map_err(|e| anyhow::anyhow!("probe: engine park: {e}"))?;
            let wakes = engine
                .drain_due(until_ms)
                .map_err(|e| anyhow::anyhow!("probe: drain_due: {e}"))?;
            if !wakes
                .iter()
                .any(|w| matches!(w, EngineMsg::Woke { key, .. } if key == &run_id))
            {
                return Err(anyhow::anyhow!("probe: parked run produced no wake"));
            }
            let _ = engine.cancel(&run_id, until_ms);
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("probe task: {e}"))?
    })
}

/// PATH lookup, then beside our own exe (dev checkouts).
fn which_beside(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join(name)))
                .filter(|p| p.is_file())
        })
}

fn which_doorbell() -> Option<PathBuf> {
    which_beside("doorbell")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cron host's unit carries its own argv and cwd: `cron start` (plus
    /// `--project` when detected) in the project directory, so a supervised
    /// host resolves the project tier instead of launching at `/`.
    #[test]
    fn cron_unit_plist_pins_argv_and_working_dir() {
        let xml = plist_xml(
            CRON_LABEL,
            Path::new("/opt/bin/flywheel"),
            &[
                "cron".to_string(),
                "start".to_string(),
                "--project".to_string(),
                "flywheel".to_string(),
            ],
            Some(Path::new("/work/flywheel")),
            Path::new("/state/cron.out.log"),
            Path::new("/state/cron.err.log"),
            Some("/opt/homebrew/bin:/usr/bin:/bin"),
        );
        assert!(xml.contains("<string>cron</string>"));
        assert!(xml.contains("<string>--project</string>"));
        assert!(xml.contains("<string>flywheel</string>"));
        assert!(xml.contains("<key>WorkingDirectory</key>\n\t<string>/work/flywheel</string>"));
        assert!(
            xml.contains("<key>RunAtLoad</key>\n\t<true/>"),
            "a reboot must bring the host back"
        );
        assert!(xml.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        let serve = plist_xml(
            DAEMON_LABEL,
            Path::new("/opt/bin/flywheel"),
            &[
                "serve".to_string(),
                "--port".to_string(),
                "8787".to_string(),
            ],
            None,
            Path::new("/state/serve.out.log"),
            Path::new("/state/serve.err.log"),
            Some("/opt/homebrew/bin:/usr/bin:/bin"),
        );
        assert!(serve.contains("<string>serve</string>"));
        assert!(!serve.contains("WorkingDirectory"));
        assert!(xml.contains("<key>PATH</key>"));
        assert!(xml.contains("/opt/homebrew/bin"));
    }

    /// bd-flywheel-probe-qjay: the probe parks a scratch run and drains its
    /// wake against a temp store — no server, no daemon, no live paths.
    #[test]
    fn capability_probe_park_and_wake() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fw-probe-{}-{nanos}", std::process::id()));
        let db = dir.join("probe.sqlite");
        durable_capability_probe_at(&dir, &db).expect("scratch store parks and wakes");
        durable_capability_probe_at(&dir, &db).expect("probe reruns green");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe fails closed: a path that cannot host a store reads as
    /// DOWN, never as silent success.
    #[test]
    fn capability_probe_refuses_a_broken_path() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file =
            std::env::temp_dir().join(format!("fw-probe-file-{}-{nanos}", std::process::id()));
        std::fs::write(&file, "not a dir").unwrap();
        let db = file.join("probe.sqlite");
        assert!(
            durable_capability_probe_at(&file, &db).is_err(),
            "a file where the store dir belongs must fail"
        );
        let _ = std::fs::remove_file(&file);
    }

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
            &[
                "serve".to_string(),
                "--port".to_string(),
                SERVE_PORT_DEFAULT.to_string(),
            ],
            None,
            Path::new("/tmp/out.log"),
            Path::new("/tmp/err.log"),
            None,
        );
        assert!(xml.contains("/usr/local/bin/flywheel"));
        assert!(xml.contains("<string>serve</string>"));
        assert!(xml.contains(&format!("<string>{SERVE_PORT_DEFAULT}</string>")));
        assert!(xml.contains("SuccessfulExit"));
        assert!(xml.contains("RunAtLoad"));
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!process_alive(0));
    }

    /// governed-by: bd-z7w0 — Degraded-arm reachability: the env opt-out
    /// deterministically produces Degraded (the ensure_running fall-through
    /// that keeps embedded mode). KEEP + documented; fires whenever the
    /// operator sets FLYWHEEL_NO_DOORBELL_ENSURE=1.
    #[test]
    fn degraded_audit_ensure_degraded_on_opt_out() {
        let _g = crate::config::lock_env();
        let prior = std::env::var_os("FLYWHEEL_NO_DOORBELL_ENSURE");
        unsafe { std::env::set_var("FLYWHEEL_NO_DOORBELL_ENSURE", "1") };
        let got = ensure_doorbell_spawn();
        match prior {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_NO_DOORBELL_ENSURE", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_NO_DOORBELL_ENSURE") },
        }
        assert!(
            matches!(got, DoorbellEnsure::Degraded { .. }),
            "opt-out must degrade, got {got:?}"
        );
    }

    #[test]
    fn doorbell_supervisor_slug_is_isolated() {
        assert_eq!(super::unit_slug("toron"), "toron");
        assert_eq!(super::unit_slug("a/b c"), "a-b-c");
        assert_eq!(super::unit_slug(""), "default");
    }
}

#[cfg(test)]
mod doorbell_supervisor_one_key_tests {
    use super::*;

    /// bd-bj9xk: starting the supervisor under the path and under the slug
    /// must land on ONE unit. Live: chiebukuro ran both
    /// `dev.flywheel.doorbell.chiebukuro` and
    /// `dev.flywheel.doorbell.-users-bangedorrunt-workspace-chiebukuro`.
    #[test]
    fn path_and_slug_are_one_supervisor_unit() {
        let slug = "chiebukuro";
        let path = "/Users/op/workspace/chiebukuro";
        assert_eq!(
            doorbell_supervisor_key(path),
            doorbell_supervisor_key(slug),
            "one project, one supervisor unit"
        );
        assert_eq!(doorbell_supervisor_key(path), "chiebukuro");
        assert_eq!(
            unit_slug("/Users/op/workspace/chiebukuro"),
            "-users-op-workspace-chiebukuro",
            "the raw spelling is what produced the second unit"
        );
    }

    fn plist_with(project: &str) -> String {
        format!(
            "<plist><array>\n<string>/usr/local/bin/flywheel</string>\n<string>doorbell</string>\n<string>supervise</string>\n<string>{project}</string>\n</array></plist>\n"
        )
    }

    /// bd-bj9xk: the migration finds a unit that supervises the SAME project
    /// under a non-canonical key, and leaves the canonical one and other
    /// projects alone. Fake supervisor dir only — never real LaunchAgents.
    #[test]
    fn stale_units_for_one_project_are_found_by_canonical_key() {
        let dir = std::env::temp_dir().join(format!("fw-units-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("dev.flywheel.doorbell.chiebukuro.plist"),
            plist_with("chiebukuro"),
        )
        .unwrap();
        std::fs::write(
            dir.join("dev.flywheel.doorbell.-users-op-workspace-chiebukuro.plist"),
            plist_with("/Users/op/workspace/chiebukuro"),
        )
        .unwrap();
        std::fs::write(
            dir.join("dev.flywheel.doorbell.flywheel.plist"),
            plist_with("flywheel"),
        )
        .unwrap();

        let stale = stale_supervisor_units(&dir, "chiebukuro");
        assert_eq!(
            stale.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            vec!["dev.flywheel.doorbell.-users-op-workspace-chiebukuro"],
            "only the non-canonical spelling is stale: {stale:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn project_argument_reads_plists_and_systemd_units() {
        assert_eq!(
            plist_project_argument(&plist_with("/Users/op/workspace/chiebukuro")).as_deref(),
            Some("/Users/op/workspace/chiebukuro")
        );
        assert_eq!(
            plist_project_argument("ExecStart=/usr/bin/flywheel doorbell supervise chiebukuro\n")
                .as_deref(),
            Some("chiebukuro")
        );
        assert_eq!(plist_project_argument("<plist></plist>"), None);
    }

    /// The install site folds the project, so the unit's argv carries the
    /// slug too — not just the label.
    #[test]
    fn install_appends_the_canonical_project() {
        let src = include_str!("daemon.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let at = production
            .find("pub fn install_doorbell_supervisor(")
            .expect("install fn");
        let body = &production[at..];
        assert!(
            body.contains("let project = crate::doorbell::canonical_project(project);"),
            "install must canonicalise before naming the unit"
        );
        assert!(
            body.contains("doorbell_supervisor_key(project)"),
            "the unit slug goes through the one key"
        );
    }
}

#[cfg(test)]
mod retire_outcomes_tests {
    use super::*;

    struct FakePort {
        fail: Option<String>,
    }

    impl SupervisorPort for FakePort {
        fn bootout(&self, _label: &str) -> Result<(), String> {
            match &self.fail {
                None => Ok(()),
                Some(e) => Err(e.clone()),
            }
        }
    }

    fn temp_plist(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fw-retire-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plist = dir.join("dev.flywheel.doorbell.-users-op-workspace-chiebukuro.plist");
        std::fs::write(&plist, "<plist/>").unwrap();
        (dir, plist)
    }

    /// A successful bootout retires the unit and takes the plist with it.
    #[test]
    fn retire_ok_removes_the_plist() {
        let (dir, plist) = temp_plist("ok");
        let out = retire_stale_unit(&FakePort { fail: None }, "dev.flywheel.x", &plist);
        assert_eq!(out, RetireOutcome::Retired);
        assert!(!plist.exists(), "a retired unit's plist is gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// "already gone" is a success: the plist still goes.
    #[test]
    fn not_loaded_still_removes_the_plist() {
        let (dir, plist) = temp_plist("gone");
        let out = retire_stale_unit(
            &FakePort {
                fail: Some(
                    "Could not find service \"dev.flywheel.x\" in domain for user gui: 501".into(),
                ),
            },
            "dev.flywheel.x",
            &plist,
        );
        assert_eq!(out, RetireOutcome::NotLoaded);
        assert!(!plist.exists(), "a gone unit's plist is cleared");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// THE defect: a failed bootout must KEEP the plist, or the still-running
    /// supervisor becomes invisible to every later install (the finder reads
    /// plists). Mutation: removing the plist anyway reddens this.
    #[test]
    fn bootout_failure_keeps_the_plist_and_reports_the_reason() {
        let (dir, plist) = temp_plist("fail");
        let out = retire_stale_unit(
            &FakePort {
                fail: Some("Boot-out failed: 1: Operation not permitted".into()),
            },
            "dev.flywheel.x",
            &plist,
        );
        assert_eq!(
            out,
            RetireOutcome::BootoutFailed("Boot-out failed: 1: Operation not permitted".into())
        );
        assert!(
            plist.exists(),
            "the plist is the only index a later install has; keep it"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
