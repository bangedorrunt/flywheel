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
pub(crate) mod history;
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
pub(crate) use backend::{BackendCmd, cmd_backend};
pub(crate) use checkpoint::{CheckpointCmd, cmd_checkpoint};
pub(crate) use coalition::CoalitionCmd;
pub(crate) use config::{ConfigCmd, cmd_config};
pub(crate) use crew::{CrewCmd, cmd_crew};
pub(crate) use cron::CronCmd;
#[cfg(feature = "serve")]
pub(crate) use daemon::DaemonCmd;
pub(crate) use doorbell::{
    DoorbellCmd, cmd_doorbell_digest, cmd_doorbell_inject, cmd_doorbell_re_inject,
    cmd_doorbell_roster, cmd_doorbell_start, cmd_doorbell_status, cmd_doorbell_stop,
    cmd_doorbell_subscribe, cmd_internal_doorbell,
};
pub(crate) use history::{
    cmd_analytics, cmd_audit, cmd_changes, cmd_history, cmd_resume, cmd_timeline,
};
pub(crate) use ledger::{LedgerCmd, cmd_ledger};
pub(crate) use loop_mode::LoopCmd;
pub(crate) use mail::{MailCmd, cmd_mail};
pub(crate) use model::{ModelCmd, cmd_models};
pub(crate) use policy::{PolicyCmd, cmd_policy};
pub(crate) use principles::{PrinciplesCmd, cmd_principles};
pub(crate) use profile::ProfileCmd;
pub(crate) use runs::{RunsCmd, cmd_runs};
pub(crate) use slot::{SlotCmd, cmd_slot};
pub(crate) use squad::SquadCmd;
pub(crate) use stage::{StageCmd, cmd_stage};
pub(crate) use standing_orders::{StandingOrdersCmd, cmd_standing_orders};
pub(crate) use toron::{ToronCmd, ToronMailCmd, cmd_toron};
pub(crate) use work::{WorkCmd, cmd_work};
pub(crate) use workflow::WorkflowCmd;
pub(crate) use worktree::{WorktreeCmd, cmd_worktree};
pub(crate) use xproj::{XprojCmd, cmd_xproj};
