//! Harness hook capability table (Phase 4b.3).
//!
//! Every shipped row is unverified: `hook` and `verified_by` are both
//! `None`. A row that names a hook without a verifier is not capable.
//! Nothing here edits an operator config or runs a harness.

/// Exact reason a hook may print. No payload is interpolated into it.
pub const HOOK_REASON: &str = "urgent steer pending: run toron next";

const HOOK_START: &str = "# flywheel-supervisor-hook start";
const HOOK_END: &str = "# flywheel-supervisor-hook end";

/// One harness's hook claim. Capability requires a verifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessCaps {
    /// Canonical harness word.
    pub harness: &'static str,
    /// Hook command, if any. `None` means not capable.
    pub hook: Option<&'static str>,
    /// Who verified the hook. `None` with `hook: Some` is an error.
    pub verified_by: Option<&'static str>,
}

impl HarnessCaps {
    /// `hook: Some` without `verified_by` is not a capable harness.
    pub fn check(self) -> Result<(), &'static str> {
        if self.hook.is_some() && self.verified_by.is_none() {
            Err("unverified harness cannot be marked capable")
        } else {
            Ok(())
        }
    }
}

/// Harnesses this repo already names. None are verified capable.
pub const TABLE: &[HarnessCaps] = &[
    HarnessCaps {
        harness: "opencode",
        hook: None,
        verified_by: None,
    },
    HarnessCaps {
        harness: "pi",
        hook: None,
        verified_by: None,
    },
    HarnessCaps {
        harness: "grok",
        hook: None,
        verified_by: None,
    },
    HarnessCaps {
        harness: "omp",
        hook: None,
        verified_by: None,
    },
    // The live fixture harness. Not a product people run. `verified_by`
    // names the test that drives it; a real harness stays `none` until its
    // own fixture does the same.
    HarnessCaps {
        harness: "fake",
        hook: Some("pre_tool_use"),
        verified_by: Some("fake_harness_denies_once_and_types_no_interrupt"),
    },
];

/// Static hook body. Runs `toron next --urgent --peek` and, only when that
/// exits 0, denies the tool call with [`HOOK_REASON`]. Peek stdout is
/// discarded so a steer body cannot reach the pane. No field is interpolated.
/// `toron next --urgent --peek` exits 0 when an urgent steer is pending and
/// non-zero when it is not; any other failure allows the tool call.
pub fn hook_script() -> &'static str {
    "#!/bin/sh\n# Constant deny. Steer text is discarded and never typed.\nif toron next --urgent --peek >/dev/null 2>&1; then\necho urgent steer pending: run toron next >&2\nexit 2\nfi\nexit 0\n"
}

/// What an urgent wake types. A capable harness with the block installed
/// keeps the urgent line and skips Interrupt. Anything else is today's
/// Interrupt plus the line. The doorbell still rings either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakePick {
    /// Hook denies the next tool call. Do not type Interrupt.
    Hook,
    /// Not capable, or the block is not installed. Interrupt plus the line.
    Interrupt,
}

/// `hook` and `verified_by` both set. An unverified row is not capable.
/// Unknown kinds are not capable.
pub fn capable(kind: &str) -> bool {
    let lower = kind.trim().to_ascii_lowercase();
    let name = match lower.as_str() {
        "oc" | "opencode" => "opencode",
        "gk" | "grok" => "grok",
        "pi" => "pi",
        "omp" => "omp",
        "fake" => "fake",
        _ => return false,
    };
    TABLE
        .iter()
        .any(|row| row.harness == name && row.hook.is_some() && row.verified_by.is_some())
}

/// Interrupt unless this harness is capable and the hook block is installed.
pub fn interrupt_for(kind: &str, installed: bool) -> bool {
    !matches!(
        if capable(kind) && installed {
            WakePick::Hook
        } else {
            WakePick::Interrupt
        },
        WakePick::Hook
    )
}

