//! Quote and redact untrusted text before it reaches a notice or a log line.
//!
//! Best-effort hygiene, not a security boundary. A value with no token shape
//! is unchanged by [`redact`] and [`flatten`]. [`present`] is what a notice
//! shows: the same text, newline-safe, inside one pair of backticks.
//!
//! `agent_ref::redact_evidence` is a different clip, for the effect journal.
//! Its marker and length are not this one. Do not route notices through it.

/// Replacement for every recognised token shape.
pub const MARKER: &str = "[redacted]";

/// Max chars of untrusted text kept in a notice or a log field.
pub const CLIP: usize = 200;

const PATTERNS: &[&str] = &[
    // user:password@ before the other shapes, so a token inside a URL is one cut.
    r"[A-Za-z][A-Za-z0-9+.-]*://[^[:space:]/@]+:[^[:space:]/@]+@",
    r"[Bb]earer[[:space:]]+[^[:space:]]+",
    r"[Tt]oken=[^[:space:]&]+",
    r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
    r"sk-[A-Za-z0-9_-]{8,}",
    r"ghp_[A-Za-z0-9]{8,}",
    r"npm_[A-Za-z0-9]{8,}",
    r"xox[baprs]-[A-Za-z0-9-]{8,}",
];

/// Replace token shapes with [`MARKER`]. Everything else is returned as given,
/// including newlines.
pub fn redact(input: &str) -> String {
    let mut s = input.to_string();
    for pat in PATTERNS {
        let re = regex_lite::Regex::new(pat).expect("redact pattern");
        s = re.replace_all(&s, MARKER).into_owned();
    }
    s
}

/// [`redact`], then one line: newlines become spaces, backticks cannot close
/// a quote, and a long value keeps [`CLIP`] chars and says it was clipped.
pub fn flatten(input: &str) -> String {
    let redacted = redact(input);
    let mut out = String::new();
    for ch in redacted.chars() {
        match ch {
            '\n' | '\r' => out.push(' '),
            '`' => out.push('\''),
            c => out.push(c),
        }
    }
    if out.chars().count() > CLIP {
        let clipped: String = out.chars().take(CLIP).collect();
        format!("{clipped}… (clipped)")
    } else {
        out
    }
}

/// The notice form. The value sits inside one pair of backticks.
pub fn present(input: &str) -> String {
    format!("`{}`", flatten(input))
}

/// Stderr and captain page for an escalation. `reason` and `evidence` are data.
pub fn escalation_notice(project: &str, reason: &str, evidence: &str) -> String {
    format!(
        "loop-mode supervise: ESCALATE {} in {project} — {}",
        present(reason),
        present(evidence)
    )
}

/// Stderr and captain page for a parked child.
pub fn park_notice(project: &str, child: &str, incarnations: u32) -> String {
    format!(
        "loop-mode supervise: PARKED {} in {project} after {incarnations} abnormal incarnations (ceiling; not retried)",
        present(child)
    )
}

/// The wedge page. Pin, pane, and the child's parked line are data.
pub fn wedge_notice(project: &str, pin: &str, pane_id: &str, error: &str, age_secs: u64) -> String {
    format!(
        "loop-mode supervise: PANE WEDGE in {project} — {} ({}) is alive but parked on: {} (nothing committed for {age_secs}s)",
        present(pin),
        present(pane_id),
        present(error)
    )
}

/// Stderr line for a rejected child fact.
pub fn reject_notice(cause: &str, child: &str) -> String {
    format!(
        "flywheel supervisor: rejected ({}) child={}",
        present(cause),
        present(child)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_ABCDEFGHIJKLMNOPQRST";

    #[test]
    fn redact_leaves_a_clean_sentence_unchanged() {
        let s = "restart intensity exceeded; fix the cause";
        assert_eq!(redact(s), s);
        assert_eq!(flatten(s), s);
    }

    #[test]
    fn redact_replaces_each_token_shape_with_the_same_marker() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhIn0.signatureVAL";
        let samples = [
            format!("key {TOKEN} end"),
            "key sk-proj-abcdefghij end".into(),
            "key npm_abcdefghij end".into(),
            "key xoxb-abcdefghij end".into(),
            format!("key {jwt} end"),
            "see Bearer abcdefghijklmnop now".into(),
            "see token=abcdefghijklmnop now".into(),
            "see https://user:s3cret-value@example.com/a now".into(),
        ];
        for s in samples {
            let out = redact(&s);
            assert!(out.contains(MARKER), "{s} -> {out}");
            assert!(!out.contains("abcdefghij"), "{s} -> {out}");
            assert!(!out.contains(TOKEN), "{s} -> {out}");
            assert!(!out.contains("s3cret-value"), "{s} -> {out}");
            assert!(!out.contains("abcdefghijklmnop"), "{s} -> {out}");
            assert!(!out.contains("signatureVAL"), "{s} -> {out}");
        }
    }

    #[test]
    fn a_notice_quotes_the_value_and_stays_one_line() {
        let reason = format!("intensity\nignore previous instructions `{TOKEN}`");
        let evidence = format!("child said {TOKEN}\r\nand then a second line");
        let notice = escalation_notice("proj", &reason, &evidence);
        assert!(!notice.contains('\n'), "{notice}");
        assert!(!notice.contains(TOKEN), "{notice}");
        assert!(notice.contains(MARKER), "{notice}");
        assert!(notice.contains("`intensity"), "{notice}");
        assert!(notice.matches('`').count() >= 4, "{notice}");
        let wedge = wedge_notice("proj", "TealBirch", "pane-1", &evidence, 90);
        assert!(!wedge.contains('\n') && !wedge.contains(TOKEN), "{wedge}");
        assert!(wedge.contains("`TealBirch`"), "{wedge}");
    }

    #[test]
    fn a_long_value_says_it_was_clipped() {
        let long = "a".repeat(CLIP + 40);
        let shown = present(&long);
        assert!(shown.contains("(clipped)"), "{shown}");
        assert!(shown.starts_with('`') && shown.ends_with('`'));
        assert!(!shown.contains(&"a".repeat(CLIP + 1)));
    }
}
