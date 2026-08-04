//! Output helpers: TTY-aware color, status painting, JSON vs human rendering.

use std::io::IsTerminal;

use crate::herdr::AgentStatus;

/// Whether ANSI color should be emitted for the given stream.
pub fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn paint(s: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn status_paint(status: AgentStatus) -> String {
    let s = status.as_str();
    match status {
        AgentStatus::Idle => paint(s, "32"),
        AgentStatus::Working => paint(s, "36"),
        AgentStatus::Blocked => paint(s, "31;1"),
        AgentStatus::Done => paint(s, "90"),
        AgentStatus::Unknown => paint(s, "90"),
    }
}

pub fn kind_paint(s: &str) -> String {
    paint(s, "33")
}

pub fn dim(s: &str) -> String {
    paint(s, "90")
}

pub fn error(s: &str) -> String {
    paint(s, "31;1")
}

pub fn warn(s: &str) -> String {
    paint(s, "33")
}

pub fn ok(s: &str) -> String {
    paint(s, "32")
}

/// Print a fatal error to stderr with a hint, exit 1.
pub fn die(err: &anyhow::Error, hint: Option<&str>) -> ! {
    eprintln!("{}", error(&format!("flywheel: {err:#}")));
    if let Some(h) = hint {
        eprintln!("{}", dim(h));
    }
    std::process::exit(1);
}

/// Print a JSON value to stdout.
pub fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warn_marks_text() {
        // NO_COLOR forces the plain form; the text survives either way
        std::env::set_var("NO_COLOR", "1");
        assert_eq!(warn("drift"), "drift");
        std::env::remove_var("NO_COLOR");
        let painted = warn("drift");
        assert!(painted.contains("drift"));
        assert!(painted.contains("\x1b[33m") || !color_enabled());
    }
}
