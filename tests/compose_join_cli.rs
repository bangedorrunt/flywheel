//! CLI-edge proof for bd-3ann1 slice 2: `--join --dry-run` decides bindings
//! without touching the plane or starting anything, and `--run` without
//! `--yes` refuses when the plan needs a new pane. A fake plane in the
//! TestSandbox bin dir records every call; no live fleet is involved.

use std::process::Command;

use flywheel_test_sandbox::TestSandbox;

fn fake_plane(sandbox: &TestSandbox, log_name: &str) -> std::path::PathBuf {
    let log = sandbox.root().join(log_name);
    for tool in ["toron", "rex"] {
        let path = sandbox.bin_dir().join(tool);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexit 0\n",
                log.display()
            ),
        )
        .expect("fake plane tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("fake plane mode");
        }
    }
    log
}

fn flywheel(sandbox: &TestSandbox) -> Command {
    let mut cmd = sandbox.command(env!("CARGO_BIN_EXE_flywheel"));
    let path = format!(
        "{}:{}",
        sandbox.bin_dir().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", path);
    cmd
}

#[test]
fn join_dry_run_binds_without_starting_anything() {
    let sandbox = TestSandbox::new();
    let log = fake_plane(&sandbox, "plane.log");
    let out = flywheel(&sandbox)
        .args([
            "workflow",
            "--skip-preflight",
            "compose",
            "bd-42 bind the wave",
            "--parts",
            "wave-swarm",
            "--join",
            "--dry-run",
            "--project",
            "joinproj",
            "--json",
        ])
        .output()
        .expect("flywheel compose --join --dry-run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "dry run must succeed: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dry_run"),
        "preview must be printed: {stdout}"
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    for verb in ["spawn", "send", "publish", "post", "claim", "dispatch"] {
        assert!(
            !calls.to_ascii_lowercase().contains(verb),
            "--dry-run must not {verb}: {calls}"
        );
    }
}

#[test]
fn join_run_without_yes_refuses_when_a_spawn_is_needed() {
    let sandbox = TestSandbox::new();
    let log = fake_plane(&sandbox, "plane.log");
    let out = flywheel(&sandbox)
        .args([
            "workflow",
            "--skip-preflight",
            "compose",
            "bd-42 bind the wave",
            "--parts",
            "wave-swarm",
            "--join",
            "--run",
            "--project",
            "joinproj",
        ])
        .output()
        .expect("flywheel compose --join --run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "no consent must refuse: {stderr}");
    assert!(
        stderr.contains("--yes"),
        "refusal must name --yes: {stderr}"
    );
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    for verb in ["spawn", "send", "publish", "post"] {
        assert!(
            !calls.to_ascii_lowercase().contains(verb),
            "a refused run must not {verb}: {calls}"
        );
    }
}