/// True when `path` contains the marked block.
pub fn hook_installed(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.contains(HOOK_START) && text.contains(HOOK_END))
        .unwrap_or(false)
}

/// The fixture and an operator override name the file here. Unset means
/// no hook file, so every pane keeps Interrupt.
pub fn installed_from_env() -> bool {
    std::env::var("FLYWHEEL_SUPERVISOR_HOOK_FILE")
        .ok()
        .is_some_and(|p| hook_installed(std::path::Path::new(&p)))
}

/// What launch does with the hook file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchHook {
    /// Not a capable harness, or no file was named.
    Skip,
    /// `FLYWHEEL_ALLOW_EXEC=0`. The file is untouched.
    Suppressed,
    /// The marked block is in the file.
    Installed,
    /// The write failed. The caller keeps Interrupt.
    Failed(String),
}

/// Write the marked block when `kind` is capable and `allow_exec` is set.
/// `Ok(false)` from a refused write is [`LaunchHook::Suppressed`].
pub fn plan_launch_hook(
    kind: &str,
    path: Option<&std::path::Path>,
    allow_exec: bool,
) -> LaunchHook {
    let Some(path) = path else {
        return LaunchHook::Skip;
    };
    if !capable(kind) {
        return LaunchHook::Skip;
    }
    if !allow_exec {
        return LaunchHook::Suppressed;
    }
    match install_at(path) {
        Ok(()) => LaunchHook::Installed,
        Err(e) => LaunchHook::Failed(e.to_string()),
    }
}

/// Append the marked block. A second call leaves the bytes alone.
pub fn install_at(path: &std::path::Path) -> std::io::Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let next = install_block(&existing);
    if next == existing {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
    let tmp = path.with_extension("hook.tmp");
    std::fs::write(&tmp, next)?;
    std::fs::rename(&tmp, path)?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(path, mode);
    }
    Ok(())
}

/// Remove the marked block. Bytes outside it stay.
pub fn uninstall_at(path: &std::path::Path) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path)?;
    let next = uninstall_block(&existing);
    if next == existing {
        return Ok(());
    }
    let mode = std::fs::metadata(path).ok().map(|m| m.permissions());
    std::fs::write(path, next)?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(path, mode);
    }
    Ok(())
}

/// Drop the block named by `FLYWHEEL_SUPERVISOR_HOOK_FILE`, if that file
/// has one. Unset is a no-op. Called when a child exits and exec is allowed.
pub fn uninstall_if_configured() {
    let Ok(path) = std::env::var("FLYWHEEL_SUPERVISOR_HOOK_FILE") else {
        return;
    };
    let _ = uninstall_at(std::path::Path::new(&path));
}

/// One doctor line for a file. Missing and unreadable are "not installed".
pub fn doctor_line(path: Option<&std::path::Path>) -> &'static str {
    let installed = path.is_some_and(hook_installed);
    hook_report(installed)
}

/// Append the marked hook block to `existing`. Does not touch other lines.
pub fn install_block(existing: &str) -> String {
    if existing.contains(HOOK_START) && existing.contains(HOOK_END) {
        return existing.to_string();
    }
    let mut out = String::new();
    out.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(HOOK_START);
    out.push('\n');
    out.push_str(hook_script());
    if !hook_script().ends_with('\n') {
        out.push('\n');
    }
    out.push_str(HOOK_END);
    out.push('\n');
    out
}

/// Remove the marked block. Text outside it is byte-identical.
pub fn uninstall_block(text: &str) -> String {
    let Some(start) = text.find(HOOK_START) else {
        return text.to_string();
    };
    let Some(rel) = text[start..].find(HOOK_END) else {
        return text.to_string();
    };
    let mut end = start + rel + HOOK_END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    let mut out = String::new();
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out
}

