//! Output helpers: TTY-aware color, status painting, JSON vs human rendering.
//! hacker-night neon-on-void theme — semantic slots with 3-tier degrade:
//! truecolor (COLORTERM=truecolor/24bit) → 256-color → 16 ANSI foundation.
//! 16 ANSI remains the base so Ghostty's palette映射 is correct; truecolor
//! guarantees neon-on-void on any terminal.

use std::io::IsTerminal;

use crate::herdr::AgentStatus;

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
        Some("truecolor" | "24bit")
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
    /// Palette completeness: every slot has an rgb + ANSI arm in the three
    /// tables below, and widget code paints from this slot set — an unused
    /// background slot is intentional surface, not dead logic.
    #[allow(dead_code)]
    BgBase,
    #[allow(dead_code)]
    BgSurface,
    BgOverlay,
    AccentCyan,
    AccentLav,
    Success,
    Info,
    Warning,
    Error,
}

pub fn slot_rgb(slot: Slot) -> (u8, u8, u8) {
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
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("serde_json::Value serializes infallibly")
    );
}

#[cfg(test)]
mod bd_confirm_tests {
    use super::confirm_destructive;

    fn targets() -> Vec<String> {
        vec!["a.yaml".into(), "b.yaml".into()]
    }

    /// Nothing to destroy must never block. An agent running `seed` on a
    /// healthy catalog should not be asked to type a word.
    #[test]
    fn no_targets_never_prompts() {
        confirm_destructive("overwrite", &[], false, false).expect("empty is allowed");
    }

    #[test]
    fn yes_accepts_without_prompting() {
        confirm_destructive("overwrite", &targets(), true, false).expect("--yes");
    }

    /// A prompt nobody can answer is not consent. Off a TTY this must REFUSE,
    /// not silently proceed — that is the difference between a gate and a
    /// suggestion.
    #[test]
    fn no_tty_without_yes_refuses() {
        let err = confirm_destructive("overwrite", &targets(), false, false)
            .expect_err("must refuse with no TTY");
        let msg = format!("{err:#}");
        assert!(msg.contains("no TTY"), "the reason must be legible: {msg}");
        assert!(
            msg.contains("--yes"),
            "the refusal must name the fix: {msg}"
        );
    }

    /// `--json` is the agent path. An agent gets a machine-readable refusal it
    /// can branch on, never an interactive hang.
    #[test]
    fn json_mode_refuses_with_a_machine_readable_error() {
        let err = confirm_destructive("overwrite", &targets(), false, true)
            .expect_err("json must refuse without --yes");
        assert!(format!("{err:#}").contains("--yes"));
    }
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

/// Emoji + color badge for a catalog state. Agent-native: the glyph survives
/// `NO_COLOR` and a pipe, so `grep '⚠'` still finds the rows that need work.
///
/// The emoji is load-bearing, not decoration. bd-3o5t's drift printed
/// "catalog drift clean" over 40 damaged files; a row that needs a human now
/// looks different at a glance AND greps differently from a row that does not.
pub fn catalog_badge(status: &str) -> String {
    let (glyph, text) = match status {
        "current" => ("✅", "current"),
        "stale" => ("⚠️", "stale"),
        "missing" => ("❌", "missing"),
        "custom" => ("🧩", "custom"),
        other => ("•", other),
    };
    match status {
        "current" => ok(&format!("{glyph} {text}")),
        "stale" => warn(&format!("{glyph} {text}")),
        "missing" | "custom" => accent_lav(&format!("{glyph} {text}")),
        _ => dim(&format!("{glyph} {text}")),
    }
}

/// Fail-closed confirmation for an action that rewrites the operator's config.
///
/// bd-3o5t follow-on. Two rules, both learned the hard way:
/// 1. **Off a TTY it refuses** unless `--yes`. A prompt nobody can answer must
///    not read as consent; an unattended agent must pass `--yes` deliberately.
/// 2. **It names the blast radius** — every file, listed — because "overwrite
///    40 files?" is a question nobody can answer and "overwrite these 40?" is.
pub fn confirm_destructive(
    what: &str,
    targets: &[String],
    yes: bool,
    json: bool,
) -> anyhow::Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    if !yes {
        if json {
            anyhow::bail!(
                "refusing to {what} {} file(s) without --yes: {}",
                targets.len(),
                targets.join(", ")
            );
        }
        if !std::io::stdin().is_terminal() {
            // Forty filenames inline is a wall of text that hides the one fact
            // that matters (it refused, and here is the fix). Print a few, point
            // at --dry-run for the rest.
            anyhow::bail!(
                "refusing to {what} {} file(s): no TTY to confirm on. \
                 Re-run with --yes to accept, or --dry-run to see the full list. \
                 First few: {}",
                targets.len(),
                targets
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        eprintln!(
            "{}",
            warn(&format!(
                "\n{} {} {} file(s) in your config directory:",
                "⚠️",
                what,
                targets.len()
            ))
        );
        for t in targets {
            eprintln!("   {}", dim(t));
        }
        eprint!("{}", accent(&format!("\nType {} to proceed: ", what)));
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != what {
            anyhow::bail!("aborted: confirmation did not match {what:?}");
        }
    }
    Ok(())
}
