//! bd-rgs02: `stack doctor` on a fresh machine, with no live daemon. HOME and
//! XDG are sandbox-private, `launchctl` and the installed binaries are stubs,
//! and the toron address is a loopback listener owned by this test.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;

use flywheel_test_sandbox::TestSandbox;

const LABELS: &[&str] = &[
    "dev.flywheel.daemon",
    "dev.flywheel.cron",
    "dev.flywheel.doorbell.demo",
    "com.toron.serve",
    "com.toron.logrotate",
];

fn executable(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write stub");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn fixture(spawn_failed: bool) -> (TestSandbox, String) {
    let sandbox = TestSandbox::new();

    let mut list = String::new();
    for label in LABELS {
        list.push_str(&format!("-\t0\t{label}\n"));
    }
    let bad = if spawn_failed {
        "dev.flywheel.daemon"
    } else {
        "never"
    };
    let shim = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$LAUNCHCTL_LOG"
mark="${{LAUNCHCTL_LOG%.log}}.fixed"
case "$1" in
  list) printf '%s' '{list}' ;;
  bootstrap) case "$*" in *{bad}) : >"$mark" ;; esac ;;
  print)
    case "$*" in
      *{bad}) if [ -f "$mark" ]; then printf 'state = running\n'; else printf 'state = spawn failed\n'; fi ;;
      *com.toron.logrotate) printf 'state = idle\n' ;;
      *) printf 'state = running\n' ;;
    esac ;;
esac
exit 0
"#
    );
    executable(&sandbox.bin_dir().join("launchctl"), &shim);
    set_holder(&sandbox, "flywheel");
    executable(
        &sandbox.bin_dir().join("rex"),
        r#"#!/bin/sh
printf '%s\n' '{"sessions":[{"session_id":"session:live-1","label":"live","pane_count":1,"tab_count":1}]}'
"#,
    );

    let bin = sandbox.root().join("cargo/bin");
    std::fs::create_dir_all(&bin).expect("cargo bin");
    executable(&bin.join("flywheel"), "#!/bin/sh\necho 'flywheel 0.10.6'\n");
    executable(&bin.join("toron"), "#!/bin/sh\necho 'toron 0.9.0'\n");

    let agents = sandbox.root().join("home/Library/LaunchAgents");
    std::fs::create_dir_all(&agents).expect("launch agents");
    for label in LABELS {
        let body = if *label == "com.toron.logrotate" {
            "<plist version=\"1.0\"><dict><key>StartInterval</key><integer>3600</integer></dict></plist>\n"
        } else {
            "<plist version=\"1.0\"><dict/></plist>\n"
        };
        std::fs::write(agents.join(format!("{label}.plist")), body).expect("plist");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let addr = listener.local_addr().expect("addr").to_string();
    // Keep the listener alive for the child's connect probe.
    std::mem::forget(listener);
    (sandbox, addr)
}

fn set_holder(sandbox: &TestSandbox, name: &str) {
    executable(&sandbox.bin_dir().join("lsof"), "#!/bin/sh\necho 4242\n");
    executable(
        &sandbox.bin_dir().join("ps"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in\n  *999999*) exit 1 ;;\n  *4242*) echo {name}; exit 0 ;;\n  *) echo S; exit 0 ;;\nesac\n"
        ),
    );
}

fn seed_seat(sandbox: &TestSandbox, with_doorbell: bool) {
    let state = sandbox.root().join("state/flywheel");
    std::fs::create_dir_all(state.join("agents")).expect("agents dir");
    std::fs::create_dir_all(state.join("events-tokens")).expect("tokens dir");
    let row = serde_json::json!([{
        "pane_id": "block:probe",
        "name": "probe-pane",
        "harness": "opencode",
        "mail_identity": "DoctorProbe",
        "skills_injected": false,
        "incarnation": 0
    }]);
    std::fs::write(state.join("agents/session-live-1.json"), row.to_string()).expect("seat row");
    let stale = serde_json::json!([{
        "pane_id": "block:dead", "name": "dead-pane", "harness": "opencode",
        "mail_identity": "FoggyLion", "skills_injected": false, "incarnation": 0
    }]);
    std::fs::write(state.join("agents/session-dead-2.json"), stale.to_string())
        .expect("stale seat row");
    std::fs::write(
        state.join("events-tokens/session-live-1-DoctorProbe.token"),
        "probe-token",
    )
    .expect("event token");
    std::fs::create_dir_all(state.join("doorbell")).expect("doorbell dir");
    let pid = if with_doorbell {
        std::process::id()
    } else {
        999999
    };
    let state_row = serde_json::json!({"pid": pid, "project": "session:live-1"});
    std::fs::write(
        state.join("doorbell/session:live-1.state.json"),
        state_row.to_string(),
    )
    .expect("doorbell state");
}

fn doctor(sandbox: &TestSandbox, addr: &str, fix: bool) -> std::process::Output {
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    cmd.env("PATH", sandbox.path_env())
        .env("CARGO_INSTALL_ROOT", sandbox.root().join("cargo"))
        .env("FLYWHEEL_TORON_URL", format!("http://{addr}"))
        .env(
            "FLYWHEEL_SERVE_PORT",
            addr.rsplit(':').next().unwrap_or("8787"),
        )
        .env("LAUNCHCTL_LOG", sandbox.root().join("launchctl.log"))
        .args(["stack", "doctor", "--json"]);
    if fix {
        cmd.arg("--fix");
    }
    cmd.output().expect("run flywheel stack doctor")
}

