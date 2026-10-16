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
];

/// Static hook body. Contains [`HOOK_REASON`] and `toron next --urgent --peek`.
/// No payload is interpolated.
pub fn hook_script() -> &'static str {
    "urgent steer pending: run toron next\ntoron next --urgent --peek\n"
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
        "supervisor hook installed; no harness verified capable"
    } else {
        "supervisor hook not installed; no harness verified capable"
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
        for row in TABLE {
            row.check().expect(row.harness);
            assert!(row.hook.is_none() && row.verified_by.is_none());
        }
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
        assert!(hook_report(true).contains("no harness verified capable"));
        assert!(!hook_report(true).contains("opencode"));
    }
}
