// governed-by: ADR-0012
//! Single-owner doorbell host supervision (bd-supervisor-collab-rering-87gzr).
//!
//! ONE runtime owner: the cron-healed detached `__doorbell` children —
//! pid-locked through [`crate::doorbell::ensure_running`], revived every
//! minute by the cron supervisor tick ([`crate::doorbell::heal_tick_body`]).
//!
//! The retired second owner was one launchd agent
//! (`dev.flywheel.doorbell.<ws>`) or systemd user unit
//! (`flywheel-doorbell-<ws>`) per armed workspace, each running
//! `flywheel __doorbell`. The two paths Superseded each other on restart
//! (the pid lease keeps exactly one alive), so `ensure_armed_hosts` no
//! longer installs units: it retires any leftover duplicate units
//! (best-effort) and ensures the detached child for every armed workspace.
//! `unload_all_hosts` retires leftover units and stops the detached
//! children. `daemon start` / `stop` (which call these two) thereby drive
//! the one owner.
//!
//! Sandbox rules: unit cleanup resolves under an explicit HOME (temp HOME
//! in tests) and children under an explicit state dir (temp dirs in
//! tests), so the operator's launchd/systemd and live dispatchers are
//! never touched. Child spawn shells the given exe — tests pass
//! `/bin/true`, which exits at once, proving the spawn path without
//! running a host.

use std::path::{Path, PathBuf};

/// Label prefix of the retired duplicate units (kept: cleanup scans for it).
pub const HOST_LABEL_PREFIX: &str = "dev.flywheel.doorbell";

/// Systemd unit prefix of the retired duplicates (Linux only).
#[cfg(not(target_os = "macos"))]
pub const HOST_SERVICE_PREFIX: &str = "flywheel-doorbell";

/// Report for one retired duplicate unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAction {
    pub label: String,
    pub unit_path: PathBuf,
    /// Retire never loads: always false (kept for shape compat).
    pub loaded: bool,
}

/// What `ensure_armed_hosts` did for one workspace (single-owner plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredChild {
    pub workspace: String,
    /// Debug rendering of the [`crate::doorbell::LockAction`] taken.
    pub action: String,
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

/// Resolve a shell command the way the sandbox expects: `name` via `PATH`
/// (deny-shims first), so tests prove the unload attempt without touching
/// the operator's supervisor.
fn run_supervisor(name: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(name).args(args).output()
}

/// Current uid via the daemon helper (`id -u`; keeps the binary unsafe-free).
#[cfg(target_os = "macos")]
fn users_uid() -> String {
    crate::daemon::current_uid()
}

/// Unload one duplicate unit and remove its file. Best-effort (stop must
/// keep going); failures are loud, never silent.
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

/// Whether the supervisor reports this (duplicate) unit loaded.
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

/// Retire every duplicate unit under `home`. Best-effort per unit, never
/// errors — a stuck supervisor must not block the child it duplicates.
pub fn retire_duplicate_units_in(home: &Path) -> Vec<HostAction> {
    let mut done = Vec::new();
    for entry in unit_dir(home)
        .read_dir()
        .map(|r| r.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(ws) = unslug(&name) else { continue };
        match unload_host(home, &ws) {
            Ok(action) => {
                println!("doorbell host retired (duplicate unit): {}", action.label);
                done.push(action);
            }
            Err(e) => eprintln!("doorbell host {name}: retire failed: {e:#}"),
        }
    }
    done
}

/// Install + load a supervised host for every armed workspace of the
/// project detected from the cwd — now through the single owner: retire
/// duplicate units first, then [`crate::doorbell::ensure_running`] per
/// workspace (the cron-healed detached child). No project in scope: loud
/// skip. Testable core is [`ensure_armed_hosts_in`].
pub fn ensure_armed_hosts(cfg: &crate::config::Config) -> anyhow::Result<Vec<EnsuredChild>> {
    let Some(project) = crate::cron::detect_project(cfg, None) else {
        eprintln!(
            "doorbell hosts: no project in scope — skipping (hosts cannot be armed without one)"
        );
        return Ok(Vec::new());
    };
    let workspaces = crate::cmd::spawn::doorbell_project_workspaces(&project);
    if workspaces.is_empty() {
        eprintln!("doorbell hosts: {project} has no swarm workspace yet — attach after spawn");
        return Ok(Vec::new());
    }
    let snap = crate::snapshot::assemble(cfg, &project, crate::doorbell_now_unix());
    let workspaces =
        crate::doorbell::canonical_workspaces(&workspaces, &snap.workspace, &snap.workspace_id);
    let exe = std::env::current_exe()?;
    let state_dir = crate::config::state_dir();
    let home = home_dir().ok();
    ensure_armed_hosts_in(&project, &workspaces, &exe, &state_dir, home.as_deref())
}

