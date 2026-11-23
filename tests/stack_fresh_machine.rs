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
case "$1" in
  list) printf '%s' '{list}' ;;
  print)
    case "$*" in
      *{bad}) printf 'state = spawn failed\n' ;;
      *) printf 'state = running\n' ;;
    esac ;;
esac
exit 0
"#
    );
    executable(&sandbox.bin_dir().join("launchctl"), &shim);

    let bin = sandbox.root().join("cargo/bin");
    std::fs::create_dir_all(&bin).expect("cargo bin");
    executable(&bin.join("flywheel"), "#!/bin/sh\necho 'flywheel 0.10.6'\n");
    executable(&bin.join("toron"), "#!/bin/sh\necho 'toron 0.9.0'\n");

    let agents = sandbox.root().join("home/Library/LaunchAgents");
    std::fs::create_dir_all(&agents).expect("launch agents");
    for label in LABELS {
        std::fs::write(
            agents.join(format!("{label}.plist")),
            "<plist version=\"1.0\"><dict/></plist>\n",
        )
        .expect("plist");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let addr = listener.local_addr().expect("addr").to_string();
    // Keep the listener alive for the child's connect probe.
    std::mem::forget(listener);
    (sandbox, addr)
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
    std::fs::write(state.join("agents/demo.json"), row.to_string()).expect("seat row");
    std::fs::write(
        state.join("events-tokens/demo-DoctorProbe.token"),
        "probe-token",
    )
    .expect("event token");
    if with_doorbell {
        std::fs::create_dir_all(state.join("doorbell")).expect("doorbell dir");
        let state_row = serde_json::json!({
            "pid": std::process::id(), "project": "demo"
        });
        std::fs::write(state.join("doorbell/demo.json"), state_row.to_string())
            .expect("doorbell state");
    }
}

fn doctor(sandbox: &TestSandbox, addr: &str, fix: bool) -> std::process::Output {
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    cmd.env("PATH", sandbox.path_env())
        .env("CARGO_INSTALL_ROOT", sandbox.root().join("cargo"))
        .env("FLYWHEEL_TORON_URL", format!("http://{addr}"))
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
    for name in ["binaries", "jobs", "toron", "doorbell", "events", "wake"] {
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

#[test]
fn doctor_fix_reregisters_a_job_then_reprobes() {
    let (sandbox, addr) = fixture(true);
    let out = doctor(&sandbox, &addr, true);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "doctor --fix must repair and pass: {text}"
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
fn doctor_fails_when_the_doorbell_dispatcher_is_down() {
    let (sandbox, addr) = fixture(false);
    seed_seat(&sandbox, false);
    let out = doctor(&sandbox, &addr, false);
    assert!(!out.status.success(), "a down dispatcher must fail doctor");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("doctor json");
    let doorbell = v["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "doorbell")
        .expect("doorbell probe");
    assert_eq!(doorbell["ok"], false);
    assert!(
        doorbell["detail"].as_str().unwrap().contains("demo=absent"),
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
