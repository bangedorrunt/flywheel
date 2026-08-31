//! Output helpers: TTY-aware color, status painting, JSON vs human rendering.
//! hacker-night neon-on-void theme — semantic slots with 3-tier degrade:
//! truecolor (COLORTERM=truecolor/24bit) → 256-color → 16 ANSI foundation.
//! 16 ANSI remains the base so Ghostty's palette映射 is correct; truecolor
//! guarantees neon-on-void on any terminal.

use std::io::IsTerminal;

use crate::herdr_shim::AgentStatus;

// ---------------------------------------------------------------------------
// color capability detection
// ---------------------------------------------------------------------------

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

fn truecolor_enabled() -> bool {
    if !color_enabled() {
        return false;
    }
    matches!(
        std::env::var("COLORTERM")
            .ok()
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("truecolor") | Some("24bit")
    )
}

fn use_256() -> bool {
    if !color_enabled() || truecolor_enabled() {
        return false;
    }
    // TERM contains 256color → 256 palette available; otherwise fall back to 16
    std::env::var("TERM")
        .map(|t| t.contains("256color"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// hacker-night palette — Ghostty v2 neon-on-void
// ---------------------------------------------------------------------------

/// Semantic color slots — widget code references these, never raw hex/ANSI.
#[derive(Debug, Clone, Copy)]
pub enum Slot {
    FgDefault,
    FgMuted,
    FgEmphasis,
    BgBase,
    BgSurface,
    BgOverlay,
    AccentCyan,
    AccentLav,
    Success,
    Info,
    Warning,
    Error,
}

fn slot_rgb(slot: Slot) -> (u8, u8, u8) {
    match slot {
        Slot::FgDefault => (0xc8, 0xd0, 0xe8),
        Slot::FgMuted => (0x56, 0x5f, 0x89),
        Slot::FgEmphasis => (0xed, 0xed, 0xed),
        Slot::BgBase => (0x08, 0x08, 0x0e),
        Slot::BgSurface => (0x12, 0x12, 0x2a),
        Slot::BgOverlay => (0x1a, 0x1a, 0x3e),
        Slot::AccentCyan => (0x0d, 0xb9, 0xd7), // vivid cyan #0db9d7
        Slot::AccentLav => (0xbb, 0x9a, 0xf7),  // electric lavender
        Slot::Success => (0x73, 0xda, 0xca),    // neon mint
        Slot::Info => (0x89, 0xdd, 0xff),       // ice cyan — working
        Slot::Warning => (0xe0, 0xaf, 0x68),    // amber gold
        Slot::Error => (0xf7, 0x76, 0x8e),      // hot pink-red (AA on void)
    }
}

/// Nearest xterm-256 index for each slot (hand-picked, not auto-quantized).
fn slot_256(slot: Slot) -> &'static str {
    match slot {
        Slot::FgDefault => "38;5;189", // lavender-grey approx
        Slot::FgMuted => "38;5;60",    // muted slate
        Slot::FgEmphasis => "38;5;255",
        Slot::BgBase => "48;5;233",
        Slot::BgSurface => "48;5;235",
        Slot::BgOverlay => "48;5;236",
        Slot::AccentCyan => "38;5;44", // vivid cyan
        Slot::AccentLav => "38;5;141",
        Slot::Success => "38;5;79",  // mint
        Slot::Info => "38;5;117",    // ice
        Slot::Warning => "38;5;180", // amber
        Slot::Error => "38;5;210",   // pink-red
    }
}

/// 16 ANSI fallback — Ghostty maps these to hacker-night hues correctly.
fn slot_ansi(slot: Slot) -> &'static str {
    match slot {
        Slot::FgDefault => "37",  // white → #c8d0e8 via palette 7
        Slot::FgMuted => "90",    // bright black → #565f89
        Slot::FgEmphasis => "97", // bright white
        Slot::BgBase => "40",     // black bg — rarely used directly
        Slot::BgSurface => "40",
        Slot::BgOverlay => "48;5;236",
        Slot::AccentCyan => "36", // cyan → #0db9d7 via palette 6
        Slot::AccentLav => "35",  // magenta → #bb9af7 via palette 5
        Slot::Success => "32",    // green → #73daca via palette 2
        Slot::Info => "96",       // bright cyan → #89ddff via palette 12
        Slot::Warning => "33",    // yellow → #e0af68 via palette 3
        Slot::Error => "91",      // bright red fallback for error
    }
}

fn paint_slot(s: &str, slot: Slot) -> String {
    if !color_enabled() {
        return s.to_string();
    }
    if truecolor_enabled() {
        let (r, g, b) = slot_rgb(slot);
        return format!("\x1b[38;2;{r};{g};{b}m{s}\x1b[0m");
    }
    if use_256() {
        return format!("\x1b[{}m{s}\x1b[0m", slot_256(slot));
    }
    format!("\x1b[{}m{s}\x1b[0m", slot_ansi(slot))
}

fn paint(s: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// public paint helpers — now semantic, still backwards-compatible names
// ---------------------------------------------------------------------------

pub fn status_paint(status: AgentStatus) -> String {
    let s = status.as_str();
    match status {
        AgentStatus::Idle => paint_slot(s, Slot::Success),
        AgentStatus::Working => paint_slot(s, Slot::Info),
        AgentStatus::Blocked => paint_slot(&format!("\x1b[1m{s}\x1b[0m"), Slot::Error),
        AgentStatus::Done => paint_slot(s, Slot::FgMuted),
        AgentStatus::Unknown => paint_slot(s, Slot::FgMuted),
    }
}

pub fn kind_paint(s: &str) -> String {
    paint_slot(s, Slot::Warning)
}

pub fn dim(s: &str) -> String {
    paint_slot(s, Slot::FgMuted)
}

pub fn error(s: &str) -> String {
    paint_slot(&format!("\x1b[1m{s}\x1b[0m"), Slot::Error)
}

pub fn warn(s: &str) -> String {
    paint_slot(s, Slot::Warning)
}

pub fn ok(s: &str) -> String {
    paint_slot(s, Slot::Success)
}

/// Accent cyan — focus, tabs, doorbell, header highlights.
pub fn accent(s: &str) -> String {
    paint_slot(s, Slot::AccentCyan)
}
/// Lavender secondary accent.
pub fn accent_lav(s: &str) -> String {
    paint_slot(s, Slot::AccentLav)
}
/// Muted helper (alias for dim, semantic name).
pub fn muted(s: &str) -> String {
    paint_slot(s, Slot::FgMuted)
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
        // NO_COLOR forces the plain form; the text survives either way.
        // Env mutation is process-global — serialize under the crate-wide
        // env lock (193.2).
        let _g = crate::config::lock_env();
        unsafe { std::env::set_var("NO_COLOR", "1") };
        assert_eq!(warn("drift"), "drift");
        unsafe { std::env::remove_var("NO_COLOR") };
        let painted = warn("drift");
        assert!(painted.contains("drift"));
        assert!(painted.contains("\x1b[33m") || !color_enabled());
    }
}
