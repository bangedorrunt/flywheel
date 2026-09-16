//! `flywheel standing-orders` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum StandingOrdersCmd {
    /// regenerate the global standing orders from the registry + wave-1
    /// criticals. Writes ONLY ~/.config/flywheel/standing-orders.md —
    /// project fences are authored and never touched (layering guarantee).
    Regen,
}
