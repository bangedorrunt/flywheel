//! `flywheel principles` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum PrinciplesCmd {
    /// print the 21 canonical principle names (one per line)
    List,
    /// print a principle's full when+rule (unknown name -> non-zero exit +
    /// a suggestion)
    Show { name: String },
}

/// ADR-0019 Spec 5/6 `flywheel principles`: the compiled-in 21 leaves.
/// `list` prints exactly the 21 canonical names; `show <name>` prints the
/// full when+rule (unknown name -> non-zero exit + a suggestion).
pub(crate) fn cmd_principles(cmd: &PrinciplesCmd) -> Result<()> {
    match cmd {
        PrinciplesCmd::List => {
            for p in crate::principles::PRINCIPLES {
                println!("{}", p.name);
            }
            Ok(())
        }
        PrinciplesCmd::Show { name } => {
            if let Some(p) = crate::principles::show(name) {
                println!("{}", p.name);
                println!("WHEN {}", p.when);
                println!("RULE {}", p.rule);
                Ok(())
            } else {
                let suggestion = crate::principles::suggest(name)
                    .map(|s| format!(" — did you mean `{s}`?"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "unknown principle `{name}`{suggestion} (see `flywheel principles list`)"
                );
            }
        }
    }
}