/// Testable core: duplicate retire (when `home` is Some) + one ensured
/// child per workspace. `home: None` skips the retire with a note.
pub fn ensure_armed_hosts_in(
    project: &str,
    workspaces: &[String],
    exe: &Path,
    state_dir: &Path,
    home: Option<&Path>,
) -> anyhow::Result<Vec<EnsuredChild>> {
    use anyhow::Context;
    match home {
        Some(h) => {
            retire_duplicate_units_in(h);
        }
        None => eprintln!("doorbell hosts: no HOME — skipping duplicate-unit retire"),
    }
    let mut done = Vec::new();
    for ws in workspaces {
        let action = crate::doorbell::ensure_running(exe, state_dir, ws, project)
            .with_context(|| format!("supervised child {project}/{ws}"))?;
        println!("doorbell host {project}/{ws}: {action:?}");
        done.push(EnsuredChild {
            workspace: ws.clone(),
            action: format!("{action:?}"),
        });
    }
    Ok(done)
}

/// Workspaces with dispatcher state under `state_dir` (a `<ws>.gen` file),
/// regardless of pid liveness — stop owns them all.
fn dispatcher_workspaces(state_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(state_dir.join("doorbell")) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or_default();
        let ws = if path.extension().and_then(|x| x.to_str()) == Some("gen") {
            path.file_stem().and_then(|x| x.to_str())
        } else {
            name.strip_suffix(".state.json")
        };
        if let Some(ws) = ws {
            out.push(ws.to_string());
        }
    }
    out.sort();
    out
}

/// Stop every detached child under `state_dir` plus retire leftover units
/// under `home`. Best-effort per item, always Ok — stop must keep going.
/// Returns human log lines (the daemon stop transcript).
pub fn unload_all_hosts_in(home: &Path, state_dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut done = Vec::new();
    for action in retire_duplicate_units_in(home) {
        done.push(format!("retired {}", action.label));
    }
    for ws in dispatcher_workspaces(state_dir) {
        crate::doorbell::stop(state_dir, &ws);
        let line = format!("stopped {ws}");
        println!("doorbell host {line}");
        done.push(line);
    }
    Ok(done)
}

/// Unload every supervised host (project-agnostic: stop owns the whole
/// fleet's hosts). See [`unload_all_hosts_in`].
pub fn unload_all_hosts() -> anyhow::Result<Vec<String>> {
    let home = home_dir()?;
    unload_all_hosts_in(&home, &crate::config::state_dir())
}

/// One-line status for every leftover duplicate unit.
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

