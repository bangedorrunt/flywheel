//! `flywheel standing-orders` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum StandingOrdersCmd {
    /// regenerate the global standing orders from the registry + wave-1
    /// criticals. Writes ONLY ~/.config/flywheel/standing-orders.md —
    /// project fences are authored and never touched (layering guarantee).
    Regen,
}

/// ADR-0019 Spec 2 `flywheel standing-orders regen`: regenerate the GLOBAL
/// standing orders (~/.config/flywheel/standing-orders.md) from the
/// principle registry + the wave-1 criticals (numbered lines, one constraint
/// each). Writes ONLY the global path — project fences
/// (<project>/.flywheel/standing-orders.md) are authored and preserved
/// byte-identical (the layering guarantee, tested in standing_orders.rs).
pub(crate) fn cmd_standing_orders(cmd: &StandingOrdersCmd) -> Result<()> {
    match cmd {
        StandingOrdersCmd::Regen => {
            let global = crate::config::config_dir().join("standing-orders.md");
            crate::standing_orders::regen_global(&global)?;
            let lines = crate::standing_orders::global_standing_orders()
                .lines()
                .filter(|l| !l.starts_with('#'))
                .count();
            println!(
                "regenerated {} ({lines} numbered constraints; project fences untouched)",
                global.display()
            );
            Ok(())
        }
    }
}
