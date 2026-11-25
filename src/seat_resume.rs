//! A seat resumed in a new pane keeps its conversation and its wakes.
//!
//! Each harness reopens a session its own way (`harness_registry::SessionResume`).
//! `flywheel resume <pin>` re-binds the seat to the pane it runs in and starts
//! the harness there with the seat's env and the resume flag.

use anyhow::{Result, anyhow, bail};

use crate::harness_registry::{SessionResume, canonical, deprecated_for, session_resume};

/// Recorded ids may carry a harness prefix (`agy:<conversation>`).
fn bare_id(recorded: &str) -> &str {
    recorded.split_once(':').map_or(recorded, |(_, id)| id)
}

/// The argv that reopens a seat's session, from what the registry recorded.
pub fn resume_args(harness: &str, recorded: Option<&str>) -> Result<Vec<String>> {
    let recorded = recorded.map(str::trim).filter(|s| !s.is_empty());
    match session_resume(harness) {
        SessionResume::ChosenId { resume, .. } | SessionResume::RecordedId { resume } => {
            let id = recorded.ok_or_else(|| {
                anyhow!(
                    "resume: this {harness} seat recorded no session id, so it cannot reopen it \
                     (seats spawned before resume support, or an agy seat whose hook never ran)"
                )
            })?;
            Ok(vec![resume.to_string(), bare_id(id).to_string()])
        }
        SessionResume::SeatDirContinue { flag } => Ok(vec![flag.to_string()]),
        SessionResume::Unsupported => {
            bail!("resume: harness {harness} cannot reopen a seat session")
        }
    }
}

/// The full resume argv: the session flag plus what every seat of that harness
/// carries (Claude's plugin dir).
pub fn resume_launch_args(harness: &str, recorded: Option<&str>) -> Result<Vec<String>> {
    let mut args = resume_args(harness, recorded)?;
    args.extend(crate::cmd::spawn::claude_plugin_args(harness));
    Ok(args)
}

/// One line for a harness that has been replaced, or `None`.
pub fn deprecation_notice(harness: &str) -> Option<String> {
    deprecated_for(harness).map(|new| {
        format!(
            "flywheel: harness {} is deprecated; use {new} instead",
            canonical(harness)
        )
    })
}

/// `flywheel resume <pin>`: run inside the pane that should hold the seat.
pub fn cmd_resume(cfg: &crate::Config, pin: &str, dry_run: bool) -> Result<()> {
    let pane = std::env::var("REX_BLOCK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "resume: run this inside the Rex pane that should hold {pin} (REX_BLOCK is unset)"
            )
        })?;
    let (project, seat) = crate::state::find_seat_by_pin(pin)
        .ok_or_else(|| anyhow!("resume: no seat for {pin} in any project registry"))?;
    let harness = seat
        .harness
        .clone()
        .ok_or_else(|| anyhow!("resume: seat {pin} recorded no harness"))?;
    if let Some(line) = deprecation_notice(&harness) {
        eprintln!("{line}");
    }
    let args = resume_launch_args(&harness, seat.harness_session.as_deref())?;
    let mut env = crate::pane::pane_env_for_launch(cfg, seat.mail_identity.as_deref(), None)?;
    let cli = crate::events_tokens::ToronTokenCli {
        bin: std::env::var("TORON_BIN").unwrap_or_else(|_| "toron".to_string()),
    };
    crate::cmd::spawn::prepare_seat_events_before_launch(
        &cli,
        &crate::config::state_dir(),
        &harness,
        &project,
        seat.mail_identity.as_deref(),
        &mut env,
    );
    let command = crate::pane::rex_env_command(&env, &harness, &args);
    if dry_run {
        println!("project: {project}\npane: {pane}\ncommand: {command}");
        return Ok(());
    }
    crate::cmd::swarm::seat_adopt(&project, Some(pin), &pane, None)?;
    exec_shell(&command)
}

#[cfg(unix)]
fn exec_shell(command: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .exec();
    Err(anyhow!("resume: cannot start the harness: {err}"))
}

#[cfg(not(unix))]
fn exec_shell(_command: &str) -> Result<()> {
    bail!("resume: needs a unix shell")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_supported_harness_reopens_its_own_way() {
        assert_eq!(
            resume_args("claude", Some("0b6e3c2a-1f4d-4c55-9a51-2a6d2f0e7c11")).unwrap(),
            ["--resume", "0b6e3c2a-1f4d-4c55-9a51-2a6d2f0e7c11"]
        );
        assert_eq!(
            resume_args("opencode", Some("ses-1-2-3")).unwrap(),
            ["--session", "ses-1-2-3"]
        );
        assert_eq!(resume_args("pi", None).unwrap(), ["--continue"]);
        assert_eq!(
            resume_args("agy", Some("agy:conv-42")).unwrap(),
            ["--conversation", "conv-42"],
            "the hook's harness prefix is stripped"
        );
    }

    #[test]
    fn a_missing_id_or_harness_without_resume_is_a_typed_refusal() {
        assert!(resume_args("claude", None).is_err());
        assert!(resume_args("opencode", Some("  ")).is_err());
        assert!(resume_args("antigravity", None).is_err());
        assert!(resume_args("gemini", Some("x")).is_err());
        assert!(resume_args("codex", Some("x")).is_err());
    }

    #[test]
    fn claude_resume_keeps_the_seat_plugin() {
        let args = resume_launch_args("claude", Some("sid")).unwrap();
        assert_eq!(&args[..2], ["--resume", "sid"]);
        assert!(args.iter().any(|a| a == "--plugin-dir"), "{args:?}");
        assert_eq!(resume_launch_args("pi", None).unwrap(), ["--continue"]);
    }

    #[test]
    fn gemini_is_deprecated_for_antigravity() {
        let line = deprecation_notice("gm").expect("gemini is deprecated");
        assert!(
            line.contains("gemini") && line.contains("antigravity"),
            "{line}"
        );
        assert_eq!(deprecation_notice("agy"), None);
        assert_eq!(deprecation_notice("claude"), None);
    }
}
