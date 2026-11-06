// governed-by: ADR-0012
//! Supervised always-on doorbell hosts (wave11-b): one launchd agent
//! (`dev.flywheel.doorbell.<ws>`) or systemd user unit
//! (`flywheel-doorbell-<ws>`) per armed workspace, each running
//! `flywheel __doorbell --project <p> --workspace <ws>` — the one
//! [`doorbell_rt::HostRunner`] loop, supervised so an idle subscribed pane
//! is woken by a done-mail with no live spawn process.
//!
//! Patterns reused from the `doorbell` daemon installer
//! (`doorbell-cli::lifecycle`): absolute binary paths, `KeepAlive` with
//! `SuccessfulExit=false`, 4096 fds, explicit HOME. `daemon::start`
//! installs + loads these; `daemon::stop` unloads them.
//!
//! Sandbox rules: unit files resolve under `$HOME` (overridable per test),
//! and the supervisor binary resolves via `PATH`, so deny-shims prove the
//! load attempt without touching the operator's launchd/systemd. Pass
//! `dry_run` to render + write without loading (shim logs stay empty).

use std::path::{Path, PathBuf};

/// Label prefix for supervised host jobs.
pub const HOST_LABEL_PREFIX: &str = "dev.flywheel.doorbell";

/// Systemd unit prefix (launchd uses [`HOST_LABEL_PREFIX`]); Linux only.
#[cfg(not(target_os = "macos"))]
pub const HOST_SERVICE_PREFIX: &str = "flywheel-doorbell";

/// Env flag that turns the `daemon start` ride-along into a plan print
/// (no writes, no loads). No CLI surface change — the flag is operator-only.
pub const DRY_RUN_ENV: &str = "FLYWHEEL_DOORBELL_HOST_DRY_RUN";

/// A supervised host: what to run, and where its unit file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUnit {
    /// `dev.flywheel.doorbell.<ws>` (macOS) or `flywheel-doorbell-<ws>`.
    pub label: String,
    /// Unit file path (plist or service).
    pub unit_path: PathBuf,
    /// Rendered unit body.
    pub body: String,
}

/// Inputs to render/install one supervised host.
#[derive(Debug, Clone)]
pub struct HostSpec {
    /// Absolute path to the `flywheel` binary.
    pub exe: PathBuf,
    pub project: String,
    pub workspace: String,
    /// Flywheel state dir (XDG state; the job's logs + env).
    pub state_dir: PathBuf,
    /// Daemon URL the child uses (operator env wins, default documented port).
    pub toron_mcp_url: String,
    /// HOME to run as (launchd gives the job none).
    pub home: PathBuf,
    /// Operator-configured backend (`FLYWHEEL_BACKEND`) to pin the child to.
    /// `None` (the default) omits it so the child auto-resolves rex from
    /// its own PATH — a stale pin would fight the operator's later change.
    pub flywheel_backend: Option<String>,
    /// Absolute bin dirs for the unit's PATH, resolved at install time
    /// (exe dir + resolved plane binaries + operator PATH minus shims).
    /// Stored, not recomputed, so the render is pure and the sandbox pins it.
    pub path_dirs: Vec<PathBuf>,
}

/// Report for one install/unload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    pub label: String,
    pub unit_path: PathBuf,
    /// True when the supervisor accepted the load (false on dry-run).
    pub loaded: bool,
}

/// Sanitize a workspace name into a label fragment.
fn slug(workspace: &str) -> String {
    workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Job label for a workspace.
pub fn host_label(workspace: &str) -> String {
    format!("{HOST_LABEL_PREFIX}.{}", slug(workspace))
}

/// Systemd service name for a workspace (Linux only; macOS uses [`host_label`]).
#[cfg(not(target_os = "macos"))]
pub fn host_service(workspace: &str) -> String {
    format!("{}-{}.service", HOST_SERVICE_PREFIX, slug(workspace))
}

/// Unit file path for a workspace under `home`.
pub fn unit_path(home: &Path, workspace: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", host_label(workspace)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config")
            .join("systemd")
            .join("user")
            .join(host_service(workspace))
    }
}

