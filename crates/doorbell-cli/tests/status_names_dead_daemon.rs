//! `doorbell status` against a DEAD daemon must read as DOWN, never "armed".
//!
//! bd-gefp measured the lie live: `doorbell status` printed
//! `doorbell: armed on flywheel/flywheel-flywheel, … (3 from persisted state)`
//! directly above `"alive": false` while launchd said `state = not running`.
//! The armed line is an ARM transcript (start/restart), not a status read, so
//! a read verb must never print it — and must name the daemon verdict itself.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("dbcl-dead-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn status_of_a_dead_daemon_says_down_and_never_armed() {
    let root = scratch();
    let home = root.join("home");
    let state = root.join("state");
    for d in [&home, &state] {
        std::fs::create_dir_all(d).unwrap();
    }
    // No pid file, and a port nothing listens on: the daemon is DOWN.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let out = Command::new(env!("CARGO_BIN_EXE_doorbell"))
        .arg("--state-dir")
        .arg(&state)
        .args(["--port", &port.to_string()])
        .arg("--flywheel-state-dir")
        .arg(root.join("fw"))
        .arg("status")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DOORBELL_STATE_DIR")
        .env_remove("DOORBELL_PORT")
        .output()
        .expect("run doorbell status");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "status must succeed while naming the verdict: {stderr}"
    );
    assert!(
        stderr.contains("daemon DOWN"),
        "a dead daemon must read as DOWN, got stderr: {stderr}"
    );
    // `armstate::announce` prints on STDOUT, so a stdout-only JSON check would
    // let the arm line back in unseen — both streams are asserted.
    assert!(
        !stdout.contains("armed on") && !stderr.contains("armed on"),
        "a status READ must never print the arm transcript, got stdout: {stdout} stderr: {stderr}"
    );
    // The JSON on stdout stays machine-readable and carries the same verdict.
    assert!(stdout.contains("\"alive\":false"), "stdout: {stdout}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A deliberate stop must not read as an outage. The supervision unit now
/// re-runs an idle job, so "down" alone would be ambiguous; the marker `stop`
/// writes is what disambiguates it, and status must read it.
#[test]
fn status_names_a_deliberate_stop_as_stopped() {
    let root = scratch();
    let home = root.join("home");
    let state = root.join("state");
    for d in [&home, &state] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(state.join("stopped-by-operator"), b"doorbell stop\n").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let out = Command::new(env!("CARGO_BIN_EXE_doorbell"))
        .arg("--state-dir")
        .arg(&state)
        .args(["--port", &port.to_string()])
        .arg("--flywheel-state-dir")
        .arg(root.join("fw"))
        .arg("status")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DOORBELL_STATE_DIR")
        .env_remove("DOORBELL_PORT")
        .output()
        .expect("run doorbell status");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stderr.contains("STOPPED by operator"),
        "a deliberate stop must read as STOPPED, got stderr: {stderr}"
    );
    assert!(
        stdout.contains("\"stopped_by_operator\":true"),
        "the JSON must carry the intent too, got stdout: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
