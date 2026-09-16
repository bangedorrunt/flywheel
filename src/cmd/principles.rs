//! `flywheel principles` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum PrinciplesCmd {
    /// print the 21 canonical principle names (one per line)
    List,
    /// print a principle's full when+rule (unknown name -> non-zero exit +
    /// a suggestion)
    Show { name: String },
}
