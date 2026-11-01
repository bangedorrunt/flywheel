#![allow(dead_code)]
// governed-by: ADR-0019 D2 (amendment 2026-11-01: mikasa-side pull-fresh poller)
//! Mikasa-side pull-fresh poller for a hosted Grok Bot captain.
//!
//! A hosted Grok Bot cannot poll `toron mail inbox` every 60s from its own
//! box (approved Shell only, bot-to-bot via SendToAgent). Freshness is the
//! durable reader cursor (D2), so mikasa advances that cursor by reading
//! the inbox *as the pin*. This module is pure argv + cadence; the CLI
//! interpreter shells `toron`. Effects return as process status — no I/O
//! inside the pure helpers.

/// Hosted poll cadence from ADR-0019 D2 (one late poll must not drop the
/// alias under the 120s freshness window).
pub const PULL_FRESH_POLL_SECS: u64 = 60;

/// Pure: is it time for another tick?
pub fn due(last_tick_ms: Option<i64>, now_ms: i64) -> bool {
    match last_tick_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= (PULL_FRESH_POLL_SECS as i64) * 1000,
    }
}

/// Pure argv for one tick. `--since last` advances the durable high-water
/// for the pin; `--limit 1` keeps the page small. The read itself is the
/// freshness signal (ADR-0019 D2).
pub fn tick_argv(project: &str, pin: &str) -> Vec<String> {
    vec![
        "mail".into(),
        "inbox".into(),
        "--project".into(),
        project.into(),
        "--as".into(),
        pin.into(),
        "--since".into(),
        "last".into(),
        "--limit".into(),
        "1".into(),
        "--json".into(),
    ]
}

/// Run one tick through an injected command runner. Returns the child's
/// exit status code. The runner is the effect boundary (elm-effect ADR-0002).
pub fn run_tick<F>(project: &str, pin: &str, mut runner: F) -> std::io::Result<i32>
where
    F: FnMut(&str, &[String]) -> std::io::Result<i32>,
{
    let argv = tick_argv(project, pin);
    runner("toron", &argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_respects_sixty_second_cadence() {
        assert!(due(None, 0));
        assert!(!due(Some(0), 59_999));
        assert!(due(Some(0), 60_000));
        // Mutant: a 15s cadence would still be due at 60s, but would also
        // be due at 15s — pin the lower bound.
        assert!(!due(Some(0), 15_000));
    }

    #[test]
    fn tick_argv_is_inbox_as_pin_since_last() {
        let argv = tick_argv("flywheel", "HostedGrok");
        assert_eq!(
            argv,
            [
                "mail",
                "inbox",
                "--project",
                "flywheel",
                "--as",
                "HostedGrok",
                "--since",
                "last",
                "--limit",
                "1",
                "--json",
            ]
        );
        // Mutant: dropping --as would poll as the operator, not the pin.
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--as" && w[1] == "HostedGrok")
        );
    }

    #[test]
    fn run_tick_invokes_toron_with_argv() {
        let mut seen: Option<(String, Vec<String>)> = None;
        let code = run_tick("flywheel", "HostedGrok", |bin, argv| {
            seen = Some((bin.to_string(), argv.to_vec()));
            Ok(0)
        })
        .expect("runner ok");
        assert_eq!(code, 0);
        let (bin, argv) = seen.expect("runner called");
        assert_eq!(bin, "toron");
        assert_eq!(argv, tick_argv("flywheel", "HostedGrok"));
    }
}
