//! `doorbell stop` under a fake HOME must never boot out the operator's
//! service.
//!
//! The launchd domain `gui/<uid>/com.doorbell.serve` is keyed by uid, not
//! HOME. The old guard read the plist under `$HOME`; with a fake HOME it could
//! not, failed open, and ran `launchctl bootout` against the live unit. A
//! full test run under a fake HOME shut the production daemon down that way.
//!
//! This drives the real binary with an empty HOME and a `launchctl` /
//! `systemctl` shim first on PATH that records its arguments and exits 1, and
//! asserts the shim saw no unload.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn scratch() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("dbcl-fakehome-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A shim that appends `<name> <args>` to `log` and exits 1.
fn shim(bin: &Path, name: &str, log: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let path = bin.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit 1\n",
            log.display()
        ),
    )
    .expect("write shim");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

#[test]
fn stop_under_a_fake_home_never_boots_out_the_service() {
    let root = scratch();
    let home = root.join("home");
    let state = root.join("state");
    let bin = root.join("bin");
    for d in [&home, &state, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let log = root.join("supervisor-calls.log");
    std::fs::write(&log, "").unwrap();
    shim(&bin, "launchctl", &log);
    shim(&bin, "systemctl", &log);

    // A live pid for the doorbell to stop: the clean-exit arm unloads the
    // supervisor, which is the call under test. A reaper thread collects the
    // corpse, so a stopped child is not a zombie the doorbell's `kill -0`
    // still sees; the handle stays shared so the test can still kill its own
    // child if the stop failed.
    let child = std::sync::Arc::new(std::sync::Mutex::new(
        Command::new("sleep").arg("300").spawn().expect("sleep"),
    ));
    let pid = child.lock().unwrap().id();
    let reaper = {
        let child = child.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                if child.lock().unwrap().try_wait().ok().flatten().is_some() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        })
    };
    std::fs::write(state.join("doorbell.pid"), format!("{pid}\n")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_doorbell"))
        .arg("--state-dir")
        .arg(&state)
        .args(["--port", &port.to_string()])
        .arg("--flywheel-state-dir")
        .arg(root.join("fw"))
        .arg("stop")
        .env("HOME", &home)
        .env("PATH", path)
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DOORBELL_STATE_DIR")
        .env_remove("DOORBELL_PORT")
        .output()
        .expect("run doorbell stop");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        let _ = child.lock().unwrap().kill();
    }
    let stopped = reaper.join().unwrap_or(false);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        out.status.success(),
        "stop: {stderr} {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(stopped, "the doorbell stopped its pid");
    let unloads: Vec<&str> = calls
        .lines()
        .filter(|l| l.contains("bootout") || l.contains(" stop "))
        .collect();
    assert!(
        unloads.is_empty(),
        "a stop under a fake HOME must not unload the supervisor: {unloads:?}"
    );
    #[cfg(target_os = "macos")]
    assert!(
        calls.lines().any(|l| l.starts_with("launchctl print gui/")),
        "the guard reads the LOADED unit through launchctl print: {calls:?}"
    );
    assert!(
        stderr.contains("supervisor not unloaded"),
        "the skip is reported: {stderr}"
    );
}
