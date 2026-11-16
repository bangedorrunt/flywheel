//! The captain-only fleet-mutation guard (bd-0uzrq.6) must cover EVERY door
//! that replaces a live binary or bounces the doorbell (bd-0uzrq.24). These
//! drive the real binary: a helper-only test stays green when an arm is dropped
//! from `main`'s dispatch.
//!
//! Sandbox contract (AGENTS.md, hard): a CLEAN environment (`.env_clear()` —
//! nothing of the operator's TORON_MCP_URL / REX_SERVER / HC_SOCKET_PATH /
//! AGENT_NAME leaks in), a temp HOME + XDG + toron store, and logging
//! deny-shims for `launchctl`/`systemctl` first on PATH. A refusal case asserts
//! the shim log is EMPTY: a guard that lets the call through must fail the test,
//! never the operator's fleet. Each child is bounded and killed, and its output
//! goes to files so a stray grandchild can never hold the suite open.

use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use flywheel_test_sandbox::TestSandbox;

fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// The shared contract sandbox (bd-jp9z1.4): env_clear + temp HOME/XDG/toron,
/// the operator's routing env scrubbed, deny-shims first on PATH, and a Drop
/// that fails the test if the real supervisor was ever reached.
struct Sandbox {
    inner: TestSandbox,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            inner: TestSandbox::new(),
        }
    }

    fn root(&self) -> &std::path::Path {
        self.inner.root()
    }

    fn shim_log(&self) -> String {
        self.inner.shim_log()
    }

    /// Positional args and an explicitly clean environment.
    fn command(&self, pane: bool, pin: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = self.inner.command(flywheel());
        cmd.args(args);
        if pane {
            cmd.env("FLYWHEEL_PANE", "1");
        }
        if let Some(pin) = pin {
            cmd.env("AGENT_NAME", pin);
        }
        cmd
    }
}

struct Run {
    code: Option<i32>,
    stderr: String,
    timed_out: bool,
}

/// Run bounded: output to files (a killed child's grandchild cannot hold us),
/// and kill on the deadline so a bypassed guard fails instead of hanging.
fn run(mut cmd: Command, sandbox: &Sandbox, limit: Duration) -> Run {
    let out_path = sandbox.root().join("out.log");
    let err_path = sandbox.root().join("err.log");
    let out = File::create(&out_path).expect("out log");
    let err = File::create(&err_path).expect("err log");
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn flywheel");
    let started = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait().expect("wait on child") {
            Some(_) => break,
            None if started.elapsed() > limit => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Run {
        code: child.try_wait().ok().flatten().and_then(|s| s.code()),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
        timed_out,
    }
}

fn refused(sandbox: &Sandbox, cmd: Command, what: &str) -> Run {
    let out = run(cmd, sandbox, Duration::from_secs(20));
    assert!(
        !out.timed_out,
        "{what} did not refuse: the guard let it run (killed after 20s); stderr: {}",
        out.stderr
    );
    assert_ne!(out.code, Some(0), "{what} must be refused: {}", out.stderr);
    assert!(
        out.stderr.contains("captain-only"),
        "{what} names the rule on stderr: {}",
        out.stderr
    );
    assert!(
        sandbox.shim_log().is_empty(),
        "{what} reached a live supervisor tool: {}",
        sandbox.shim_log()
    );
    out
}

/// A crew pane that unset AGENT_NAME is not the operator: `flywheel upgrade`
/// replaces the live binary and must refuse it.
#[test]
fn upgrade_refuses_a_crew_pane_with_no_pin() {
    let sandbox = Sandbox::new();
    refused(
        &sandbox,
        sandbox.command(true, None, &["upgrade"]),
        "upgrade",
    );
}

/// The same door with a pin that no captain vouches for.
#[test]
fn upgrade_refuses_a_crew_pin() {
    let sandbox = Sandbox::new();
    refused(
        &sandbox,
        sandbox.command(true, Some("CrewPin"), &["upgrade"]),
        "upgrade",
    );
}

/// `doorbell supervise` starts (and thereby bounces) the dispatcher in a loop.
#[test]
fn doorbell_supervise_refuses_a_crew_pin() {
    let sandbox = Sandbox::new();
    refused(
        &sandbox,
        sandbox.command(
            true,
            Some("CrewPin"),
            &["doorbell", "supervise", "guard-proj"],
        ),
        "doorbell supervise",
    );
}

/// `stack install` replaces the installed flywheel/toron binaries.
#[test]
fn stack_install_refuses_a_crew_pin() {
    let sandbox = Sandbox::new();
    refused(
        &sandbox,
        sandbox.command(true, Some("CrewPin"), &["stack", "install"]),
        "stack install",
    );
}

/// `daemon start` installs/supervises the serve host and every armed doorbell
/// host with it. `--port` keeps even a bypassed run off the live ports.
#[test]
fn daemon_start_refuses_a_crew_pin() {
    let sandbox = Sandbox::new();
    refused(
        &sandbox,
        sandbox.command(
            true,
            Some("CrewPin"),
            &["daemon", "start", "--port", "39071"],
        ),
        "daemon start",
    );
}

/// The positive control: the same sandboxed binary DOES act for the operator
/// shell (no pin, no pane marker), so the refusals above are about identity,
/// never about the sandbox being broken. `daemon status` is the read-only verb.
#[test]
fn the_operator_shell_still_reaches_a_read_only_verb() {
    let sandbox = Sandbox::new();
    // `daemon status` probes the live supervisor with `launchctl print`: a read,
    // and exactly what this control wants to prove still works.
    sandbox.inner.allow_supervisor_calls();
    let out = run(
        sandbox.command(false, None, &["daemon", "status", "--port", "39071"]),
        &sandbox,
        Duration::from_secs(20),
    );
    assert!(!out.timed_out, "daemon status timed out: {}", out.stderr);
    assert!(
        !out.stderr.contains("captain-only"),
        "a read-only verb is never guarded: {}",
        out.stderr
    );
}
