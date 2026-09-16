//! `flywheel crew` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

/// ADR-0010 D7 crew plane: workflow preview + lifecycle verbs.
#[derive(Subcommand)]
pub(crate) enum CrewCmd {
    /// Preview the auto-crew a workflow would spawn (crew: auto)
    Preview {
        /// workflow name (built-in or catalog)
        name: String,
    },
    /// Lifecycle verbs for a spawned agent pane. NO arbitrary-text entry
    /// point: interrupt|exit|relaunch are the whole surface (ADR-0010 D7).
    Control {
        project: String,
        /// the pane target (herdr agent name, kind short, or pane id)
        pin: String,
        #[command(subcommand)]
        verb: ControlVerbCmd,
    },
}

/// The three control verbs (postconditions asserted in src/control.rs).
#[derive(Subcommand)]
pub(crate) enum ControlVerbCmd {
    /// Cancel the current tool call (Escape only — kill-style keys stay off
    /// this plane). Confirmed only when the composer is provably empty.
    Interrupt,
    /// Stop the agent pane. Reservations + dirty files preserved.
    Exit,
    /// Journaled relaunch: checkpoint the spawn record, stop, launch the
    /// same pin with an incarnation token; rollback on launch failure.
    Relaunch,
}
