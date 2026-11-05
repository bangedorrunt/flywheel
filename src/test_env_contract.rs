//! bd-w7g8: prove a test process never touches the operator's HOME.
//!
//! [`doorbell_rt::testkit::apply_test_env`] is the contract; this is the proof
//! that it holds for a process this workspace actually spawns. The child below
//! is this same test binary re-executed with its own environment, so the run is
//! hermetic without needing the production binary to be built (a unit test in a
//! bin target cannot reach `CARGO_BIN_EXE_*`).
//!
//! The mutation that must turn this red is dropping the `XDG_*` pins from the
//! contract: flywheel's own `config_dir` / `state_dir` read `HOME` directly, so
//! an unpinned child writes `~/.config/flywheel` and `~/.local/state/flywheel`
//! into whatever HOME it inherited.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_MARKER: &str = "FLYWHEEL_TEST_ENV_PROBE";
const PROBE: &str = "test_env_contract::operator_home_probe_child";

#[test]
fn a_test_process_does_not_touch_the_operator_home() {
    let root = doorbell_rt::testkit::harness_root();
    let _ = std::fs::remove_dir_all(&root);
    let operator_home =
        std::env::temp_dir().join(format!("flywheel-operator-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&operator_home);
    let shim_dir = root.join("bin");
    std::fs::create_dir_all(&operator_home).expect("fake operator home");
    std::fs::create_dir_all(&shim_dir).expect("shim dir");

    let launchctl_log = root.join("launchctl.log");
    let shim = shim_dir.join("launchctl");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho \"launchctl $*\" >> '{}'\nexit 1\n",
            launchctl_log.display()
        ),
    )
    .expect("write launchctl shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
            .expect("chmod launchctl shim");
    }

    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([
        "--ignored",
        "--exact",
        PROBE,
        "--test-threads=1",
        "--nocapture",
    ])
    .env("HOME", &operator_home)
    .env("XDG_CONFIG_HOME", operator_home.join(".config"))
    .env("XDG_STATE_HOME", operator_home.join(".local/state"))
    .env("XDG_DATA_HOME", operator_home.join(".local/share"))
    .env("XDG_CACHE_HOME", operator_home.join(".cache"))
    .env("PATH", format!("{}:/usr/bin:/bin", shim_dir.display()))
    .env("TORON_MCP_URL", "http://127.0.0.1:1/never-a-daemon")
    .env(PROBE_MARKER, "1");
    doorbell_rt::testkit::apply_test_env(&mut cmd);

    let out = cmd.output().expect("spawn the probe child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("running 1 test"),
        "the probe child did not run a test, so nothing was proved:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let leaked: Vec<&str> = [
        ".config/flywheel",
        ".local/state/flywheel",
        ".local/share/toron",
        "index.sqlite",
        "operator_nsec",
    ]
    .into_iter()
    .filter(|p| operator_home.join(p).exists())
    .collect();
    assert!(
        out.status.success() && leaked.is_empty(),
        "the probe escaped the contract: leaked={leaked:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let calls = std::fs::read_to_string(&launchctl_log).unwrap_or_default();
    assert!(
        calls.is_empty(),
        "the probe reached the supervisor service: {calls}"
    );

    for landed in [
        "home/.config/flywheel",
        "home/.local/state/flywheel",
        "data/projects",
    ] {
        assert!(
            root.join(landed).is_dir(),
            "the sandbox should hold {landed}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&operator_home);
}

/// The child half. Ignored so it never runs on its own; the parent runs it with
/// the contract applied and `HOME` pointed at a fake operator home.
#[test]
#[ignore = "child half of a_test_process_does_not_touch_the_operator_home"]
fn operator_home_probe_child() {
    assert_eq!(
        std::env::var(PROBE_MARKER).as_deref(),
        Ok("1"),
        "the probe child only runs from a_test_process_does_not_touch_the_operator_home"
    );

    let root = PathBuf::from(
        std::env::var(doorbell_rt::testkit::UNDER_TEST_ENV)
            .expect("TORON_UNDER_TEST must be set by the harness"),
    );

    let config = crate::config::config_dir();
    let state = crate::config::state_dir();
    let (data, db) = crate::mail::mailbox_roots();
    std::fs::create_dir_all(&config).expect("write config dir");
    std::fs::create_dir_all(&state).expect("write state dir");
    std::fs::create_dir_all(&data).expect("write mail data dir");

    for var in [
        "TORON_DATA_DIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "TMPDIR",
        "HC_SOCKET_PATH",
    ] {
        let value = std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set"));
        assert!(
            Path::new(&value).starts_with(&root),
            "{var} must point inside the harness root {root:?}, got {value}"
        );
    }
    assert!(
        std::env::var_os("TORON_MCP_URL").is_none(),
        "the contract must remove the ambient daemon URL"
    );
    let sock = std::env::var("HC_SOCKET_PATH").expect("hc socket path");
    assert!(
        !Path::new(&sock).exists(),
        "the hc pin must name a socket that is not there, got {sock}"
    );

    assert!(
        config.starts_with(&root),
        "config dir escaped the sandbox: {}",
        config.display()
    );
    assert!(
        state.starts_with(&root),
        "state dir escaped the sandbox: {}",
        state.display()
    );

    assert!(
        data.starts_with(&root) && db.starts_with(&root),
        "mail roots escaped the sandbox: {} / {}",
        data.display(),
        db.display()
    );
}
