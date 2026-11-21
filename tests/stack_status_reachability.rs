//! bd-4bxtl: `stack status --check` is a health check, not a build-record
//! reader. When nothing answers on the toron address it must fail and name
//! that address. A mutant that drops the reachability term turns this RED.

use std::process::Command;

use flywheel_test_sandbox::TestSandbox;

#[test]
fn stack_check_fails_when_the_toron_address_refuses_connections() {
    let sandbox = TestSandbox::new();
    // A fake toron status command: a real one probes launchd, which a sandbox
    // forbids. The daemon address under test is refused by the TCP probe, not
    // by this status report.
    let script = "#!/bin/sh\necho '{\"state\":\"stopped\",\"addr\":\"127.0.0.1:1\",\"build\":{\"verdict\":\"current\",\"restart_required\":false}}'\n";
    let cargo_bin = sandbox.root().join("cargo/bin");
    std::fs::create_dir_all(&cargo_bin).expect("fake cargo bin");
    for path in [sandbox.bin_dir().join("toron"), cargo_bin.join("toron")] {
        std::fs::write(&path, script).expect("fake toron");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("fake toron mode");
        }
    }
    let _scope = sandbox.scope();

    let out = Command::new(env!("CARGO_BIN_EXE_flywheel"))
        .args(["stack", "status", "--check"])
        .env("CARGO_INSTALL_ROOT", sandbox.root().join("cargo"))
        .env("FLYWHEEL_TORON_URL", "http://127.0.0.1:1/never-a-daemon")
        .output()
        .expect("run flywheel stack status --check");

    assert!(
        !out.status.success(),
        "--check must fail when the daemon is down"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("127.0.0.1:1") && text.contains("toron daemon not listening"),
        "the failure must name the down daemon: {text}"
    );
}
