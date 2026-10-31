//! gc: the hourly prune must reclaim abandoned workspace state through the
//! REAL binary, and say what it removed.
//!
//! Unit tests cover the classifier (`doorbell-rt` `store::prune_tests`). This
//! is the wiring witness: a scratch state dir with one dead workspace (dead
//! pid, old files) and one live-but-UNHOSTED workspace (a live sleep child and
//! a fresh lease). The daemon must remove the dead one's records, keep the
//! live one's, and name every family it pruned in the log.
//!
//! The live workspace is deliberately not in `--host`: only the live-lease
//! widening in `prune_live_set` protects its audit log, and only the live-pid
//! guard protects its sidecars (their mtimes are aged like the dead ones).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn scratch() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("dbcl-prune-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn age(path: &Path, secs: u64, now: u64) {
    let when = UNIX_EPOCH + Duration::from_secs(now - secs);
    std::fs::File::open(path)
        .and_then(|f| f.set_modified(when))
        .unwrap_or_else(|e| panic!("age {}: {e}", path.display()));
}

fn lease(pid: u32, heartbeat: u64) -> String {
    format!(
        r#"{{"pid":{pid},"boot_id":"test","host":"test","started_unix":{heartbeat},"heartbeat_unix":{heartbeat}}}"#
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe")
        .local_addr()
        .expect("probe addr")
        .port()
}

#[test]
fn daemon_prunes_abandoned_state_and_names_every_family() {
    let root = scratch();
    let fw = root.join("fw");
    let db = root.join("db");
    let doorbell = fw.join("doorbell");
    for d in [&doorbell, &db] {
        std::fs::create_dir_all(d).unwrap();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    const OLD: u64 = 30 * 24 * 60 * 60;

    // A workspace whose host died long ago.
    write(&doorbell.join("gone.pid"), "999999\n");
    write(&doorbell.join("gone.gen"), "{}");
    write(&doorbell.join("gone.project"), "p");
    write(&doorbell.join("gone.audit.jsonl"), &"x".repeat(1024));
    write(&doorbell.join("owner-gone.json"), &lease(999999, now - OLD));

    // A workspace with a LIVE host that this daemon does not host.
    let mut sleeper = Command::new("sleep")
        .arg("60")
        .stdout(Stdio::null())
        .spawn()
        .expect("sleep child");
    let live_pid = sleeper.id();
    write(&doorbell.join("leased-live.pid"), &format!("{live_pid}\n"));
    write(&doorbell.join("leased-live.gen"), "{}");
    write(&doorbell.join("leased-live.project"), "p");
    write(&doorbell.join("leased-live.audit.jsonl"), "keep\n");
    write(
        &doorbell.join("owner-leased-live.json"),
        &lease(live_pid, now),
    );

    // Everything is OLD except the live lease: the live sidecars are aged too,
    // so only the PID can protect them.
    for f in [
        "gone.pid",
        "gone.gen",
        "gone.project",
        "gone.audit.jsonl",
        "owner-gone.json",
        "leased-live.pid",
        "leased-live.gen",
        "leased-live.project",
        "leased-live.audit.jsonl",
    ] {
        age(&doorbell.join(f), OLD, now);
    }

    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_doorbell"))
        .arg("serve")
        .arg("--state-dir")
        .arg(&db)
        .args(["--port", &port.to_string()])
        .arg("--flywheel-state-dir")
        .arg(&fw)
        .args(["--host", "smoke/smoke"])
        .env("DOORBELL_TORON_BIN", "true")
        .env("DOORBELL_HERDR_BIN", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    // The rotate/prune tick fires its FIRST tick immediately; 5s is margin.
    std::thread::sleep(Duration::from_secs(5));
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait daemon");
    let _ = sleeper.kill();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !doorbell.join("gone.audit.jsonl").exists(),
        "a dead workspace's audit log must go: {log}"
    );
    assert!(
        !doorbell.join("gone.pid").exists()
            && !doorbell.join("gone.gen").exists()
            && !doorbell.join("gone.project").exists(),
        "a dead host's pid/gen/project records must go: {log}"
    );
    assert!(
        !doorbell.join("owner-gone.json").exists(),
        "a stale lease must go: {log}"
    );
    assert!(
        doorbell.join("leased-live.pid").exists()
            && doorbell.join("leased-live.gen").exists()
            && doorbell.join("leased-live.project").exists(),
        "a live pid protects its records even when they are old: {log}"
    );
    assert!(
        doorbell.join("leased-live.audit.jsonl").exists(),
        "a live lease protects its logs even when the daemon does not host it: {log}"
    );
    assert!(
        doorbell.join("owner-leased-live.json").exists(),
        "a live lease is never a prune candidate: {log}"
    );
    assert!(
        log.contains("pruned")
            && log.contains("audit")
            && log.contains("sidecar")
            && log.contains("lease"),
        "the prune line must name every family it touched (never delete silently): {log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