/// Directories on the unit's PATH, resolved at install time (wave15-b):
/// the operator PATH if available, else the absolute dirs containing the
/// binaries the host shells (`flywheel`, plus `rex`/`hc`/`toron` when
/// found). Mise shim dirs are skipped — a shim without the operator's cwd
/// and version pin dies under launchd. Pure for tests: `operator_path` is
/// the ambient PATH (or `None`), `exe` the flywheel binary, `bins` the
/// already-resolved plane binary paths.
pub fn unit_path_dirs(
    operator_path: Option<&std::ffi::OsStr>,
    exe: &Path,
    bins: &[Option<PathBuf>],
) -> Vec<PathBuf> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    let mut push = |d: PathBuf| {
        if seen.insert(d.clone()) {
            dirs.push(d);
        }
    };
    if let Some(dir) = exe.parent() {
        push(dir.to_path_buf());
    }
    for b in bins.iter().flatten() {
        if let Some(dir) = b.parent() {
            push(dir.to_path_buf());
        }
    }
    if let Some(path_env) = operator_path {
        for dir in std::env::split_paths(path_env) {
            if is_mise_shim_dir(&dir) {
                continue;
            }
            push(dir);
        }
    }
    for sys in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        push(PathBuf::from(sys));
    }
    dirs
}

/// `.../mise/shims` is a dispatcher dir, not a real bin directory — a shim
/// without the operator's cwd and version pin dies under launchd.
fn is_mise_shim_dir(dir: &Path) -> bool {
    dir.components()
        .map(|c| c.as_os_str())
        .collect::<Vec<_>>()
        .windows(2)
        .any(|w| w[0] == "mise" && w[1] == "shims")
}

/// Render the supervisor unit for one host. Pure: the sandbox pins this.
pub fn render_host_unit(spec: &HostSpec) -> HostUnit {
    let label = host_label(&spec.workspace);
    let exe = spec.exe.display().to_string();
    let state = spec.state_dir.display().to_string();
    let home = spec.home.display().to_string();
    let log_out = spec
        .state_dir
        .join(format!("doorbell-host-{}.out.log", slug(&spec.workspace)))
        .display()
        .to_string();
    let log_err = spec
        .state_dir
        .join(format!("doorbell-host-{}.err.log", slug(&spec.workspace)))
        .display()
        .to_string();
    let path_value = spec
        .path_dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let backend_env = spec
        .flywheel_backend
        .as_deref()
        .map(|b| format!("        <key>FLYWHEEL_BACKEND</key>\n        <string>{b}</string>\n"))
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>__doorbell</string>
        <string>--project</string>
        <string>{project}</string>
        <string>--workspace</string>
        <string>{workspace}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>WorkingDirectory</key>
    <string>{home}</string>
    <key>StandardOutPath</key>
    <string>{log_out}</string>
    <key>StandardErrorPath</key>
    <string>{log_err}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home}</string>
        <key>PATH</key>
        <string>{path_value}</string>
        <key>TORON_MCP_URL</key>
        <string>{toron_mcp_url}</string>
        <key>DOORBELL_STATE_DIR</key>
        <string>{state}</string>
{backend_env}    </dict>
    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>4096</integer>
    </dict>
