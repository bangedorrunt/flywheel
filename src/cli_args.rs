// governed-by: ADR-0016
//! Pure command-line normalization before clap parsing.
//!
//! This boundary owns compatibility rewrites only. It does not inspect the
//! filesystem, environment, clocks, or process state.

/// Rewrite the legacy bare form `flywheel loop <project> [flags]` into the
/// canonical `flywheel loop run <project> [flags]` before clap parsing.
///
/// The same rule applies to `loop-mode`, while explicit subcommands and help
/// flags remain byte-for-byte unchanged.
pub(crate) fn rewrite_legacy_loop_args(args: &[String]) -> Vec<String> {
    const SUBCOMMANDS: [&str; 8] = [
        "run",
        "supervise",
        "status",
        "list",
        "prune",
        "doctor",
        "replay",
        "why",
    ];
    if args.len() >= 2
        && matches!(args[1].as_str(), "loop" | "loop-mode")
        && let Some(next) = args.get(2)
        && !SUBCOMMANDS.contains(&next.as_str())
        && !next.starts_with('-')
    {
        let mut out = args.to_vec();
        out.insert(2, "run".to_string());
        return out;
    }
    args.to_vec()
}

#[cfg(test)]
mod tests {
    use super::rewrite_legacy_loop_args;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn rewrites_bare_legacy_projects_for_both_names() {
        assert_eq!(
            rewrite_legacy_loop_args(&args(&["flywheel", "loop", "project"])),
            args(&["flywheel", "loop", "run", "project"])
        );
        assert_eq!(
            rewrite_legacy_loop_args(&args(&["flywheel", "loop-mode", "project"])),
            args(&["flywheel", "loop-mode", "run", "project"])
        );
    }

    #[test]
    fn preserves_explicit_subcommands_and_help() {
        for subcommand in [
            "run",
            "supervise",
            "status",
            "list",
            "prune",
            "doctor",
            "replay",
            "why",
        ] {
            let input = args(&["flywheel", "loop", subcommand]);
            assert_eq!(rewrite_legacy_loop_args(&input), input);
        }
        for flag in ["--help", "-h"] {
            let input = args(&["flywheel", "loop", flag]);
            assert_eq!(rewrite_legacy_loop_args(&input), input);
        }
    }

    #[test]
    fn leaves_other_commands_unchanged() {
        let input = args(&["flywheel", "send", "project"]);
        assert_eq!(rewrite_legacy_loop_args(&input), input);
    }
}