#[test]
fn stack_doctor_probes_every_job_on_a_fresh_machine() {
    let (sandbox, addr) = fixture(false);
    seed_seat(&sandbox, true);
    let out = doctor(&sandbox, &addr, false);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "fresh-machine doctor must pass: {text}"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    assert_eq!(v["kind"], "flywheel-stack-doctor.v1");
    assert_eq!(v["ok"], true);
    for name in [
        "binaries",
        "jobs",
        "toron",
        "doorbell",
        "events",
        "wake",
        "daemon_port",
    ] {
        assert_eq!(
            v["probes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .and_then(|p| p["ok"].as_bool()),
            Some(true),
            "probe {name} must run and pass: {text}"
        );
    }
    let log = std::fs::read_to_string(sandbox.root().join("launchctl.log")).expect("launchctl log");
    for label in LABELS {
        assert!(
            log.lines()
                .any(|l| l.contains("print gui/") && l.ends_with(label)),
            "doctor must probe job {label}: {log}"
        );
    }
}

fn log_has_repair(sandbox: &TestSandbox) -> bool {
    std::fs::read_to_string(sandbox.root().join("launchctl.log"))
        .unwrap_or_default()
        .lines()
        .any(|l| l.starts_with("bootstrap gui/") && l.contains("dev.flywheel.daemon"))
}

#[test]
fn doctor_fix_reregisters_a_job_then_reprobes() {
    let (sandbox, addr) = fixture(true);
    let out = doctor(&sandbox, &addr, true);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The shim reports the job as spawn failed again after bootstrap, so the
    // final probe still fails; what this witness proves is that --fix attempted
    // the repair and re-ran the probes, not that the stub changed its mind.
    assert!(
        log_has_repair(&sandbox),
        "doctor --fix must attempt repair: {text}"
    );
    let log = std::fs::read_to_string(sandbox.root().join("launchctl.log")).expect("launchctl log");
    assert!(
        log.contains("bootout gui/") && log.contains("dev.flywheel.daemon"),
        "the failed job must be booted out: {log}"
    );
    assert!(
        log.contains("bootstrap gui/") && log.contains("dev.flywheel.daemon.plist"),
        "the failed job must be bootstrapped from its plist: {log}"
    );
}

#[test]
fn install_reports_a_bad_job_and_restarts_the_rest() {
    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = TestSandbox::new();
    let out = sandbox
        .command("/bin/bash")
        .env("PATH", sandbox.path_env())
        .arg(format!("{root}/scripts/test-stack-restart-failure.sh"))
        .current_dir(root)
        .output()
        .expect("run restart-failure witness");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn doctor_names_a_foreign_holder_of_the_daemon_port() {
    let (sandbox, addr) = fixture(false);
    set_holder(&sandbox, "caveman-proxy");
    let out = doctor(&sandbox, &addr, false);
    assert!(
        !out.status.success(),
        "a foreign port holder must fail doctor"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    let probe = v["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "daemon_port")
        .expect("daemon_port probe");
    assert_eq!(probe["ok"], false);
    let text = format!("{} {}", probe["detail"], probe["fix"]);
    assert!(
        text.contains("caveman-proxy"),
        "the holder must be named: {text}"
    );
}

#[test]
fn doctor_fails_when_the_doorbell_dispatcher_is_down() {
    let (sandbox, addr) = fixture(false);
    seed_seat(&sandbox, false);
    let out = doctor(&sandbox, &addr, false);
    assert!(
        !out.status.success(),
        "a down dispatcher must fail doctor: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    let doorbell = v["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "doorbell")
        .expect("doorbell probe");
    assert_eq!(doorbell["ok"], false);
    assert!(
        doorbell["detail"]
            .as_str()
            .unwrap()
            .contains("session:live-1=down"),
        "{doorbell}"
    );
}

#[test]
fn doctor_fails_loudly_when_a_probe_fails() {
    let (sandbox, addr) = fixture(true);
    let out = doctor(&sandbox, &addr, false);
    assert!(
        !out.status.success(),
        "doctor must exit non-zero on a failed probe"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    assert_eq!(v["ok"], false);
}

#[test]
fn interval_jobs_and_dead_sessions_do_not_fail_doctor() {
    let (sandbox, addr) = fixture(false);
    seed_seat(&sandbox, true);
    let out = doctor(&sandbox, &addr, false);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "no false FAIL: {text}");
    assert!(
        text.contains("com.toron.logrotate=idle (interval)"),
        "{text}"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    let warnings: Vec<_> = v["probes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["warn"].is_string())
        .map(|p| p["name"].clone())
        .collect();
    assert!(
        !warnings.is_empty(),
        "stale sessions must be a WARN: {text}"
    );
    assert!(
        !text.contains("session:dead-2=absent"),
        "dead sessions are not probed: {text}"
    );
}