</dict>
</plist>
"#,
        label = label,
        exe = exe,
        project = spec.project,
        workspace = spec.workspace,
        home = home,
        log_out = log_out,
        log_err = log_err,
        toron_mcp_url = spec.toron_mcp_url,
        state = state,
        path_value = path_value,
        backend_env = backend_env,
    );
    #[cfg(not(target_os = "macos"))]
    let args = format!(
        "__doorbell --project {} --workspace {}",
        spec.project, spec.workspace
    );
    #[cfg(not(target_os = "macos"))]
    let body = format!(
        "[Unit]\nDescription=flywheel doorbell host {project}/{workspace} (always-on wake)\nAfter=network-online.target\n\n[Service]\nType=simple\nExecStart={exe} {args}\nWorkingDirectory={home}\nRestart=on-failure\nRestartSec=30\nLimitNOFILE=4096\nEnvironment=HOME={home}\nEnvironment=PATH={path_value}\nEnvironment=TORON_MCP_URL={toron_mcp_url}\nEnvironment=DOORBELL_STATE_DIR={state}{backend_systemd}\nStandardOutput=append:{log_out}\nStandardError=append:{log_err}\n\n[Install]\nWantedBy=default.target\n",
        project = spec.project,
        workspace = spec.workspace,
        exe = exe,
        args = args,
        home = home,
        toron_mcp_url = spec.toron_mcp_url,
        state = state,
        log_out = log_out,
        log_err = log_err,
        path_value = path_value,
        backend_systemd = spec
            .flywheel_backend
            .as_deref()
            .map(|b| format!("\nEnvironment=FLYWHEEL_BACKEND={b}"))
            .unwrap_or_default(),
    );
    #[cfg(not(target_os = "macos"))]
    let label = host_service(&spec.workspace);
    let unit_path = unit_path(&spec.home, &spec.workspace);
    HostUnit {
        label,
        unit_path,
        body,
    }
}

/// Resolve a shell command the way the sandbox expects: `name` via `PATH`
/// (deny-shims first), so tests prove the load attempt without touching the
/// operator's supervisor.
fn run_supervisor(name: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(name).args(args).output()
}

/// Install (+ load, unless `dry_run`) one supervised host. Idempotent:
/// rewrites the unit when the render drifts, then (re)loads it.
/// Load failure is loud (names the manual command) — never a silent skip,
/// or the operator believes wake is always-on while nothing hosts it.
pub fn install_host(spec: &HostSpec, dry_run: bool) -> anyhow::Result<HostAction> {
    use anyhow::Context;
    let unit = render_host_unit(spec);
    if dry_run {
        println!("--- {} ---", unit.unit_path.display());
        print!("{}", unit.body);
        return Ok(HostAction {
            label: unit.label,
            unit_path: unit.unit_path,
            loaded: false,
        });
    }
    if let Some(parent) = unit.unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create unit dir {}", parent.display()))?;
    }
    std::fs::write(&unit.unit_path, &unit.body)
        .with_context(|| format!("write unit {}", unit.unit_path.display()))?;
    load_unit(&unit)?;
    Ok(HostAction {
        label: unit.label,
        unit_path: unit.unit_path,
        loaded: true,
    })
}