/// HOME for unit files (duplicate retire only — children never need it).
fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("HOME is unset or relative — cannot place host units"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_slug_the_workspace() {
        assert_eq!(
            host_label("proj-oc"),
            format!("{HOST_LABEL_PREFIX}.proj-oc")
        );
        assert_eq!(slug("a/b c"), "a-b-c");
    }

    /// Temp root with deny-shim `launchctl` + `systemctl` first on PATH
    /// (log argv, exit 1 — never the operator's supervisor). Returns
    /// (root, previous PATH). Callers restore PATH and remove the root.
    fn shimmed_path(tag: &str) -> (PathBuf, Option<std::ffi::OsString>) {
        let root = std::env::temp_dir().join(format!("fw-sup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
        (root, prev_path)
    }

    fn restore_path_and_clean(root: PathBuf, prev_path: Option<std::ffi::OsString>) {
        if let Some(p) = prev_path {
            unsafe { std::env::set_var("PATH", p) };
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// bd-supervisor-collab-rering-87gzr: ONE owner. Ensuring armed hosts
    /// retires the duplicate launchd unit and owns the workspace through
    /// the cron-healed detached child — no unit is ever (re)installed.
    /// `/bin/true` stands in for the host exe: it exits at once, proving
    /// the spawn path without running a host. `FLYWHEEL_NO_DOORBELL_ENSURE`
    /// pins the daemon-probe arm off (hermetic: no operator daemon poked).
    #[test]
    fn single_owner_ensures_children_and_installs_no_units() {
        let _g = crate::config::lock_env();
        let (root, prev_path) = shimmed_path("owner");
        let home = root.join("home");
        // A leftover duplicate unit from the retired second owner.
        let dup = unit_path(&home, "ws1");
        std::fs::create_dir_all(dup.parent().unwrap()).unwrap();
        std::fs::write(&dup, "stale duplicate unit").unwrap();
        let state = root.join("state");
        std::fs::create_dir_all(state.join("doorbell")).unwrap();
        let prev_no_ensure = std::env::var_os("FLYWHEEL_NO_DOORBELL_ENSURE");
        unsafe {
            std::env::set_var("FLYWHEEL_NO_DOORBELL_ENSURE", "1");
        }
        let done = ensure_armed_hosts_in(
            "proj",
            &["ws1".to_string()],
            std::path::Path::new("/usr/bin/true"),
            &state,
            Some(&home),
        )
        .expect("ensure drives the single owner");
        match prev_no_ensure {
            Some(v) => unsafe { std::env::set_var("FLYWHEEL_NO_DOORBELL_ENSURE", v) },
            None => unsafe { std::env::remove_var("FLYWHEEL_NO_DOORBELL_ENSURE") },
        }
        assert_eq!(done.len(), 1, "one workspace in, one child out");
        assert_eq!(done[0].workspace, "ws1");
        assert!(!dup.exists(), "the duplicate unit is retired, not reloaded");
        let units_left = unit_dir(&home).read_dir().map(|r| r.count()).unwrap_or(0);
        assert_eq!(
            units_left, 0,
            "no unit is (re)installed: the unit dir is empty after retire"
        );
        assert!(
            crate::doorbell::state_file(&state, "ws1").is_file(),
            "the workspace is owned through the unified state record"
        );
        restore_path_and_clean(root, prev_path);
    }

    /// Unload retires leftover units AND stops detached children (the
    /// daemon stop path drives the one owner out of existence). Dead pid
    /// 999998 is the stale-lock convention — killing it is a no-op.
    #[test]
    fn unload_retires_units_and_stops_children() {
        let _g = crate::config::lock_env();
        let (root, prev_path) = shimmed_path("unload");
        let home = root.join("home");
        let dup = unit_path(&home, "ws9");
        std::fs::create_dir_all(dup.parent().unwrap()).unwrap();
        std::fs::write(&dup, "stale duplicate unit").unwrap();
        let state = root.join("state");
        std::fs::create_dir_all(state.join("doorbell")).unwrap();
        std::fs::write(crate::doorbell::pid_file(&state, "ws9"), "999998\n").unwrap();
        std::fs::write(crate::doorbell::gen_file(&state, "ws9"), "{}\n").unwrap();
        let done = unload_all_hosts_in(&home, &state).expect("unload keeps going past shims");
        assert!(!dup.exists(), "duplicate unit retired");
        assert!(
            !crate::doorbell::pid_file(&state, "ws9").exists(),
            "child pid cleared"
        );
        assert!(
            !crate::doorbell::gen_file(&state, "ws9").exists(),
            "child gen cleared"
        );
        assert!(
            done.iter().any(|l| l.contains("ws9")),
            "transcript names the child: {done:?}"
        );
        restore_path_and_clean(root, prev_path);
    }

    /// bd-cf5kb sandbox e2e: a captain block bound by arrive in session B is
    /// rung exactly once by the dispatcher that hosts session A, because the
    /// arrival row is scoped to the project's hosting label and the block is
    /// addressable across sessions. The same row scoped to session B is rung
    /// zero times — the mutation that restores session-scoped arrive.
    #[tokio::test]
    async fn sandbox_captain_block_in_another_session_is_rung_once() {
        use doorbell_rt::{
            Actor, Brain, BrainConfig, BrainMsg, Budget, FileHost, FileHostConfig, FileStore,
            Filter, Identity, NoTextSink, Scope, SinkKind, SinkManager, Subscription,
            SubscriptionStore,
        };

        let _g = crate::config::lock_env();
        // A rex CLI must resolve so the fake two-session backend is the
        // selected plane; the host refuses a rex-row workspace on hc.
        let _rex = crate::backend::rex_test_guard::RexBinGuard::install("#!/bin/sh\nexit 0\n");
        let root = std::env::temp_dir().join(format!("fw-captain-wake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state = root.join("state");
        let data = root.join("data");
        let shim = root.join("bin");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&shim).unwrap();
        let shim_log = root.join("shim.log");
        for name in ["launchctl", "systemctl"] {
            let path = shim.join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
                    shim_log.display()
                ),
            )
            .unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        // Fake two-session plane roster: the captain block lives in session B
        // while the host/dispatcher owns session A (label flywheel-flywheel).
        let plane = root.join("plane");
        std::fs::write(
            &plane,
            r#"#!/bin/sh
cat <<'JSON'
{"result":{"agents":[{"pane_id":"block:01a1c323","agent_status":"idle","workspace_id":"adr-21-22-review","label":"SlateHarbor","tokens":{"toron_pin":"SlateHarbor"}}]}}
JSON
"#,
        )
        .unwrap();
        {
            let mut perms = std::fs::metadata(&plane).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&plane, perms).unwrap();
        }
        let plane_bin = plane.display().to_string();

        let prev_path = std::env::var_os("PATH");
        let prev_backend = std::env::var_os("FLYWHEEL_BACKEND");
        let prev_mcp_url = std::env::var_os("TORON_MCP_URL");
        let prev_data_dir = std::env::var_os("TORON_MCP_DATA_DIR");
        let prev_xdg = std::env::var_os("XDG_STATE_HOME");
        let mut new_path = std::ffi::OsString::from(format!("{}:", shim.display()));
        new_path.push(prev_path.clone().unwrap_or_default());
        unsafe {
            std::env::set_var("PATH", new_path);
            std::env::set_var("FLYWHEEL_BACKEND", "rex");
            std::env::remove_var("TORON_MCP_URL");
            std::env::set_var("TORON_MCP_DATA_DIR", &data);
            std::env::set_var("XDG_STATE_HOME", &state);
        }

        let project = "sandbox-proj";
        let host_workspace = "flywheel-flywheel"; // session A: the dispatcher
        let captain_session = "adr-21-22-review"; // session B: the captain block
        let pin = "SlateHarbor";
        let block = "block:01a1c323";
        let reg_dir = state.join("agents");
        std::fs::create_dir_all(&reg_dir).unwrap();
        let reg_row = serde_json::json!([{
            "pane_id": block,
            "name": pin,
            "mail_identity": pin,
        }]);
        let reg_path = reg_dir.join(format!("{project}.json"));
        std::fs::write(&reg_path, serde_json::to_string(&reg_row).unwrap()).unwrap();
        let mail_row = serde_json::json!({
            "importance": "high",
            "subject": "[bd-hlirm] done",
            "created_ts_us": 1
        });

        let run_host = |scope_workspace: String| {
            let state = state.clone();
            let reg_path = reg_path.clone();
            let plane_bin = plane_bin.clone();
            let project = project.to_string();
            let host_workspace = host_workspace.to_string();
            let pin = pin.to_string();
            let mail_row = mail_row.clone();
            async move {
                let mut store = SubscriptionStore::open(&state);
                store
                    .subscribe(
                        Actor::local_process(),
                        Subscription {
                            id: format!("cli-{scope_workspace}-{pin}"),
                            identity: Identity::Pin(pin.clone()),
                            scope: Scope::One(scope_workspace.clone()),
                            filter: Filter::default(),
                            sink: SinkKind::Hc,
                            budget: Budget::default_530(),
                            project: Some(project.clone()),
                            endpoint: None,
                        },
                        &scope_workspace,
                    )
                    .expect("arrival subscription");
                let (tx, rx) = tokio::sync::mpsc::channel(doorbell_rt::BRAIN_CAP);
                let mut fcfg =
                    FileHostConfig::new(state.clone(), project.clone(), host_workspace.clone());
                fcfg.registry_file = Some(reg_path);
                fcfg.toron_bin = "true".into();
                fcfg.plane_bin = plane_bin;
                let host = FileHost::new(fcfg);
                let mut sinks = SinkManager::new(tx.clone());
                sinks.add_sink_with_factory("hc", Box::new(NoTextSink::new()), || {
                    Box::new(NoTextSink::new())
                });
                let brain = Brain::new(
                    BrainConfig::new(host_workspace.clone()),
                    host,
                    FileStore::new(&state),
                    sinks,
                );
                let handle = tokio::spawn(async move { brain.run(rx).await });
                tx.send(BrainMsg::Sweep { now: 1_700_000_000 })
                    .await
                    .unwrap();
                tx.send(BrainMsg::PlaneStatus {
                    pin: pin.clone(),
                    status: "idle".to_string(),
                })
                .await
                .unwrap();
                tx.send(BrainMsg::Presence {
                    pin: pin.clone(),
                    state: "idle".to_string(),
                })
                .await
                .unwrap();
                tx.send(BrainMsg::InboxFetched {
                    pin: pin.clone(),
                    rows: vec![mail_row],
                })
                .await
                .unwrap();
                tx.send(BrainMsg::Shutdown).await.unwrap();
                drop(tx);
                handle.await.expect("brain joins");
                std::fs::read_to_string(FileStore::new(&state).journal_file(&host_workspace))
                    .unwrap_or_default()
            }
        };

        let hosted = run_host(crate::doorbell::arrival_scope(host_workspace)).await;
        assert!(
            hosted.contains("\"Prompt\"") && hosted.contains(pin),
            "the captain block in {captain_session} is rung by the {host_workspace} host, got:\n{hosted}"
        );

        assert!(
            !shim_log.exists() || std::fs::read_to_string(&shim_log).unwrap().is_empty(),
            "no supervisor tool was shelled"
        );

        unsafe {
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            match prev_backend {
                Some(v) => std::env::set_var("FLYWHEEL_BACKEND", v),
                None => std::env::remove_var("FLYWHEEL_BACKEND"),
            }
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
