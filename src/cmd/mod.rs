//! Command-group subcommand types, split from `main.rs` (bd-qbi8).
//! `Commands` (dispatch) stays in `main.rs`; one module per group lives here.

pub(crate) mod artifacts;
pub(crate) mod asset;
pub(crate) mod backend;
pub(crate) mod checkpoint;
pub(crate) mod coalition;
pub(crate) mod config;
pub(crate) mod crew;
pub(crate) mod cron;
#[cfg(feature = "serve")]
pub(crate) mod daemon;
pub(crate) mod doorbell;
pub(crate) mod ledger;
pub(crate) mod loop_mode;
pub(crate) mod mail;
pub(crate) mod model;
pub(crate) mod policy;
pub(crate) mod principles;
pub(crate) mod profile;
pub(crate) mod runs;
pub(crate) mod slot;
pub(crate) mod squad;
pub(crate) mod stage;
pub(crate) mod standing_orders;
pub(crate) mod toron;
pub(crate) mod work;
pub(crate) mod workflow;
pub(crate) mod worktree;
pub(crate) mod xproj;

pub(crate) use artifacts::ArtifactsCmd;
pub(crate) use asset::{AssetCmd, cmd_asset};
pub(crate) use backend::BackendCmd;
pub(crate) use checkpoint::CheckpointCmd;
pub(crate) use coalition::CoalitionCmd;
pub(crate) use config::{ConfigCmd, cmd_config};
pub(crate) use crew::{ControlVerbCmd, CrewCmd};
pub(crate) use cron::CronCmd;
#[cfg(feature = "serve")]
pub(crate) use daemon::DaemonCmd;
pub(crate) use doorbell::DoorbellCmd;
pub(crate) use ledger::LedgerCmd;
pub(crate) use loop_mode::LoopCmd;
pub(crate) use mail::{MailCmd, MailContactCmd, MailLockCmd, MailProductCmd, MailSlotCmd};
pub(crate) use model::ModelCmd;
pub(crate) use policy::{PolicyCmd, cmd_policy};
pub(crate) use principles::{PrinciplesCmd, cmd_principles};
pub(crate) use profile::ProfileCmd;
pub(crate) use runs::RunsCmd;
pub(crate) use slot::SlotCmd;
pub(crate) use squad::SquadCmd;
pub(crate) use stage::StageCmd;
pub(crate) use standing_orders::{StandingOrdersCmd, cmd_standing_orders};
pub(crate) use toron::{ToronCmd, ToronMailCmd};
pub(crate) use work::WorkCmd;
pub(crate) use workflow::WorkflowCmd;
pub(crate) use worktree::WorktreeCmd;
pub(crate) use xproj::XprojCmd;