/// Load (or reload) an installed unit through the supervisor.
fn load_unit(unit: &HostUnit) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let uid = users_uid();
        let domain = format!("gui/{uid}/{}", unit.label);
        let target = unit.unit_path.display().to_string();
        // cheap bootstrap (fresh) → bootout stale → bootstrap fresh → kickstart.
        let _ = run_supervisor("launchctl", &["bootstrap", &format!("gui/{uid}"), &target]);
        let _ = run_supervisor("launchctl", &["bootout", &domain]);
        let out = run_supervisor("launchctl", &["bootstrap", &format!("gui/{uid}"), &target])
            .map_err(|e| anyhow::anyhow!("launchctl bootstrap: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl bootstrap failed: {} (unit at {}; load it by hand: launchctl bootstrap gui/{uid} {})",
                String::from_utf8_lossy(&out.stderr),
                unit.unit_path.display(),
                target
            );
        }
        let out = run_supervisor("launchctl", &["kickstart", "-k", &domain])
            .map_err(|e| anyhow::anyhow!("launchctl kickstart: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "launchctl kickstart failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let svc = &unit.label;
        let out = run_supervisor("systemctl", &["--user", "daemon-reload"])
            .map_err(|e| anyhow::anyhow!("systemctl daemon-reload: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl daemon-reload failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let out = run_supervisor("systemctl", &["--user", "enable", "--now", svc])
            .map_err(|e| anyhow::anyhow!("systemctl enable: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl enable --now {svc} failed: {} (unit at {}; enable it by hand: systemctl --user enable --now {svc})",
                String::from_utf8_lossy(&out.stderr),
                unit.unit_path.display()
            );
        }
        Ok(())
    }
}

/// Current uid via the daemon helper (`id -u`; keeps the binary unsafe-free).
#[cfg(target_os = "macos")]
fn users_uid() -> String {
    crate::daemon::current_uid()
}

/// Unload one supervised host and remove its unit file. Best-effort per
/// unit (stop must keep going); failures are loud, never silent.
pub fn unload_host(home: &Path, workspace: &str) -> anyhow::Result<HostAction> {
    let label = host_label(workspace);
    #[cfg(target_os = "macos")]
    let svc_label = label.clone();
    #[cfg(not(target_os = "macos"))]
    let svc_label = host_service(workspace);
    let path = unit_path(home, workspace);
    #[cfg(target_os = "macos")]
    {
        let uid = users_uid();
        let out = run_supervisor("launchctl", &["bootout", &format!("gui/{uid}/{svc_label}")]);
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => eprintln!(
                "doorbell host {label}: bootout noted: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("doorbell host {label}: bootout failed: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        match run_supervisor("systemctl", &["--user", "disable", "--now", &svc_label]) {
            Ok(o) if o.status.success() => {}
            Ok(o) => eprintln!(
                "doorbell host {label}: disable noted: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("doorbell host {label}: disable failed: {e}"),
        }
    }
    if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    Ok(HostAction {
        label,
        unit_path: path,
        loaded: false,
    })
}

/// Whether the supervisor reports this host loaded.
pub fn host_loaded(workspace: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let uid = users_uid();
        let label = host_label(workspace);
        run_supervisor("launchctl", &["print", &format!("gui/{uid}/{label}")])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let svc = host_service(workspace);
        run_supervisor("systemctl", &["--user", "is-active", "--quiet", &svc])
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
/// Install + load a supervised host for every armed workspace of the
/// project detected from the cwd (mirrors the cron host). No project in
/// scope: loud skip (hosts cannot be armed without one). Returns the
/// per-workspace actions. `FLYWHEEL_DOORBELL_HOST_DRY_RUN=1` prints the
/// plan instead (no writes, no loads).
pub fn ensure_armed_hosts(cfg: &crate::config::Config) -> anyhow::Result<Vec<HostAction>> {
    use anyhow::Context;
    let dry_run = std::env::var(DRY_RUN_ENV).as_deref() == Ok("1");
    let Some(project) = crate::cron::detect_project(cfg, None) else {
        eprintln!("doorbell hosts: no project in scope — skipping supervised hosts");
        return Ok(Vec::new());
    };
    let workspaces = crate::cmd::spawn::doorbell_project_workspaces(&project);
    if workspaces.is_empty() {
        eprintln!("doorbell hosts: {project} has no swarm workspace yet — attach after spawn");
        return Ok(Vec::new());
    }
    let exe = std::env::current_exe().context("current_exe")?;
    let home = home_dir()?;
    // wave15-b: resolve the plane binaries NOW, while the operator PATH
    // is available — the unit must never rely on launchd's minimal PATH.
    let bins = [
        doorbell_rt::binpath::resolve_cli("rex"),
        doorbell_rt::binpath::resolve_cli("hc"),
        doorbell_rt::binpath::resolve_cli("toron"),
    ];
    let path_dirs = unit_path_dirs(std::env::var_os("PATH").as_deref(), &exe, &bins);
    let flywheel_backend = std::env::var("FLYWHEEL_BACKEND")
        .ok()
        .filter(|s| !s.is_empty());
    let mut done = Vec::new();
    for ws in &workspaces {
        let spec = HostSpec {
            exe: exe.clone(),
            project: project.clone(),
            workspace: ws.clone(),
            state_dir: crate::config::state_dir(),
            toron_mcp_url: toron_mcp_url(),
            home: home.clone(),
            flywheel_backend: flywheel_backend.clone(),
            path_dirs: path_dirs.clone(),
        };
        let action = install_host(&spec, dry_run)
            .with_context(|| format!("supervised host {project}/{ws}"))?;
        println!(
            "doorbell host {project}/{ws}: {} (loaded: {})",
            action.unit_path.display(),
            action.loaded
        );
        done.push(action);
    }
    Ok(done)
}

/// Unload every supervised host unit found under the user unit dir
/// (project-agnostic: stop owns the whole fleet's hosts). Best-effort per
/// unit; failures are loud, never silent.
pub fn unload_all_hosts() -> anyhow::Result<Vec<HostAction>> {
    let home = home_dir()?;
    let mut done = Vec::new();
    for entry in unit_dir(&home)
        .read_dir()
        .map(|r| r.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(ws) = unslug(&name) else { continue };
        match unload_host(&home, &ws) {
            Ok(action) => {
                println!("doorbell host unloaded: {}", action.label);
                done.push(action);
            }
            Err(e) => eprintln!("doorbell host {name}: unload failed: {e:#}"),
        }
    }
    Ok(done)
}

/// One-line status for every installed supervised host.
pub fn hosts_status() -> Vec<(String, bool)> {
    let Ok(home) = home_dir() else {
        return Vec::new();
    };
    unit_dir(&home)
        .read_dir()
        .map(|r| r.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            unslug(&name).map(|ws| {
                let loaded = host_loaded(&ws);
                (ws, loaded)
            })
        })
        .collect()
}

/// Strip a unit file name back to its workspace slug.
fn unslug(file_name: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        file_name
            .strip_prefix(&format!("{HOST_LABEL_PREFIX}."))
            .and_then(|s| s.strip_suffix(".plist"))
            .map(str::to_string)
    }
    #[cfg(not(target_os = "macos"))]
    {
        file_name
            .strip_prefix(&format!("{HOST_SERVICE_PREFIX}-"))
            .and_then(|s| s.strip_suffix(".service"))
            .map(str::to_string)
    }
}

/// Unit dir for the current user.
fn unit_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("LaunchAgents")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config").join("systemd").join("user")
    }
}

/// HOME for unit files + job env (launchd gives the job none).
fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("HOME is unset or relative — cannot place host units"))
}

