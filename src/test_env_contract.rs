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

/// bd-q52vj: a `#[test]` that resolves a state/registry/last_sent path must
/// hold a hermetic guard. `load_corrupt_file_returns_empty` once wrote into
/// whatever XDG_STATE_HOME a parallel test's guard had installed, and that
/// guard's Drop removed the dir mid-write (NotFound). Keep the class out.
#[test]
fn state_path_tests_hold_a_hermetic_guard() {
    const DANGER: &[&str] = &[
        "last_sent_file(",
        "spawn_registry_file(",
        "state_dir(",
        "agents_dir(",
        "plan_file(",
        "save_spawn_registry(",
        "load_spawn_registry(",
        "save_agent_names(",
        "load_agent_names(",
        "record_sent(",
        "log_event(",
    ];
    const GUARDS: &[&str] = &[
        "test_state_home",
        "loops_test_guard",
        "lock_env",
        "ENV_LOCK",
        "set_registry_root_override",
        "harness_root",
        "TestSandbox",
        "apply_test_env",
        "tempdir",
        "TempDir",
        "tempfile",
        "test_project",
        "test_home",
        "sandbox",
        "UNDER_TEST",
    ];

    fn has_call(body: &str, name: &str) -> bool {
        let needle = name.to_string();
        let mut from = 0;
        while let Some(at) = body[from..].find(&needle) {
            let pos = from + at;
            let prev = pos.checked_sub(1).map(|i| body.as_bytes()[i] as char);
            if prev.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_')) {
                return true;
            }
            from = pos + needle.len();
        }
        false
    }

    fn strip(line: &str) -> String {
        let clean = line.split("//").next().unwrap_or(line).to_string();
        let mut out = String::new();
        let mut in_str = false;
        let mut prev = '\0';
        for c in clean.chars() {
            if c == '"' && prev != '\\' {
                in_str = !in_str;
                out.push(' ');
            } else if in_str {
                out.push(' ');
            } else {
                out.push(c);
            }
            prev = c;
        }
        out
    }

    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut violations = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = source.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            if !lines[i].trim().starts_with("#[test") {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < lines.len() && !lines[j].contains("fn ") {
                j += 1;
            }
            if j == lines.len() {
                break;
            }
            let name = lines[j]
                .split("fn ")
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or("?")
                .to_string();
            let mut depth = 0i32;
            let mut started = false;
            let mut body = String::new();
            let mut k = j;
            while k < lines.len() {
                body.push_str(&strip(lines[k]));
                for c in strip(lines[k]).chars() {
                    if c == '{' {
                        depth += 1;
                        started = true;
                    } else if c == '}' {
                        depth -= 1;
                    }
                }
                if started && depth <= 0 {
                    break;
                }
                k += 1;
            }
            if DANGER.iter().any(|p| has_call(&body, p)) && !GUARDS.iter().any(|g| body.contains(g))
            {
                violations.push(format!("{}:{}", file.display(), name));
            }
            i = k + 1;
        }
    }
    assert!(
        violations.is_empty(),
        "state-path tests without a hermetic guard: {violations:?}"
    );
}

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