/// One doctor line. The loop doctor stays untouched: it talks to live
/// services, and this phase does not mark a harness capable.
pub fn hook_report(installed: bool) -> &'static str {
    if installed {
        "supervisor hook installed; production harnesses unverified"
    } else {
        "supervisor hook not installed; production harnesses unverified"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unverified_harness_cannot_be_marked_capable() {
        let bad = HarnessCaps {
            harness: "opencode",
            hook: Some("echo hi"),
            verified_by: None,
        };
        assert!(bad.check().is_err());
        let src = include_str!("harness_caps.rs");
        for row in TABLE {
            row.check().expect(row.harness);
            match row.harness {
                "opencode" | "pi" | "grok" | "omp" => {
                    assert!(row.hook.is_none() && row.verified_by.is_none());
                }
                "fake" => {
                    assert_eq!(row.hook, Some("pre_tool_use"));
                    let v = row.verified_by.expect("fake names its fixture");
                    assert!(
                        src.contains(&format!("fn {v}(")),
                        "{v} is not a test in this file"
                    );
                }
                other => panic!("unexpected harness {other}"),
            }
        }
        assert!(!capable("opencode"));
        assert!(!capable("oc"));
        assert!(capable("fake"));
        assert!(interrupt_for("opencode", true));
        assert!(interrupt_for("fake", false), "no block, so interrupt stays");
        assert!(!interrupt_for("fake", true));
    }

    #[test]
    fn the_hook_script_names_the_reason_and_does_not_interpolate_a_payload() {
        let s = hook_script();
        assert!(s.contains(HOOK_REASON));
        assert!(s.contains("toron next --urgent --peek"));
        assert!(!s.contains('{'), "no format payload");
        assert!(!s.contains("$"), "no shell payload");
    }

    #[test]
    fn uninstall_removes_only_the_marked_block() {
        let before = "# operator hook: keep me\n";
        let after = "# operator hook: and me\n";
        let file = format!("{before}{}{after}", install_block(""));
        assert_eq!(uninstall_block(&file), format!("{before}{after}"));
        let fixture = "operator hook before\noperator hook after\n";
        assert_eq!(uninstall_block(&install_block(fixture)), fixture);
        assert_eq!(uninstall_block(fixture), fixture);
        assert_eq!(
            install_block(&install_block(fixture)),
            install_block(fixture)
        );
    }

    #[test]
    fn hook_report_names_install_and_never_a_capable_harness() {
        assert!(hook_report(false).contains("not installed"));
        assert!(hook_report(true).contains("installed"));
        assert!(hook_report(true).contains("production harnesses unverified"));
        assert!(!hook_report(true).contains("opencode"));
    }

    #[test]
    fn install_round_trip_leaves_the_operator_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks");
        let original = "# operator hook: keep me\nkeep-this-line\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            plan_launch_hook("fake", Some(&path), false),
            LaunchHook::Suppressed
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            plan_launch_hook("opencode", Some(&path), true),
            LaunchHook::Skip
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            plan_launch_hook("fake", Some(&path), true),
            LaunchHook::Installed
        );
        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(installed.contains(HOOK_REASON));
        assert!(installed.starts_with(original));
        uninstall_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(doctor_line(Some(&path)), hook_report(false));
    }

    fn run_hook(dir: &std::path::Path) -> (i32, String, String) {
        let script = std::fs::read_to_string(dir.join("hooks")).unwrap();
        let start = script.find(HOOK_START).unwrap() + HOOK_START.len();
        let end = script[start..].find(HOOK_END).unwrap() + start;
        let body = script[start..end].trim();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(body)
            .current_dir(dir)
            .env("PATH", dir)
            .env_remove("TORON_MCP_URL")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Fake harness: tool steps run the installed hook. An urgent peek
    /// denies once with the constant reason and types no Interrupt and no
    /// payload. `toron next` clears it. A non-capable kind still interrupts.
    #[test]
    fn fake_harness_denies_once_and_types_no_interrupt() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("toron");
        std::fs::write(
            &stub,
            "#!/bin/sh\nif [ \"$*\" = \"next --urgent --peek\" ]; then\n  if [ -f pending ]; then echo SECRET_DELTA_goal; exit 0; fi\n  exit 1\nfi\nif [ \"$*\" = \"next\" ]; then rm -f pending; exit 0; fi\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&stub).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&stub, p).unwrap();
        }
        let hooks = dir.path().join("hooks");
        std::fs::write(&hooks, "# operator hook: keep me\n").unwrap();
        assert_eq!(
            plan_launch_hook("fake", Some(&hooks), true),
            LaunchHook::Installed
        );

        let (code, stdout, stderr) = run_hook(dir.path());
        assert_eq!(code, 0, "no pending steer allows the tool: {stderr}");
        assert!(stdout.is_empty());
        let mut scroll = String::from("tool\n");

        std::fs::write(dir.path().join("pending"), "steer body\n").unwrap();
        let (code, stdout, stderr) = run_hook(dir.path());
        assert_eq!(code, 2, "{stderr}");
        assert_eq!(stderr.trim(), HOOK_REASON);
        assert!(
            !stdout.contains("SECRET") && !stderr.contains("SECRET"),
            "payload leaked: {stdout:?} {stderr:?}"
        );
        scroll.push_str("denied\n");
        assert!(!scroll.contains("Interrupt"));
        assert!(!scroll.contains("Escape"));

        let cleared = std::process::Command::new(dir.path().join("toron"))
            .arg("next")
            .current_dir(dir.path())
            .env_remove("TORON_MCP_URL")
            .status()
            .unwrap();
        assert!(cleared.success());
        let (code, _, _) = run_hook(dir.path());
        assert_eq!(code, 0, "after toron next the next tool proceeds");
        scroll.push_str("tool\n");
        assert_eq!(scroll.matches("denied").count(), 1);

        let pane = inbox_wake_recorder();
        crate::inbox_ports::ring_wake(
            &pane,
            "w1:fake",
            7,
            &crate::pane_write::wake_line(
                crate::pane_write::Wm(7),
                crate::pane_write::Urgency::Normal,
            ),
            toron_core::wake::Urgency::Urgent,
            interrupt_for("fake", true),
        )
        .unwrap();
        crate::inbox_ports::ring_wake(
            &pane,
            "w1:pi",
            7,
            &crate::pane_write::wake_line(
                crate::pane_write::Wm(7),
                crate::pane_write::Urgency::Normal,
            ),
            toron_core::wake::Urgency::Urgent,
            interrupt_for("pi", true),
        )
        .unwrap();
        let writes = pane.lines();
        let fake: Vec<_> = writes.iter().filter(|w| w.starts_with("w1:fake")).collect();
        let pi: Vec<_> = writes.iter().filter(|w| w.starts_with("w1:pi")).collect();
        assert!(
            fake.iter().all(|w| !w.contains("keys")),
            "capable hook types no Interrupt: {fake:?}"
        );
        assert!(
            fake.iter().any(|w| w.contains("URGENT steer")),
            "the ring stays: {fake:?}"
        );
        assert_eq!(
            pi.iter().filter(|w| w.contains("keys")).count(),
            1,
            "a non-capable harness still interrupts once: {pi:?}"
        );
        assert!(uninstall_at(&hooks).is_ok());
        assert_eq!(
            std::fs::read_to_string(&hooks).unwrap(),
            "# operator hook: keep me\n"
        );
    }

    fn inbox_wake_recorder() -> WakeRec {
        WakeRec::default()
    }

    #[derive(Default)]
    struct WakeRec(std::sync::Mutex<Vec<String>>);

    impl WakeRec {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl crate::pane_write::PaneTyper for WakeRec {
        fn prompt(&self, target: &str, line: &str) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{target} prompt {line}"));
            Ok(())
        }
        fn keys(&self, target: &str, keys: &[&str]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{target} keys {}", keys.join("+")));
            Ok(())
        }
        fn close(&self, target: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("{target} close"));
            Ok(())
        }
    }
}