/// Daemon URL the host child uses (operator env wins; default is toron's
/// documented daemon port).
fn toron_mcp_url() -> String {
    std::env::var("TORON_MCP_URL").unwrap_or_else(|_| "http://127.0.0.1:18080/mcp".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> HostSpec {
        HostSpec {
            exe: PathBuf::from("/opt/flywheel/bin/flywheel"),
            project: "flywheel".into(),
            workspace: "proj-oc".into(),
            state_dir: PathBuf::from("/tmp/fw-state"),
            toron_mcp_url: "http://127.0.0.1:18080/mcp".into(),
            home: PathBuf::from("/tmp/fw-home"),
            flywheel_backend: None,
            path_dirs: vec![
                PathBuf::from("/opt/flywheel/bin"),
                PathBuf::from("/opt/rex/bin"),
                PathBuf::from("/usr/bin"),
            ],
        }
    }

    #[test]
    fn labels_slug_the_workspace() {
        assert_eq!(
            host_label("proj-oc"),
            format!("{HOST_LABEL_PREFIX}.proj-oc")
        );
        assert_eq!(slug("a/b c"), "a-b-c");
    }

    #[test]
    fn render_pins_the_supervision_contract() {
        let unit = render_host_unit(&spec());
        // absolute binary + embedded entry + project/workspace routing
        assert!(unit.body.contains("/opt/flywheel/bin/flywheel"));
        assert!(unit.body.contains("__doorbell"));
        assert!(unit.body.contains("--project"));
        assert!(unit.body.contains("proj-oc"));
        // supervision halves: restart on crash, backoff, fds, HOME
        assert!(unit.body.contains("SuccessfulExit") || unit.body.contains("Restart=on-failure"));
        assert!(unit.body.contains("4096"));
        assert!(unit.body.contains("HOME"));
        assert!(unit.body.contains("TORON_MCP_URL"));
        // no second loop: the unit runs the one HostRunner entry, nothing else
        assert!(!unit.body.contains("serve"));
        // wave15-b: the unit PATH resolves the plane binaries at install
        // time — no bare-name lookup under launchd's minimal PATH.
        assert!(unit.body.contains("/opt/flywheel/bin"));
        assert!(unit.body.contains("/opt/rex/bin"));
        // no backend pin unless the operator configured one: auto resolves
        // rex from the unit PATH.
        assert!(!unit.body.contains("FLYWHEEL_BACKEND"));
        let mut pinned = spec();
        pinned.flywheel_backend = Some("hc".into());
        assert!(render_host_unit(&pinned).body.contains("FLYWHEEL_BACKEND"));
        // restart rule (documented choice): SuccessfulExit=false stays —
        // deliberate retires (pid gone, workspace gone, daemon stop) must
        // stay dead, not respawn-loop. Only the stale-binary retire exits
        // nonzero, and launchd restarts exactly that one. `daemon start`
        // kickstarts loaded units so a fresh binary takes over at once
        // (proven by the shim-log test on load_unit, not the render).
        assert!(
            unit.body.contains("KeepAlive") || unit.body.contains("Restart=on-failure"),
            "restart on crash, never on clean exit"
        );
    }

    #[test]
    fn unit_path_dirs_prefers_real_bins_over_shims() {
        use std::ffi::OsString;
        let exe = PathBuf::from("/opt/flywheel/bin/flywheel");
        let bins = vec![Some(PathBuf::from("/opt/rex/bin/rex")), None];
        let op: OsString = "/usr/bin:/home/op/.local/share/mise/shims:/home/op/bin".into();
        let dirs = unit_path_dirs(Some(&op), &exe, &bins);
        let names: Vec<_> = dirs.iter().map(|d| d.display().to_string()).collect();
        assert_eq!(names[0], "/opt/flywheel/bin", "exe dir first");
        assert!(names.contains(&"/opt/rex/bin".to_string()));
        assert!(
            !names.iter().any(|d| d.contains("mise/shims")),
            "shims never ride the unit PATH: {names:?}"
        );
        assert!(names.contains(&"/home/op/bin".to_string()));
        assert!(names.contains(&"/usr/bin".to_string()));
        // no operator PATH: resolved dirs + system fallback only.
        let dirs = unit_path_dirs(None, &exe, &bins);
        assert!(
            dirs.iter()
                .all(|d| !d.display().to_string().contains("mise"))
        );
        assert!(dirs.contains(&PathBuf::from("/usr/bin")));
    }

    /// Sandbox install: temp HOME, deny-shim supervisor on PATH, dry run —
    /// files land, no supervisor is contacted (shim log stays absent).
    #[test]
    fn dry_run_writes_nothing_and_contacts_nobody() {
        let _g = crate::config::lock_env();
        let (root, home, log, prev_path) = shimmed_sandbox("dry");
        let mut s = spec();
        s.home = home.clone();
        s.state_dir = root.join("state");
        let action = install_host(&s, true).expect("dry run installs the plan");
        assert!(!action.loaded);
        assert!(
            action.unit_path.starts_with(&home),
            "unit lands under temp HOME"
        );
        assert!(!log.exists(), "dry run must not contact the supervisor");
        restore_path_and_clean(root, prev_path);
    }

    /// Sandbox supervisor: temp root with a deny-shim `launchctl` +
    /// `systemctl` first on PATH (log argv, exit 1 — never the operator's
    /// supervisor). Returns (root, home, shim log, previous PATH).
    /// Callers restore PATH and remove the root when done.
    fn shimmed_sandbox(tag: &str) -> (PathBuf, PathBuf, PathBuf, Option<std::ffi::OsString>) {
        let root = std::env::temp_dir().join(format!("fw-host-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let shim = root.join("bin");
        std::fs::create_dir_all(&shim).unwrap();
        let log = root.join("shim.log");
        for b in ["launchctl", "systemctl"] {
            std::fs::write(
                shim.join(b),
                format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", log.display()),
            )
            .unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for b in ["launchctl", "systemctl"] {
                let p = shim.join(b);
                let mut perms = std::fs::metadata(&p).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&p, perms).unwrap();
            }
        }
        let prev_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    shim.display(),
                    prev_path.clone().unwrap_or_default().into_string().unwrap()
                ),
            );
        }
        (root, home, log, prev_path)
    }

    fn restore_path_and_clean(root: PathBuf, prev_path: Option<std::ffi::OsString>) {
        if let Some(p) = prev_path {
            unsafe { std::env::set_var("PATH", p) };
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Load attempt with a deny-shim supervisor: loud failure, unit file
    /// still written, shim log proves the bootstrap was attempted.
    #[test]
    fn load_attempt_hits_the_shim_and_fails_loud() {
        let _g = crate::config::lock_env();
        let (root, home, log, prev_path) = shimmed_sandbox("load");
        let mut s = spec();
        s.home = home.clone();
        s.state_dir = root.join("state");
        let err = install_host(&s, false).expect_err("deny-shim load must fail loud");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("launchctl") || msg.contains("systemctl"),
            "failure names the supervisor: {msg}"
        );
        assert!(
            unit_path(&home, "proj-oc").is_file(),
            "unit file is written before the load attempt"
        );
        let logged = std::fs::read_to_string(&log).expect("shim log proves the attempt");
        assert!(
            logged.contains("bootstrap") || logged.contains("daemon-reload"),
            "shim saw the load: {logged}"
        );
        // On a FAILED load there is nothing to kickstart: the failure is
        // loud and no stale unit is kicked.
        let restarted = logged
            .lines()
            .any(|l| l.starts_with("kickstart") || l.contains("--now"));
        assert!(
            !restarted,
            "a failed load must not kick a unit that never loaded: {logged}"
        );
        restore_path_and_clean(root, prev_path);
    }

    /// `daemon start` on an already-loaded unit kickstarts it: a succeeding
    /// shim records the full bootstrap/bootout/bootstrap/kickstart order.
    #[test]
    fn load_success_kickstarts_the_fresh_unit() {
        let _g = crate::config::lock_env();
        let root = std::env::temp_dir().join(format!("fw-host-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let shim = root.join("bin");
        std::fs::create_dir_all(&shim).unwrap();
        let log = root.join("shim.log");
        for b in ["launchctl", "systemctl"] {
            std::fs::write(
                shim.join(b),
                format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display()),
            )
            .unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for b in ["launchctl", "systemctl"] {
                let p = shim.join(b);
                let mut perms = std::fs::metadata(&p).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&p, perms).unwrap();
            }
        }
        let prev_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var(
                "PATH",
                format!(
                    "{}:{}",
                    shim.display(),
                    prev_path.clone().unwrap_or_default().into_string().unwrap()
                ),
            );
        }
        let mut s = spec();
        s.home = home.clone();
        s.state_dir = root.join("state");
        let action = install_host(&s, false).expect("load succeeds with a working supervisor");
        assert!(action.loaded);
        let logged = std::fs::read_to_string(&log).unwrap();
        let verbs: Vec<&str> = logged
            .lines()
            .map(|l| l.split_whitespace().next().unwrap_or(""))
            .collect();
        assert_eq!(
            verbs,
            vec!["bootstrap", "bootout", "bootstrap", "kickstart"],
            "load order reloads the fresh unit and kickstarts it: {logged}"
        );
        restore_path_and_clean(root, prev_path);
    }

    /// Sandbox host loop (wave11-b acceptance core): temp HOME/XDG, sandbox
    /// toron data dir, no live ports, no hc socket. An idle subscribed pin
    /// gets a kind-9 wake through the REAL Brain + FileHost; with no pane
    /// the wake is journaled (digest), never silently dropped.
    #[tokio::test]
    async fn sandbox_host_records_an_idle_pins_wake() {
        use doorbell_rt::{
            Actor, Brain, BrainConfig, BrainMsg, Budget, FileHost, FileHostConfig, FileStore,
            Filter, Identity, NoTextSink, Scope, SinkKind, SinkManager, Subscription,
            SubscriptionStore,
        };

        let _g = crate::config::lock_env();
        let root = std::env::temp_dir().join(format!("fw-host-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        let data = root.join("data");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        // sandbox env: everything resolves under the temp root
        let prev_mcp_url = std::env::var_os("TORON_MCP_URL");
        let prev_data_dir = std::env::var_os("TORON_MCP_DATA_DIR");
        let prev_xdg = std::env::var_os("XDG_STATE_HOME");
        unsafe {
            std::env::remove_var("TORON_MCP_URL");
            std::env::set_var("TORON_MCP_DATA_DIR", &data);
            std::env::set_var("XDG_STATE_HOME", &state);
        }

        let project = "sandbox-proj";
        let workspace = "sandbox-ws";
        let pin = "SandboxPin";
        // spawn registry: the reducer index resolves p-tags through it
        let reg_dir = state.join("agents");
        std::fs::create_dir_all(&reg_dir).unwrap();
        let reg_row = serde_json::json!([{
            "pane_id": "sandbox-pane-0",
            "name": "sandbox-agent",
            "mail_identity": pin,
        }]);
        let reg_path = reg_dir.join(format!("{project}.json"));
        std::fs::write(&reg_path, serde_json::to_string(&reg_row).unwrap()).unwrap();
        // idle + subscribed (same-workspace scope, hc sink, local actor)
        let mut store = SubscriptionStore::open(&state);
        store
            .subscribe(
                Actor::local_process(),
                Subscription {
                    id: format!("{pin}-hc"),
                    identity: Identity::Pin(pin.to_string()),
                    scope: Scope::One(workspace.to_string()),
                    filter: Filter::default(),
                    sink: SinkKind::Hc,
                    budget: Budget::default_530(),
                    project: Some(project.to_string()),
                    endpoint: None,
                },
                workspace,
            )
            .expect("sandbox subscribe");
        let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
        let mut fcfg =
            FileHostConfig::new(state.clone(), project.to_string(), workspace.to_string());
        fcfg.registry_file = Some(reg_path);
        // `true` exits 0 with no output: toron/hc shell-outs parse to
        // nothing (established file_host test pattern — no live processes).
        fcfg.toron_bin = "true".into();
        fcfg.plane_bin = "true".into();
        let host = FileHost::new(fcfg);
        let mut sinks = SinkManager::new(tx.clone());
        sinks.add_sink_with_factory("hc", Box::new(NoTextSink::new()), || {
            Box::new(NoTextSink::new())
        });
        let journal = FileStore::new(&state);
        let line = serde_json::json!({
            "kind": 9, "id": "sandbox-e1", "pubkey": "ab",
            "tags": [["p", pin]], "content": "done-mail wake", "created_at": 1
        })
        .to_string();
        let brain = Brain::new(BrainConfig::new(workspace), host, journal, sinks);
        let handle = tokio::spawn(async move { brain.run(rx).await });
        tx.send(BrainMsg::PlaneStatus {
            pin: pin.to_string(),
            status: "idle".to_string(),
        })
        .await
        .unwrap();
        tx.send(BrainMsg::Sweep { now: 1_700_000_000 })
            .await
            .unwrap();
        tx.send(BrainMsg::Watch(line)).await.unwrap();
        tx.send(BrainMsg::Shutdown).await.unwrap();
        drop(tx);
        let report = handle.await.expect("brain joins");
        let _ = report.stats;

        let journal_text = std::fs::read_to_string(FileStore::new(&state).journal_file(workspace))
            .expect("brain journals to the sandbox store");
        // Prompt (sink attempt) or Digest (no route, journaled) — either is
        // the recorded wake. A bare status row does not satisfy this: the
        // kind-9 Watch must have been dispatched through the host. (serde
        // Map orders keys alphabetically — match per key, not per object.)
        let wake_row = journal_text.lines().any(|l| {
            l.contains("SandboxPin") && (l.contains("\"Prompt\"") || l.contains("\"Digest\""))
        });
        assert!(
            wake_row,
            "the idle pin's kind-9 wake is delivered or digest-recorded, got:\n{journal_text}"
        );

        // restore env
        unsafe {
            match prev_mcp_url {
                Some(v) => std::env::set_var("TORON_MCP_URL", v),
                None => std::env::remove_var("TORON_MCP_URL"),
            }
            match prev_data_dir {
                Some(v) => std::env::set_var("TORON_MCP_DATA_DIR", v),
                None => std::env::remove_var("TORON_MCP_DATA_DIR"),
            }
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
